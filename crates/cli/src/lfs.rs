//! Standard Git LFS transport. Git stores a self-contained recipe per file;
//! raw FastCDC chunks are ordinary SHA-256 LFS objects. Nothing writes the
//! index, a ref, or a tracked sidecar as a filter side effect.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use git_cdc_core::chunker::{Chunk, ChunkParams, chunk_stream};
use git_cdc_core::manifest::{Manifest, is_manifest};
use sha2::{Digest, Sha256};

use crate::git::{ChunkParamsFor, chunk_params, git_out, repo_root};
use crate::manifest::{MAX_MANIFEST_SIZE, detect, read_candidate};

pub(crate) mod cache;
mod context;
mod history;
mod hooks;
mod push;
mod retention;
mod transfer;

pub use hooks::{dispatch_hook, install};

const STORAGE: &str = "git-lfs-v1";
const MAX_CHUNKS: usize = 65_536;
const LFS_POINTER_VERSION: &str = "version https://git-lfs.github.com/spec/v1";
/// Bind Git LFS's named remote to this pre-push invocation's destination,
/// retaining remote-specific LFS settings through command-local overrides.
struct PushRemote {
    name: String,
    destination: String,
}

impl PushRemote {
    fn new(remote: &str, url: &str) -> Self {
        let temporary = git_out(&["config", "--get", &format!("remote.{remote}.url")]).is_err();
        Self {
            name: if temporary { "cdc-lfs-target" } else { remote }.into(),
            destination: url.into(),
        }
    }

    fn apply(&self, cmd: &mut Command) {
        // Git LFS uses the last configured value. Override both directions so
        // recovery also uses this destination, even with multiple push URLs.
        for key in ["url", "pushurl"] {
            cmd.arg("-c")
                .arg(format!("remote.{}.{key}={}", self.name, self.destination));
        }
        cmd.env("GIT_NO_REPLACE_OBJECTS", "1");
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        self.apply(&mut cmd);
        cmd
    }
}

fn sha256(data: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    Sha256::digest(data)
        .iter()
        .flat_map(|b| {
            [
                HEX[(b >> 4) as usize] as char,
                HEX[(b & 15) as usize] as char,
            ]
        })
        .collect()
}

fn lfs_key(c: &Chunk) -> String {
    format!("lfs.{}", c.hash.to_hex())
}

fn lfs_pointer(oid: &str, size: u64) -> String {
    format!("{LFS_POINTER_VERSION}\noid sha256:{oid}\nsize {size}\n")
}

fn is_sha256(oid: &str) -> bool {
    oid.len() == 64
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_git_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Publication must inspect the same original objects that Git sends.
fn git_without_replacements() -> Command {
    let mut command = Command::new("git");
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command
}

fn trace(phase: &str, started: std::time::Instant, detail: impl std::fmt::Display) {
    if std::env::var_os("GIT_CDC_TRACE").is_some_and(|value| value != "0") {
        eprintln!(
            "git-cdc: {phase}: {detail}; {} ms",
            started.elapsed().as_millis()
        );
    }
}

fn chunk_oid<'a>(m: &'a Manifest, c: &Chunk) -> Result<&'a str> {
    let oid = m
        .extra
        .get(&lfs_key(c))
        .context("missing LFS chunk reference")?;
    ensure!(is_sha256(oid), "invalid SHA-256 LFS chunk reference");
    Ok(oid)
}

pub(crate) fn validate_manifest(m: Manifest) -> Result<Manifest> {
    ensure!(
        m.extra.get("storage").map(String::as_str) == Some(STORAGE),
        "not a standard-LFS git-cdc manifest; use its original filter/backend"
    );
    let params = ChunkParams {
        min: m.chunk_min,
        avg: m.chunk_avg,
        max: m.chunk_max,
    }
    .validate()?;
    ensure!(
        m.chunks.len() <= MAX_CHUNKS,
        "too many chunks in LFS manifest"
    );
    let mut offset = 0u64;
    for c in &m.chunks {
        ensure!(
            c.offset == offset && c.length > 0 && c.length <= params.max,
            "invalid chunk offset or length in LFS manifest"
        );
        offset = offset
            .checked_add(c.length as u64)
            .context("file size overflow")?;
        chunk_oid(&m, c)?;
    }
    ensure!(offset == m.size, "LFS manifest size mismatch");
    Ok(m)
}

