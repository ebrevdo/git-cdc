//! Git plumbing: running git, resolving repo paths/config, and walking
//! history/index for manifests.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Git, Stdio};

use anyhow::{Context, Result, bail, ensure};
use git_cdc_core::chunker::ChunkParams;
use git_cdc_core::manifest::Manifest;
use git_cdc_core::store::DiskStore;

use crate::manifest::{MAX_MANIFEST_SIZE, detect};

pub fn git_out(args: &[&str]) -> Result<String> {
    let out = Git::new("git").args(args).output().context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

pub fn repo_root() -> Result<PathBuf> {
    Ok(PathBuf::from(git_out(&["rev-parse", "--show-toplevel"])?))
}

pub fn git_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(git_out(&[
        "rev-parse",
        "--absolute-git-dir",
    ])?))
}

pub fn local_store() -> Result<DiskStore> {
    Ok(DiskStore::new(git_dir()?.join("cdc").join("objects")))
}

/// Explicit pointer-only checkout policy shared by both CDC filters.
/// Transfer failures remain errors when hydration is enabled.
pub fn auto_hydrate() -> Result<bool> {
    let out = Git::new("git")
        .args(["config", "--type=bool", "--get", "cdc.hydrate"])
        .output()?;
    if out.status.code() == Some(1) {
        return Ok(true);
    }
    if !out.status.success() {
        bail!(
            "cannot read cdc.hydrate: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim() == "true")
}

/// `cdc.chunk.{min,avg,max}` from git config, defaults where unset.
/// `--type=int` expands git's k/m/g suffixes ("512k" → 524288).
pub fn chunk_params() -> Result<ChunkParams> {
    let get = |key: &str, default: u32| -> Result<u32> {
        if git_out(&["config", "--get", key]).is_err() {
            return Ok(default); // unset — a malformed value must NOT land here
        }
        let v = git_out(&["config", "--type=int", "--get", key])?;
        v.parse::<u64>()
            .ok()
            .and_then(|v| u32::try_from(v).ok())
            .with_context(|| format!("{key} = {v:?} is not a valid size"))
    };
    use git_cdc_core::chunker::{AVG_SIZE, MAX_SIZE, MIN_SIZE};
    ChunkParams {
        min: get("cdc.chunk.min", MIN_SIZE)?,
        avg: get("cdc.chunk.avg", AVG_SIZE)?,
        max: get("cdc.chunk.max", MAX_SIZE)?,
    }
    .validate()
}

/// Per-path lookups before a filter process switches to an index snapshot.
const PER_PATH_LOOKUPS: usize = 64;

/// Resolves the chunk parameters `clean` should use for a given path.
///
/// A file already tracked by git-cdc is re-chunked with the parameters its
/// *committed* manifest was made with, not whatever this clone has in
/// `cdc.chunk.*`. Boundaries are only reproducible for a fixed parameter set,
/// so without this a differently-configured clone silently rewrites manifests
/// for files nobody edited and uploads a second copy of content the remote
/// already has. Untracked (new) files, and repos with `cdc.chunk.inherit`
/// set false, use the local config.
///
/// The first lookups query each path; a filter process that keeps going then
/// snapshots the active index once, so later new paths need no Git request.
/// Long-lived helpers check object sizes before reading candidate recipes;
/// converting a large raw Git blob must not read its old contents as well.
pub struct ChunkParamsFor {
    local: ChunkParams,
    inherit: bool,
    snapshot_index: bool,
    lookups: usize,
    index: Option<HashMap<Vec<u8>, String>>,
    cached: HashMap<String, Option<ChunkParams>>,
    check: Option<CatFile>,
    batch: Option<CatFile>,
}

struct CatFile {
    child: std::process::Child,
    stdin: BufWriter<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
}

impl ChunkParamsFor {
    pub fn new(local: ChunkParams) -> ChunkParamsFor {
        // Opt-out for anyone deliberately re-chunking a repo to new settings.
        let inherit = git_out(&["config", "--type=bool", "--get", "cdc.chunk.inherit"])
            .map(|v| v != "false")
            .unwrap_or(true);
        ChunkParamsFor {
            local,
            inherit,
            snapshot_index: false,
            lookups: 0,
            index: None,
            cached: HashMap::new(),
            check: None,
            batch: None,
        }
    }

    pub fn for_process(local: ChunkParams) -> Self {
        let mut params = Self::new(local);
        params.snapshot_index = true;
        params
    }

    /// `None` path (the one-shot `clean` on an old install, whose filter
    /// config predates `%f`) falls back to the local config.
    pub fn get(&mut self, path: Option<&str>) -> ChunkParams {
        if !self.inherit {
            return self.local;
        }
        let Some(path) = path.filter(|p| !p.is_empty()) else {
            return self.local;
        };
        self.committed(path).unwrap_or(self.local)
    }

    fn committed(&mut self, path: &str) -> Option<ChunkParams> {
        let oid = if self.snapshot_index && self.lookups >= PER_PATH_LOOKUPS {
            let index = self
                .index
                .get_or_insert_with(|| index_oids().unwrap_or_default());
            index.get(path.as_bytes())?.clone()
        } else {
            // Small operations (and one-shot filters) look up each path, so
            // adding a few files to a large repo never lists its whole index.
            self.lookups += 1;
            if path.contains('\n') {
                git_out(&["rev-parse", "--verify", &format!(":{path}")]).ok()?
            } else {
                format!(":{path}")
            }
        };
        if let Some(params) = self.cached.get(&oid) {
            return *params;
        }
        let params = match self.read_params(&oid) {
            Ok(params) => params,
            Err(_) => {
                // A truncated response must not desynchronize later lookups.
                if let Some(batch) = self.batch.take() {
                    batch.close();
                }
                if let Some(check) = self.check.take() {
                    check.close();
                }
                None
            }
        };
        // Bound this cache independently of repository size. Identical recipes
        // share one lookup, without retaining every parsed manifest in memory.
        if self.cached.len() >= 4096 {
            self.cached.clear();
        }
        self.cached.insert(oid, params);
        params
    }

    fn read_params(&mut self, oid: &str) -> Result<Option<ChunkParams>> {
        if self.check.is_none() {
            self.check = Some(CatFile::spawn("--batch-check")?);
        }
        let header = self.check.as_mut().unwrap().request(oid)?;
        let mut parts = header.split_whitespace();
        let (name, typ, size) = (parts.next(), parts.next(), parts.next());
        if typ != Some("blob") {
            return Ok(None);
        }
        let oid = name.context("missing blob ID")?;
        let size: u64 = size.context("missing blob size")?.parse()?;
        if size > MAX_MANIFEST_SIZE {
            return Ok(None);
        }
        if self.batch.is_none() {
            self.batch = Some(CatFile::spawn("--batch")?);
        }
        let batch = self.batch.as_mut().unwrap();
        writeln!(batch.stdin, "{oid}")?;
        batch.stdin.flush()?;
        let body = read_blob(&mut batch.stdout, oid, size)?;
        let Some(m) = detect(&body) else {
            return Ok(None);
        };
        // A manifest committed with values fastcdc rejects must not be
        // inherited — chunking on them is undefined in a release build.
        Ok(ChunkParams {
            min: m.chunk_min,
            avg: m.chunk_avg,
            max: m.chunk_max,
        }
        .validate()
        .ok())
    }
}

impl Drop for ChunkParamsFor {
    fn drop(&mut self) {
        if let Some(check) = self.check.take() {
            check.close();
        }
        if let Some(batch) = self.batch.take() {
            batch.close();
        }
    }
}

/// NUL framing handles tabs/newlines in names, and --full-name works when
/// the filter runs below the worktree root. Git honors GIT_INDEX_FILE here.
fn index_oids() -> Result<HashMap<Vec<u8>, String>> {
    let mut child = Git::new("git")
        .args(["ls-files", "--stage", "--full-name", "-z", "--", ":/"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut result = HashMap::new();
    let parsed = (|| -> Result<()> {
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut entry = Vec::new();
        while reader.read_until(0, &mut entry)? != 0 {
            ensure!(entry.pop() == Some(0), "incomplete index entry");
            let tab = entry
                .iter()
                .position(|&b| b == b'\t')
                .context("invalid index entry")?;
            let metadata = std::str::from_utf8(&entry[..tab])?;
            let mut fields = metadata.split_whitespace();
            let (_mode, oid, stage) = (fields.next(), fields.next(), fields.next());
            if let (Some(oid), Some("0")) = (oid, stage) {
                result.insert(entry[tab + 1..].to_vec(), oid.to_owned());
            }
            entry.clear();
        }
        Ok(())
    })();
    let status = child.wait()?;
    parsed?;
    ensure!(status.success(), "cannot read active Git index");
    Ok(result)
}

pub fn oid_str(hash: &blake3::Hash) -> String {
    format!("blake3:{}", hash.to_hex())
}

/// Every legacy manifest blob across history. Walk object IDs with rev-list,
/// then inspect bounded blobs using cat-file. Recipe signatures identify
/// historical and renamed files without relying on current attributes.
pub fn all_manifests() -> Result<Vec<Manifest>> {
    let mut manifests = Vec::new();
    ManifestScan::new()?.visit(&["--all".into()], |manifest| {
        if manifest.extra.get("storage").map(String::as_str) != Some("git-lfs-v1") {
            manifests.push(manifest.clone());
        }
        Ok(())
    })?;
    Ok(manifests)
}

impl CatFile {
    fn spawn(mode: &str) -> Result<Self> {
        let mut child = Git::new("git")
            .args(["cat-file", mode])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self {
            stdin: BufWriter::new(child.stdin.take().unwrap()),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
        })
    }

    fn request(&mut self, oid: &str) -> Result<String> {
        writeln!(self.stdin, "{oid}")?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        Ok(header)
    }

    fn close(self) {
        let Self {
            mut child,
            stdin,
            stdout,
        } = self;
        drop(stdin);
        drop(stdout);
        let _ = child.wait();
    }
}

fn read_blob(output: &mut impl BufRead, oid: &str, size: u64) -> Result<Vec<u8>> {
    let mut header = String::new();
    output.read_line(&mut header)?;
    ensure!(
        header == format!("{oid} blob {size}\n"),
        "Git object changed during scan"
    );
    let mut body = vec![0; size as usize + 1];
    output.read_exact(&mut body)?;
    ensure!(body.pop() == Some(b'\n'), "invalid cat-file response");
    Ok(body)
}

/// Check the entire object list at once, then stream only bounded blobs.
/// File-backed input lets Git read ahead without filling a request pipe.
fn visit_blobs<'a>(
    root: &Path,
    oids: &[&'a str],
    mut visit: impl FnMut(&'a str, Vec<u8>) -> Result<()>,
) -> Result<()> {
    if oids.is_empty() {
        return Ok(());
    }
    let mut input = Vec::new();
    for oid in oids {
        writeln!(input, "{oid}")?;
    }
    let metadata = String::from_utf8(git_with_input(
        root,
        &["cat-file", "--batch-check"],
        Some(&input),
    )?)?;
    let headers: Vec<_> = metadata.lines().collect();
    ensure!(headers.len() == oids.len(), "incomplete cat-file response");
    let mut bounded = Vec::new();
    input.clear();
    for (oid, header) in oids.iter().copied().zip(headers) {
        let fields: Vec<_> = header.split_whitespace().collect();
        ensure!(
            fields.len() == 3 && fields[0] == oid,
            "unreadable Git object {oid}: {header}"
        );
        let size: u64 = fields[2].parse()?;
        if fields[1] == "blob" && size <= MAX_MANIFEST_SIZE {
            writeln!(input, "{oid}")?;
            bounded.push((oid, size));
        }
    }
    if bounded.is_empty() {
        return Ok(());
    }
    with_blob_batch(root, &input, "Git history blob scan failed", |output| {
        for (oid, size) in bounded {
            visit(oid, read_blob(output, oid, size)?)?;
        }
        Ok(())
    })
}

/// Close the stream before waiting, and stop Git if parsing or visiting fails.
fn with_blob_batch(
    root: &Path,
    input: &[u8],
    failure: &str,
    read: impl FnOnce(&mut BufReader<std::process::ChildStdout>) -> Result<()>,
) -> Result<()> {
    let mut cat = Git::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .stdin(input_file(input)?)
        .stdout(Stdio::piped())
        .spawn()?;
    let mut output = BufReader::new(cat.stdout.take().unwrap());
    let result = read(&mut output);
    drop(output);
    if result.is_err() {
        let _ = cat.kill();
    }
    let status = cat.wait()?;
    result?;
    ensure!(status.success(), "{failure}");
    Ok(())
}

/// Different reachability sets share object reads and recipe parsing. Prune
/// keeps its separate protection rules without reading the same blobs again.
pub(crate) struct ManifestScan {
    root: PathBuf,
    seen: HashSet<String>,
    manifests: HashMap<String, Manifest>,
}

impl ManifestScan {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            root: std::env::current_dir()?,
            seen: HashSet::new(),
            manifests: HashMap::new(),
        })
    }

    pub(crate) fn visit(
        &mut self,
        revisions: &[String],
        visit: impl FnMut(&Manifest) -> Result<()>,
    ) -> Result<()> {
        self.visit_revisions(revisions, &[], &BTreeSet::new(), visit)
    }

    pub(crate) fn visit_tips(
        &mut self,
        tips: &[String],
        excluded: &BTreeSet<String>,
        visit: impl FnMut(&Manifest) -> Result<()>,
    ) -> Result<()> {
        if tips.is_empty() {
            return Ok(());
        }
        self.visit_revisions(&[], tips, excluded, visit)
    }

    fn visit_revisions(
        &mut self,
        revisions: &[String],
        tips: &[String],
        excluded: &BTreeSet<String>,
        mut visit: impl FnMut(&Manifest) -> Result<()>,
    ) -> Result<()> {
        let mut args = vec!["rev-list", "--objects", "--no-object-names"];
        args.extend(revisions.iter().map(String::as_str));
        args.push("--stdin");
        let mut input = Vec::new();
        for oid in tips {
            writeln!(input, "{oid}")?;
        }
        for oid in excluded {
            writeln!(input, "^{oid}")?;
        }
        let list = String::from_utf8(git_with_input(&self.root, &args, Some(&input))?)?;
        let unseen: Vec<_> = list
            .lines()
            .filter(|oid| !oid.is_empty())
            .filter(|oid| self.seen.insert((*oid).to_owned()))
            .collect();
        visit_blobs(&self.root, &unseen, |oid, body| {
            if let Some(manifest) = detect(&body) {
                self.manifests.insert(oid.to_owned(), manifest);
            }
            Ok(())
        })?;
        for oid in list.lines() {
            if let Some(manifest) = self.manifests.get(oid) {
                visit(manifest)?;
            }
        }
        Ok(())
    }
}

/// HEAD and staged recipes for the legacy no-argument diff. Attribute and
/// object lookups use NUL records so unusual filenames remain literal.
pub fn diff_manifests() -> Result<Vec<(String, Manifest, Option<Manifest>)>> {
    let root = repo_root()?;
    let mut indexed = Vec::new();
    visit_index_blobs(
        &root,
        false,
        |filter| filter == b"cdc",
        |entry| {
            if entry.stage == 0
                && let Some(manifest) = detect(&entry.blob)
            {
                indexed.push((entry.path, manifest));
            }
            Ok(())
        },
    )?;
    let tree = Git::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-tree", "-r", "--full-tree", "-z", "HEAD"])
        .output()?;
    let mut head_oids = HashMap::new();
    if tree.status.success() {
        for record in tree.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .context("invalid tree entry")?;
            let metadata = std::str::from_utf8(&record[..tab])?;
            let oid = metadata
                .split_whitespace()
                .nth(2)
                .context("invalid tree metadata")?;
            head_oids.insert(&record[tab + 1..], oid);
        }
    }
    let oids: BTreeSet<_> = indexed
        .iter()
        .filter_map(|(path, _)| head_oids.get(path.as_bytes()).copied())
        .collect();
    let mut head_manifests = HashMap::new();
    visit_blobs(&root, &oids.into_iter().collect::<Vec<_>>(), |oid, body| {
        if let Some(manifest) = detect(&body) {
            head_manifests.insert(oid, manifest);
        }
        Ok(())
    })?;
    Ok(indexed
        .into_iter()
        .map(|(path, manifest)| {
            let previous = head_oids
                .get(path.as_bytes())
                .and_then(|oid| head_manifests.get(oid))
                .cloned();
            (path, manifest, previous)
        })
        .collect())
}