/// Separate local LFS cache: stock `git lfs prune` cannot discover references
/// inside CDC recipes, so it must not sweep these objects. This is still the
/// standard LFS cache layout and transport, shared by linked worktrees.
pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn new() -> Result<Self> {
        let common = PathBuf::from(git_out(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])?);
        Ok(Self {
            root: common.join("cdc").join("lfs"),
        })
    }

    fn path(&self, oid: &str) -> PathBuf {
        self.root
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(oid)
    }

    fn lease(&self, oid: &str, exclusive: bool) -> Result<File> {
        let directory = self.root.join("locks");
        fs::create_dir_all(&directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(oid))?;
        if exclusive {
            file.lock()?;
        } else {
            file.lock_shared()?;
        }
        Ok(file)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        // Relative to Git's common directory. An absolute override also
        // reaches the standalone-file helper and can make the remote use
        // the source cache as its destination.
        cmd.args(["-c", "lfs.storage=cdc/lfs"]);
        // A caller's skip/include/allow-incomplete settings must not turn a
        // failed required transfer into a successful checkout or publication.
        cmd.args([
            "-c",
            "lfs.skipdownloaderrors=false",
            "-c",
            "lfs.allowincompletepush=false",
            "-c",
            "lfs.fetchinclude=",
            "-c",
            "lfs.fetchexclude=",
        ]);
        cmd.env_remove("GIT_LFS_SKIP_SMUDGE")
            .env_remove("GIT_LFS_SKIP_DOWNLOAD_ERRORS");
        cmd
    }

    fn put(&self, bytes: &[u8]) -> Result<String> {
        let oid = sha256(bytes);
        let _lease = self.lease(&oid, true)?;
        let path = self.path(&oid);
        if path.is_file() {
            // Do not trust an existing name: a previous disk error must be
            // detected. A clean with known-good source bytes may repair it.
            let mut old = Vec::with_capacity(bytes.len());
            File::open(&path)?
                .take(bytes.len() as u64 + 1)
                .read_to_end(&mut old)?;
            if old == bytes {
                return Ok(oid);
            }
        }
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path).context("atomically storing LFS chunk")?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(oid)
    }

    fn get(&self, oid: &str, c: &Chunk) -> Result<Vec<u8>> {
        let _lease = self.lease(oid, false)?;
        let bytes = self.get_object_leased(oid, c.length as u64, None)?;
        ensure!(blake3::hash(&bytes) == c.hash, "corrupt LFS chunk {oid}");
        Ok(bytes)
    }

    fn get_object_leased(
        &self,
        oid: &str,
        size: u64,
        remote: Option<&PushRemote>,
    ) -> Result<Vec<u8>> {
        let path = self.path(oid);
        if !path.is_file() {
            self.download(oid, size, remote)?;
        }
        self.read_cached_leased(oid, size)
    }

    fn read_cached_leased(&self, oid: &str, size: u64) -> Result<Vec<u8>> {
        let path = self.path(oid);
        let file = File::open(&path)
            .with_context(|| format!("LFS chunk {oid} is missing after download"))?;
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(size + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == size && sha256(&bytes) == oid,
            "corrupt LFS chunk {oid}"
        );
        Ok(bytes)
    }

    fn download(&self, oid: &str, size: u64, remote: Option<&PushRemote>) -> Result<()> {
        let path = self.path(oid);
        let pointer = lfs_pointer(oid, size);
        // Recover evicted history from the normal download remote first,
        // then the push target. A new backup remote may not have it yet.
        let candidates = std::iter::once(None).chain(remote.map(Some));
        for candidate in candidates {
            let mut cmd = self.command();
            if let Some(remote) = candidate {
                remote.apply(&mut cmd);
                cmd.arg("-c")
                    .arg(format!("remote.lfsdefault={}", remote.name));
                // Git LFS prefers branch tracking over remote.lfsdefault.
                // Override it only for this fallback download invocation.
                if let Ok(reference) = git_out(&["symbolic-ref", "--quiet", "HEAD"])
                    && let Some(branch) = reference.strip_prefix("refs/heads/")
                {
                    cmd.arg("-c")
                        .arg(format!("branch.{branch}.remote={}", remote.name));
                }
                cmd.args(["-c", "lfs.remote.autodetect=false"]);
            }
            let mut child = cmd
                .args(["lfs", "smudge"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .context("running git lfs smudge (install Git LFS 3.x)")?;
            let sent = child.stdin.take().unwrap().write_all(pointer.as_bytes());
            let status = child.wait()?;
            sent?;
            if status.success() && path.is_file() {
                return Ok(());
            }
        }
        bail!("LFS download failed for {oid}")
    }
}

pub(crate) fn clean(
    reader: impl Read,
    mut writer: impl Write,
    store: &Store,
    params: ChunkParams,
) -> Result<()> {
    let mut reader = BufReader::with_capacity(64, reader);
    let (prefix, manifest) = read_candidate(&mut reader, true)?;
    if manifest.is_some() {
        writer.write_all(&prefix)?;
        return Ok(());
    }
    let mut refs = BTreeMap::new();
    let mut count = 0usize;
    let (chunks, oid, size) = chunk_stream(prefix.as_slice().chain(reader), params, |c, bytes| {
        count += 1;
        ensure!(
            count <= MAX_CHUNKS,
            "file exceeds the {MAX_CHUNKS}-chunk manifest limit"
        );
        refs.insert(lfs_key(c), store.put(bytes)?);
        Ok(())
    })?;
    refs.insert("storage".into(), STORAGE.into());
    let mut m = Manifest::new(oid, size, chunks, params);
    m.extra = refs;
    let encoded = m.encode();
    ensure!(
        encoded.len() as u64 <= MAX_MANIFEST_SIZE,
        "LFS manifest exceeds 16 MiB"
    );
    writer.write_all(encoded.as_bytes())?;
    Ok(())
}

fn reassemble(m: &Manifest, store: &Store, mut writer: impl Write) -> Result<()> {
    let mut hasher = blake3::Hasher::new();
    for c in &m.chunks {
        let bytes = store.get(chunk_oid(m, c)?, c)?;
        hasher.update(&bytes);
        writer.write_all(&bytes)?;
    }
    ensure!(
        hasher.finalize() == m.oid,
        "reassembled file does not match manifest oid"
    );
    Ok(())
}

pub(crate) fn smudge(reader: impl Read, mut writer: impl Write, store: &Store) -> Result<()> {
    let mut reader = BufReader::with_capacity(64, reader);
    let (prefix, manifest) = read_candidate(&mut reader, true)?;
    let Some(m) = manifest else {
        writer.write_all(&prefix)?;
        io::copy(&mut reader, &mut writer)?;
        return Ok(());
    };
    // Callers spool this result until the whole file has passed verification.
    reassemble(&m, store, &mut writer)
}

pub fn cmd_clean(path: Option<&str>) -> Result<()> {
    let mut params = ChunkParamsFor::new(chunk_params()?);
    clean(
        io::stdin().lock(),
        io::stdout().lock(),
        &Store::new()?,
        params.get(path),
    )
}

pub fn cmd_smudge() -> Result<()> {
    if !crate::git::auto_hydrate()? {
        io::copy(&mut io::stdin().lock(), &mut io::stdout().lock())?;
        return Ok(());
    }
    let mut verified = tempfile::tempfile()?;
    smudge(io::stdin().lock(), &mut verified, &Store::new()?)?;
    verified.rewind()?;
    io::copy(&mut verified, &mut io::stdout().lock())?;
    Ok(())
}

pub fn track(patterns: &[String]) -> Result<()> {
    ensure!(
        !patterns.is_empty(),
        "usage: git cdc track [--lfs] <pattern>..."
    );
    let path = repo_root()?.join(".gitattributes");
    let mut content = fs::read_to_string(&path).unwrap_or_default();
    for pattern in patterns {
        ensure!(!pattern.is_empty(), "empty attribute pattern");
        let quoted = pattern
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        let line = format!("\"{quoted}\" filter=cdc-lfs diff=cdc-lfs -merge -text");
        if !content.lines().any(|l| l == line) {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&line);
            content.push('\n');
        }
    }
    fs::write(path, content)?;
    Ok(())
}

/// Read blobs reachable from the actual pushed tips, including deleted files,
/// detached commits and tag targets. No index or worktree scan is involved.
fn visit_manifests(
    tips: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
    mut visit: impl FnMut(Manifest) -> Result<()>,
) -> Result<()> {
    visit_blobs_with(
        tips,
        excluded,
        Some(MAX_MANIFEST_SIZE),
        None,
        is_manifest,
        |body| {
            if let Some(m) = detect(body)
                && m.extra.contains_key("storage")
            {
                // A parsed recipe with broken references must block publication:
                // skipping it could leave pushed history without its chunks.
                visit(validate_manifest(m)?)?;
            }
            Ok(())
        },
        git_without_replacements,
    )
}

fn visit_blobs_with(
    tips: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
    max_blob_size: Option<u64>,
    paths: Option<&BTreeSet<String>>,
    matches: impl Fn(&[u8]) -> bool,
    mut visit: impl FnMut(&[u8]) -> Result<()>,
    mut command: impl FnMut() -> Command,
) -> Result<()> {
    let mut revisions = tempfile::tempfile()?;
    for tip in tips {
        writeln!(revisions, "{tip}")?;
    }
    for tip in excluded {
        writeln!(revisions, "^{tip}")?;
    }
    revisions.rewind()?;
    let mut objects = BufWriter::new(tempfile::tempfile()?);
    let mut walk_command = command();
    walk_command.args(["rev-list", "--objects", "--stdin"]);
    if paths.is_none() {
        walk_command.arg("--no-object-names");
    }
    with_git_output(
        walk_command
            .stdin(Stdio::from(revisions))
            .stderr(Stdio::inherit()),
        "Git history walk failed",
        |output| {
            for line in output.lines() {
                let line = line?;
                let (oid, path) = line.split_once(' ').unwrap_or((&line, ""));
                if paths.is_none_or(|paths| paths.contains(path)) {
                    writeln!(objects, "{oid}")?;
                }
            }
            Ok(())
        },
    )?;

    // Queue requests in files so Git can read ahead without either side
    // filling a pipe. Each stage streams its output with bounded memory.
    let mut objects = objects.into_inner()?;
    objects.rewind()?;
    let mut blobs = BufWriter::new(tempfile::tempfile()?);
    with_git_output(
        command()
            .args(["cat-file", "--batch-check"])
            .stdin(Stdio::from(objects)),
        "Git object inspection failed",
        |output| {
            for header in output.lines() {
                let header = header?;
                let fields: Vec<_> = header.split_whitespace().collect();
                ensure!(
                    fields.len() == 3 && is_git_oid(fields[0]),
                    "unreadable Git object: {header}"
                );
                let size: u64 = fields[2].parse()?;
                if fields[1] != "blob" || max_blob_size.is_some_and(|limit| size > limit) {
                    continue;
                }
                writeln!(blobs, "{}", fields[0])?;
            }
            Ok(())
        },
    )?;

    let mut blobs = blobs.into_inner()?;
    blobs.rewind()?;
    with_git_output(
        command()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::from(blobs)),
        "Git history blob scan failed",
        |output| {
            loop {
                let mut header = String::new();
                if output.read_line(&mut header)? == 0 {
                    break;
                }
                let fields: Vec<_> = header.split_whitespace().collect();
                ensure!(
                    fields.len() == 3 && is_git_oid(fields[0]) && fields[1] == "blob",
                    "unreadable Git blob: {header}"
                );
                let size: u64 = fields[2].parse()?;
                ensure!(
                    max_blob_size.is_none_or(|limit| size <= limit),
                    "Git object changed during scan"
                );
                let mut body = Vec::new();
                output.by_ref().take(size.min(64)).read_to_end(&mut body)?;
                let remaining = size - body.len() as u64;
                let matched = matches(&body);
                if matched {
                    ensure!(
                        size <= MAX_MANIFEST_SIZE,
                        "oversized LFS metadata in pushed history"
                    );
                    output.by_ref().take(remaining).read_to_end(&mut body)?;
                } else {
                    let drained = io::copy(&mut output.by_ref().take(remaining), &mut io::sink())?;
                    ensure!(drained == remaining, "truncated Git blob");
                }
                let mut lf = [0];
                output.read_exact(&mut lf)?;
                ensure!(lf == *b"\n", "invalid cat-file response");
                if matched {
                    visit(&body)?;
                }
            }
            Ok(())
        },
    )
}