pub(crate) struct IndexBlob {
    pub path: String,
    pub mode: String,
    pub blob_oid: String,
    pub stage: u8,
    pub filter: Vec<u8>,
    pub blob: Vec<u8>,
}

struct IndexEntry<'a> {
    path: &'a str,
    mode: &'a str,
    oid: &'a str,
    stage: u8,
}

pub(crate) fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_with_input(root, args, None)
}

pub(crate) fn input_file(bytes: &[u8]) -> Result<File> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(file)
}

pub(crate) fn git_with_input(root: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut command = Git::new("git");
    command.arg("-C").arg(root).args(args);
    if let Some(input) = input {
        command.stdin(input_file(input)?);
    }
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

/// NUL-delimited index/attribute records preserve whitespace and newlines in
/// paths. Keep the original blob and mode, including staged snapshots.
pub(crate) fn visit_index_blobs(
    root: &Path,
    cached_attributes: bool,
    matches: impl Fn(&[u8]) -> bool,
    mut visit: impl FnMut(IndexBlob) -> Result<()>,
) -> Result<()> {
    let listed = git_bytes(root, &["ls-files", "--stage", "--full-name", "-z"])?;
    let mut entries = Vec::new();
    let mut paths = Vec::new();
    for entry in listed.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid index entry")?;
        let (meta, path) = entry.split_at(tab);
        let path = std::str::from_utf8(&path[1..]).context("non-UTF-8 CDC path")?;
        let fields: Vec<_> = std::str::from_utf8(meta)?.split_whitespace().collect();
        let [mode, oid, stage] = fields.as_slice() else {
            bail!("invalid index metadata");
        };
        if !matches!(*mode, "100644" | "100755") {
            continue;
        }
        entries.push(IndexEntry {
            path,
            mode,
            oid,
            stage: stage.parse()?,
        });
        paths.extend_from_slice(path.as_bytes());
        paths.push(0);
    }
    if entries.is_empty() {
        return Ok(());
    }
    let mut args = vec!["check-attr", "--stdin", "-z", "filter"];
    if cached_attributes {
        args.push("--cached");
    }
    let attributes = git_with_input(root, &args, Some(&paths))?;
    let fields: Vec<_> = attributes
        .strip_suffix(b"\0")
        .context("unterminated attribute response")?
        .split(|b| *b == 0)
        .collect();
    ensure!(
        fields.len() == entries.len() * 3,
        "invalid attribute response"
    );
    let (attributes, _) = fields.as_chunks::<3>();
    let mut tracked = Vec::new();
    let mut oids = Vec::new();
    for (entry, &[path, name, filter]) in entries.into_iter().zip(attributes) {
        ensure!(
            path == entry.path.as_bytes() && name == b"filter",
            "invalid attribute response"
        );
        if !matches(filter) {
            continue;
        }
        writeln!(oids, "{}", entry.oid)?;
        tracked.push((entry, filter));
    }
    if tracked.is_empty() {
        return Ok(());
    }
    let metadata = String::from_utf8(git_with_input(
        root,
        &["cat-file", "--batch-check"],
        Some(&oids),
    )?)?;
    let headers: Vec<_> = metadata.lines().collect();
    ensure!(
        headers.len() == tracked.len(),
        "incomplete cat-file response"
    );
    let mut bounded = Vec::new();
    oids.clear();
    for ((entry, filter), header) in tracked.into_iter().zip(headers) {
        let fields: Vec<_> = header.split_whitespace().collect();
        ensure!(
            fields.len() == 3 && fields[0] == entry.oid && fields[1] == "blob",
            "invalid indexed blob: {header}"
        );
        let size = fields[2].parse::<u64>()?;
        if size > MAX_MANIFEST_SIZE {
            continue;
        }
        writeln!(oids, "{}", entry.oid)?;
        bounded.push((entry, filter, size));
    }
    if bounded.is_empty() {
        return Ok(());
    }
    with_blob_batch(root, &oids, "Git index blob scan failed", |output| {
        for (entry, filter, size) in bounded {
            let blob = read_blob(output, entry.oid, size)?;
            visit(IndexBlob {
                path: entry.path.to_owned(),
                mode: entry.mode.to_owned(),
                blob_oid: entry.oid.to_owned(),
                stage: entry.stage,
                blob,
                filter: filter.to_vec(),
            })?;
        }
        Ok(())
    })
}