/// Close the output before waiting, and stop Git if reading or visiting fails.
fn with_git_output(
    command: &mut Command,
    failure: &str,
    read: impl FnOnce(&mut BufReader<std::process::ChildStdout>) -> Result<()>,
) -> Result<()> {
    let mut child = command.stdout(Stdio::piped()).spawn()?;
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let result = read(&mut output);
    drop(output);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    ensure!(status.success(), "{failure}");
    Ok(())
}

pub fn pre_push(remote: &str, url: &str) -> Result<()> {
    let target = PushRemote::new(remote, url);
    let mut updates = tempfile::tempfile()?;
    io::copy(&mut io::stdin().lock(), &mut updates)?;
    // A user veto must run before either binary or retention metadata transfer.
    hooks::pre_push(remote, url, &mut updates)?;
    updates.rewind()?;
    let mut tips = BTreeSet::new();
    let mut managed_tips = BTreeSet::new();
    let mut remote_tips = BTreeSet::new();
    let mut ordinary = tempfile::tempfile()?;
    for line in BufReader::new(&mut updates).lines() {
        let line = line?;
        let fields: Vec<_> = line.split_whitespace().collect();
        let [_, local_oid, remote_ref, remote_oid] = fields.as_slice() else {
            bail!("malformed pre-push input");
        };
        for oid in [local_oid, remote_oid] {
            ensure!(is_git_oid(oid), "invalid pushed object id");
        }
        if !remote_oid.bytes().all(|b| b == b'0') {
            remote_tips.insert(remote_oid.to_string());
        }
        let managed = remote_ref.starts_with(retention::PREFIX);
        if !local_oid.bytes().all(|b| b == b'0') {
            tips.insert(local_oid.to_string());
            if managed {
                managed_tips.insert(local_oid.to_string());
            }
        }
        // Managed pointers use the CDC cache and transport. The stock handler
        // sees only ordinary user updates and retains its ordinary LFS cache.
        if !managed {
            writeln!(ordinary, "{line}")?;
        }
    }
    ordinary.rewind()?;
    let started = std::time::Instant::now();
    let status = target
        .command()
        .args([
            "-c",
            "lfs.allowincompletepush=false",
            "lfs",
            "pre-push",
            &target.name,
            url,
        ])
        .stdin(Stdio::from(ordinary))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    ensure!(status.success(), "standard Git LFS pre-push failed");
    trace("stock LFS pre-push", started, "completed");
    let destination = push::Destination::discover(&target, remote_tips)?;
    let full = push::full_check()?;
    if full && tips.is_empty() {
        // An up-to-date push supplies no updates. Still allow a full check to
        // repair locally available destination history without a dummy commit.
        tips.extend(destination.available.iter().cloned());
        managed_tips.extend(
            destination
                .roots
                .iter()
                .filter(|(oid, _)| destination.available.contains(oid))
                .map(|(oid, _)| oid.clone()),
        );
    }
    let mut objects = BTreeMap::new();
    if !tips.is_empty() {
        ensure!(
            git_out(&["rev-parse", "--is-shallow-repository"])? == "false",
            "cannot verify complete LFS history from a shallow repository; fetch --unshallow first"
        );
        objects = if full || destination.excluded.is_empty() {
            history::collect(&tips)?
        } else {
            history::collect_new(&tips, &destination.excluded)?
        };
        if !managed_tips.is_empty() {
            let excluded = if full {
                BTreeSet::new()
            } else {
                destination.excluded.clone()
            };
            retention::visit_objects(&managed_tips, &excluded, |oid, size| {
                transfer::add_object(&mut objects, oid, size, None)
            })?;
        }
    }
    if objects.is_empty() {
        if destination.needs_retention_guard()? {
            retention::Publication::new(&target, &destination.roots)?.publish(&BTreeMap::new())?;
        }
        Ok(())
    } else {
        transfer::publish(&Store::new()?, &target, &objects, &destination.roots)
    }
}

/// Text remains readable in `git diff`/`show`; binary diffs display the size
/// and content fingerprint. binary=true prevents `add -p` from patching a
/// recipe with a textconv patch (which Git cannot apply to transformed blobs).
pub fn textconv(path: &Path) -> Result<()> {
    let mut file = tempfile::tempfile()?;
    smudge(File::open(path)?, &mut file, &Store::new()?)?;
    file.rewind()?;
    let mut head = vec![0u8; 8000];
    let n = file.read(&mut head)?;
    if head[..n].contains(&0) {
        file.rewind()?;
        let mut hash = blake3::Hasher::new();
        hash.update_reader(&mut file)?;
        println!(
            "Binary content: {} bytes\nContent: blake3:{}",
            file.metadata()?.len(),
            hash.finalize().to_hex()
        );
    } else {
        let mut out = io::stdout().lock();
        out.write_all(&head[..n])?;
        io::copy(&mut file, &mut out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(data: &[u8]) -> Result<Manifest> {
        ensure!(
            data.len() as u64 <= MAX_MANIFEST_SIZE,
            "LFS manifest exceeds 16 MiB"
        );
        validate_manifest(Manifest::parse(data)?)
    }

    fn recipe() -> Manifest {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store {
            root: tmp.path().to_owned(),
        };
        let mut bytes = Vec::new();
        clean(
            &b"sample content"[..],
            &mut bytes,
            &store,
            ChunkParams::default(),
        )
        .unwrap();
        parse(&bytes).unwrap()
    }

    #[test]
    fn validates_lengths_offsets_parameters_and_lfs_hashes() {
        let base = recipe();
        assert_eq!(parse(base.encode().as_bytes()).unwrap(), base);
        let mut invalid = base.clone();
        invalid.chunks[0].offset = 1;
        assert!(parse(invalid.encode().as_bytes()).is_err());
        let mut invalid = base.clone();
        invalid.chunk_max = u32::MAX;
        assert!(parse(invalid.encode().as_bytes()).is_err());
        let mut invalid = base.clone();
        invalid.extra.remove(&lfs_key(&invalid.chunks[0]));
        assert!(parse(invalid.encode().as_bytes()).is_err());
        let mut invalid = base.clone();
        invalid
            .extra
            .insert(lfs_key(&invalid.chunks[0]), "../../outside".into());
        assert!(parse(invalid.encode().as_bytes()).is_err());
        let mut invalid = base.clone();
        invalid
            .extra
            .insert("storage".into(), "future-unsupported-version".into());
        assert!(parse(invalid.encode().as_bytes()).is_err());
        let duplicated = base.encode().replace(
            "storage git-lfs-v1\n",
            "storage git-lfs-v1\nstorage git-lfs-v1\n",
        );
        assert!(parse(duplicated.as_bytes()).is_err());
    }

    #[test]
    fn chunks_are_standard_raw_sha256_objects_and_corruption_is_repairable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store {
            root: tmp.path().to_owned(),
        };
        let data = b"hello";
        let oid = store.put(data).unwrap();
        assert_eq!(
            oid,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(fs::read(store.path(&oid)).unwrap(), data);
        let c = Chunk {
            hash: blake3::hash(data),
            offset: 0,
            length: data.len() as u32,
        };
        fs::write(store.path(&oid), b"damaged").unwrap();
        assert!(store.get(&oid, &c).is_err());
        assert_eq!(store.put(data).unwrap(), oid);
        assert_eq!(store.get(&oid, &c).unwrap(), data);
    }
}
