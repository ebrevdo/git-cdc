//! Shared CDC worktree/cache lifecycle. Commands plan from indexed recipes,
//! validate before changing files, and delegate object I/O to backend adapters.
//! Cache eviction is local; it never invokes remote garbage collection.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use git_cdc_core::manifest::Manifest;

use crate::git::{ManifestScan, git_bytes, git_out, input_file, repo_root, visit_index_blobs};
use crate::manifest::detect;

mod backend;
mod lock;
use backend::{Backends, Presence};
use lock::IndexLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BackendKind {
    Legacy,
    Lfs,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ObjectId {
    Legacy(blake3::Hash),
    Lfs(String),
}

impl ObjectId {
    fn backend(&self) -> BackendKind {
        match self {
            Self::Legacy(_) => BackendKind::Legacy,
            Self::Lfs(_) => BackendKind::Lfs,
        }
    }
}

#[derive(Clone, Debug)]
struct Object {
    id: ObjectId,
    size: u64,
}

fn objects(manifest: &Manifest) -> Result<Vec<Object>> {
    let params = git_cdc_core::chunker::ChunkParams {
        min: manifest.chunk_min,
        avg: manifest.chunk_avg,
        max: manifest.chunk_max,
    }
    .validate()?;
    let mut offset = 0u64;
    for chunk in &manifest.chunks {
        ensure!(
            chunk.offset == offset && chunk.length > 0 && chunk.length <= params.max,
            "invalid chunk offset or length in CDC recipe"
        );
        offset = offset
            .checked_add(chunk.length as u64)
            .context("CDC file size overflow")?;
    }
    ensure!(offset == manifest.size, "CDC recipe size mismatch");
    let kind = match manifest.extra.get("storage").map(String::as_str) {
        None => BackendKind::Legacy,
        Some("git-lfs-v1") => BackendKind::Lfs,
        Some(storage) => bail!("unsupported CDC storage backend {storage:?}"),
    };
    manifest
        .chunks
        .iter()
        .map(|chunk| {
            let id = match kind {
                BackendKind::Legacy => ObjectId::Legacy(chunk.hash),
                BackendKind::Lfs => {
                    let key = format!("lfs.{}", chunk.hash.to_hex());
                    let oid = manifest
                        .extra
                        .get(&key)
                        .context("missing LFS chunk reference")?;
                    ensure!(
                        oid.len() == 64
                            && oid
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                        "invalid SHA-256 LFS chunk reference"
                    );
                    ObjectId::Lfs(oid.clone())
                }
            };
            Ok(Object {
                id,
                size: chunk.length as u64,
            })
        })
        .collect()
}

fn add_objects(target: &mut HashMap<ObjectId, Object>, manifest: &Manifest) -> Result<()> {
    for object in objects(manifest)? {
        let size = object.size;
        if let Some(previous) = target.insert(object.id.clone(), object) {
            ensure!(previous.size == size, "inconsistent CDC chunk size");
        }
    }
    Ok(())
}

struct IndexedFile {
    path: String,
    mode: String,
    blob_oid: String,
    stage: u8,
    blob: Vec<u8>,
    manifest: Manifest,
}

fn index_files(root: &Path) -> Result<Vec<IndexedFile>> {
    let mut files = Vec::new();
    visit_index_blobs(
        root,
        true,
        |filter| matches!(filter, b"cdc" | b"cdc-lfs"),
        |entry| {
            let Some(manifest) = detect(&entry.blob) else {
                return Ok(());
            };
            let is_lfs = manifest.extra.get("storage").map(String::as_str) == Some("git-lfs-v1");
            ensure!(
                (entry.filter == b"cdc-lfs") == is_lfs,
                "CDC filter/backend mismatch for {:?}",
                entry.path
            );
            objects(&manifest)?;
            files.push(IndexedFile {
                path: entry.path,
                mode: entry.mode,
                blob_oid: entry.blob_oid,
                stage: entry.stage,
                blob: entry.blob,
                manifest,
            });
            Ok(())
        },
    )?;
    Ok(files)
}

fn select(files: &[IndexedFile], paths: &[String], all: bool) -> Result<Vec<usize>> {
    if all {
        return Ok((0..files.len()).filter(|i| files[*i].stage == 0).collect());
    }
    let mut args = vec!["ls-files", "--error-unmatch", "--full-name", "-z", "--"];
    args.extend(paths.iter().map(String::as_str));
    let output = Command::new("git").args(args).output()?;
    ensure!(
        output.status.success(),
        "cannot resolve CDC paths: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let indexed: HashMap<_, _> = files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.stage == 0)
        .map(|(i, file)| (file.path.as_str(), i))
        .collect();
    for path in output
        .stdout
        .split(|b| *b == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(path)?;
        let i = indexed
            .get(path)
            .copied()
            .with_context(|| format!("{path:?} is not an unmerged-free indexed CDC file"))?;
        if seen.insert(i) {
            selected.push(i);
        }
    }
    ensure!(
        !selected.is_empty(),
        "no indexed CDC files match the selected paths"
    );
    Ok(selected)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileState {
    Missing,
    Recipe,
    Content,
    Modified,
}

fn file_state(root: &Path, file: &IndexedFile, verify_content: bool) -> Result<FileState> {
    let path = root.join(&file.path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileState::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Ok(FileState::Modified);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if (metadata.permissions().mode() & 0o111 != 0) != (file.mode == "100755") {
            return Ok(FileState::Modified);
        }
    }
    if metadata.len() == file.blob.len() as u64 && fs::read(&path)? == file.blob {
        return Ok(FileState::Recipe);
    }
    // Pull only replaces exact recipes or missing files. It does not need
    // to distinguish edited content from already-hydrated content.
    if !verify_content || metadata.len() != file.manifest.size {
        return Ok(FileState::Modified);
    }
    let mut hash = blake3::Hasher::new();
    hash.update_reader(File::open(path)?)?;
    Ok(if hash.finalize() == file.manifest.oid {
        FileState::Content
    } else {
        FileState::Modified
    })
}

fn check_index(root: &Path, files: &[&IndexedFile]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let current = git_bytes(root, &["ls-files", "--stage", "-z"])?;
    let mut entries: HashMap<&[u8], Vec<&[u8]>> = HashMap::new();
    for entry in current.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid index entry")?;
        entries.entry(&entry[tab + 1..]).or_default().push(entry);
    }
    for file in files {
        let expected = format!("{} {} 0\t{}", file.mode, file.blob_oid, file.path);
        ensure!(
            entries
                .get(file.path.as_bytes())
                .is_some_and(|entries| entries.as_slice() == [expected.as_bytes()]),
            "index changed while processing {:?}",
            file.path
        );
    }
    Ok(())
}

fn prepare_file(root: &Path, file: &IndexedFile) -> Result<tempfile::NamedTempFile> {
    let path = root.join(&file.path);
    let directory = path.parent().context("CDC file directory")?;
    fs::create_dir_all(directory)?;
    let temporary = tempfile::NamedTempFile::new_in(directory)?;
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if file.mode == "100755" { 0o755 } else { 0o644 };
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(temporary)
}

struct IndexUpdate<'a> {
    root: &'a Path,
    index: PathBuf,
    lock: IndexLock,
    temporary: tempfile::NamedTempFile,
    entries: Vec<u8>,
}

impl<'a> IndexUpdate<'a> {
    fn new(root: &'a Path, files: &[&IndexedFile]) -> Result<Self> {
        let path = String::from_utf8(git_bytes(root, &["rev-parse", "--git-path", "index"])?)?;
        let index = root.join(path.strip_suffix('\n').context("invalid Git index path")?);
        let mut lock = index.as_os_str().to_os_string();
        lock.push(".lock");
        let temporary = tempfile::Builder::new()
            .prefix(".git-cdc-index-")
            .tempfile_in(index.parent().context("Git index directory")?)?;
        let lock = IndexLock::acquire(PathBuf::from(lock), temporary.path())?;
        let update = Self {
            root,
            index,
            lock,
            temporary,
            entries: Vec::new(),
        };
        fs::copy(&update.index, update.temporary.path())?;
        check_index(root, files)?;
        Ok(update)
    }

    fn replace(
        &mut self,
        file: &IndexedFile,
        expected: FileState,
        temporary: tempfile::NamedTempFile,
    ) -> Result<()> {
        // Dehydration must be durable before evicting chunks. Hydration only
        // recreates content and follows Git's normal worktree durability policy.
        let durable = expected == FileState::Content;
        if durable {
            temporary.as_file().sync_all()?;
        }
        ensure!(
            file_state(self.root, file, durable)? == expected,
            "working file changed while processing {:?}",
            file.path
        );
        let path = self.root.join(&file.path);
        temporary.persist(&path)?;
        write!(
            self.entries,
            "{} {}\t{}\0",
            file.mode, file.blob_oid, file.path
        )?;
        #[cfg(unix)]
        if durable {
            File::open(path.parent().context("CDC file directory")?)?.sync_all()?;
        }
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(self.root)
            .env("GIT_INDEX_FILE", self.temporary.path());
        command
    }

    fn finish(mut self) -> Result<()> {
        if self.entries.is_empty() {
            return Ok(());
        }
        // Git writes a separate private index: a surviving child must never
        // recreate the real index.lock after signal cleanup removes it.
        let output = self
            .command()
            .args(["update-index", "-z", "--index-info"])
            .stdin(input_file(&self.entries)?)
            .output()?;
        ensure!(
            output.status.success(),
            "cannot reset CDC index stat data: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = self
            .command()
            .args(["update-index", "--refresh"])
            .output()?;
        self.lock.publish(self.temporary, &self.index)?;
        Ok(())
    }
}

fn reassemble(file: &IndexedFile, backends: &Backends, mut output: impl Write) -> Result<()> {
    let references = objects(&file.manifest)?;
    let mut hasher = blake3::Hasher::new();
    for (chunk, object) in file.manifest.chunks.iter().zip(references) {
        let backend = object.id.backend();
        let bytes = backends.get(backend).read(&object)?;
        // DiskStore already verifies BLAKE3. LFS cache reads verify SHA-256,
        // so those bytes still need their CDC chunk-hash check here.
        ensure!(
            bytes.len() as u64 == object.size
                && (backend == BackendKind::Legacy || blake3::hash(&bytes) == chunk.hash),
            "corrupt CDC chunk for {:?}",
            file.path
        );
        hasher.update(&bytes);
        output.write_all(&bytes)?;
    }
    ensure!(
        hasher.finalize() == file.manifest.oid,
        "reassembled file does not match recipe for {:?}",
        file.path
    );
    Ok(())
}

fn by_backend<'a>(objects: impl Iterator<Item = &'a Object>, kind: BackendKind) -> Vec<Object> {
    objects
        .filter(|object| object.id.backend() == kind)
        .cloned()
        .collect()
}

pub fn pull(paths: &[String]) -> Result<()> {
    let root = repo_root()?;
    let files = index_files(&root)?;
    let selected = select(&files, paths, paths.is_empty())?;
    let mut planned = Vec::new();
    for i in &selected {
        let file = &files[*i];
        let state = file_state(&root, file, false)?;
        if state == FileState::Recipe || (state == FileState::Missing && !paths.is_empty()) {
            planned.push((file, state));
        }
    }
    if planned.is_empty() {
        eprintln!("git-cdc: materialized 0 files");
        warn_chunk_param_mismatch(selected.iter().map(|i| &files[*i].manifest));
        return Ok(());
    }
    let backends = Backends::new()?;
    let mut needed = HashMap::new();
    for (file, _) in &planned {
        add_objects(&mut needed, &file.manifest)?;
    }
    for kind in [BackendKind::Legacy, BackendKind::Lfs] {
        let objects = by_backend(needed.values(), kind);
        if !objects.is_empty() {
            backends.get(kind).fetch(&objects)?;
        }
    }
    let mut index = IndexUpdate::new(
        &root,
        &planned.iter().map(|(file, _)| *file).collect::<Vec<_>>(),
    )?;
    let result = (|| {
        for (file, state) in &planned {
            let mut temporary = prepare_file(&root, file)?;
            reassemble(file, &backends, &mut temporary)?;
            index.replace(file, *state, temporary)?;
        }
        anyhow::Ok(())
    })();
    let refreshed = index.finish();
    result?;
    refreshed?;
    eprintln!("git-cdc: materialized {} files", planned.len());
    warn_chunk_param_mismatch(selected.iter().map(|i| &files[*i].manifest));
    Ok(())
}

/// Warn when local chunk parameters differ from the prevailing indexed set.
fn warn_chunk_param_mismatch<'a>(manifests: impl Iterator<Item = &'a Manifest>) {
    let Ok(local) = crate::git::chunk_params() else {
        return; // a broken config is `clean`'s error to report, not ours
    };
    let mut counts: HashMap<(u32, u32, u32), usize> = HashMap::new();
    for manifest in manifests {
        *counts
            .entry((manifest.chunk_min, manifest.chunk_avg, manifest.chunk_max))
            .or_default() += 1;
    }
    // The prevailing committed set — mixed histories are possible, and the
    // majority is the one worth matching.
    let Some((&(min, avg, max), n)) = counts.iter().max_by_key(|(_, n)| **n) else {
        return;
    };
    if (min, avg, max) == (local.min, local.avg, local.max) {
        return;
    }
    let inherit = git_out(&["config", "--type=bool", "--get", "cdc.chunk.inherit"])
        .map(|v| v != "false")
        .unwrap_or(true);
    eprintln!(
        "git-cdc: note: {n} tracked file(s) were committed with chunk parameters \
         (min/avg/max {min}/{avg}/{max}) that differ from this repo's config \
         ({}/{}/{}).",
        local.min, local.avg, local.max
    );
    if inherit {
        eprintln!(
            "git-cdc: those files keep their committed parameters, but new files \
             added here will use this repo's config — a mix that costs dedup. \
             To match the rest of the repo:"
        );
    } else {
        eprintln!(
            "git-cdc: cdc.chunk.inherit is false, so the next `git add` will \
             re-chunk them too, losing dedup against what the remote already \
             has. To match the committed settings:"
        );
    }
    eprintln!("git-cdc:     git config cdc.chunk.min {min}");
    eprintln!("git-cdc:     git config cdc.chunk.avg {avg}");
    eprintln!("git-cdc:     git config cdc.chunk.max {max}");
}

struct EvictionCandidate {
    object: Object,
    stored_bytes: u64,
}

struct DropPlan {
    files: Vec<(usize, FileState)>,
    candidates: HashMap<ObjectId, EvictionCandidate>,
    skipped: usize,
}

fn plan_drop(
    root: &Path,
    files: &[IndexedFile],
    selected: &[usize],
    backends: &Backends,
    chunks: bool,
) -> Result<DropPlan> {
    let mut plan = DropPlan {
        files: Vec::new(),
        candidates: HashMap::new(),
        skipped: 0,
    };
    let mut pinned = HashMap::new();
    let selected_set: HashSet<_> = selected.iter().copied().collect();
    for (i, file) in files.iter().enumerate() {
        if !selected_set.contains(&i) {
            add_objects(&mut pinned, &file.manifest)?;
        }
    }
    let mut modified = 0;
    for i in selected {
        let file = &files[*i];
        let state = file_state(root, file, true)?;
        match state {
            FileState::Modified | FileState::Missing => {
                if state == FileState::Modified {
                    eprintln!("git-cdc: {:?}: uncommitted changes; skipped", file.path);
                    modified += 1;
                }
                add_objects(&mut pinned, &file.manifest)?;
                plan.skipped += 1;
                continue;
            }
            FileState::Content => {
                // Prove that replacing the full file will not destroy its
                // only usable bytes. This reads bounded chunks, not files.
                reassemble(file, backends, std::io::sink())?;
            }
            FileState::Recipe => {}
        }
        if chunks {
            for object in objects(&file.manifest)? {
                let Some(stored_bytes) = backends.get(object.id.backend()).cached_bytes(&object)?
                else {
                    continue;
                };
                plan.candidates.insert(
                    object.id.clone(),
                    EvictionCandidate {
                        object,
                        stored_bytes,
                    },
                );
            }
        }
        plan.files.push((*i, state));
    }
    ensure!(
        selected.is_empty() || modified != selected.len(),
        "every selected file has uncommitted changes; nothing dropped"
    );
    plan.candidates.retain(|id, _| !pinned.contains_key(id));
    Ok(plan)
}

fn verify(
    candidates: &HashMap<ObjectId, EvictionCandidate>,
    backends: &Backends,
    dry_run: bool,
) -> Result<HashSet<ObjectId>> {
    let mut verified = HashSet::new();
    for kind in [BackendKind::Legacy, BackendKind::Lfs] {
        let batch = by_backend(candidates.values().map(|candidate| &candidate.object), kind);
        if batch.is_empty() {
            continue;
        }
        let backend = backends.get(kind);
        let results = match backend.remote_presence(&batch) {
            Ok(results) => results,
            Err(error) if dry_run => {
                eprintln!(
                    "git-cdc: cannot verify remote backing ({error:#}); cached chunks would be kept"
                );
                HashMap::new()
            }
            Err(error) => {
                return Err(
                    error.context("local eviction needs a remote: cannot verify safe backing")
                );
            }
        };
        if !dry_run
            && results
                .values()
                .any(|presence| *presence == Presence::Present)
        {
            backend.confirm_eviction()?;
        }
        for object in batch {
            match results
                .get(&object.id)
                .copied()
                .unwrap_or(Presence::Unknown)
            {
                Presence::Present => {
                    verified.insert(object.id);
                }
                Presence::Missing | Presence::Unknown => {
                    eprintln!(
                        "git-cdc: {:?}: remote backing not verified; kept locally",
                        object.id
                    );
                }
            }
        }
    }
    Ok(verified)
}

fn evict_verified(
    candidates: &HashMap<ObjectId, EvictionCandidate>,
    verified: &HashSet<ObjectId>,
    backends: &Backends,
    dry_run: bool,
) -> Result<u64> {
    let mut bytes = 0;
    for id in verified {
        let candidate = &candidates[id];
        if !dry_run {
            backends.get(id.backend()).evict(&candidate.object)?;
        }
        bytes += candidate.stored_bytes;
    }
    Ok(bytes)
}

pub fn drop_files(paths: &[String], all: bool, chunks: bool, dry_run: bool) -> Result<()> {
    ensure!(
        all || !paths.is_empty(),
        "usage: git cdc drop [--chunks] [--dry-run] --all | <path>..."
    );
    let root = repo_root()?;
    let files = index_files(&root)?;
    let selected = select(&files, paths, all)?;
    let backends = Backends::new()?;
    let plan = plan_drop(&root, &files, &selected, &backends, chunks)?;
    // Every remote proof is obtained before replacing any working files.
    let verified = verify(&plan.candidates, &backends, dry_run)?;
    let planned: Vec<_> = plan.files.iter().map(|(i, _)| &files[*i]).collect();
    let mut index = if dry_run || planned.is_empty() {
        None
    } else {
        Some(IndexUpdate::new(&root, &planned)?)
    };
    let mut freed = 0;
    let mut dropped = 0;
    let result = (|| {
        for (i, state) in &plan.files {
            let file = &files[*i];
            if *state != FileState::Content {
                continue;
            }
            if let Some(index) = &mut index {
                let mut temporary = prepare_file(&root, file)?;
                temporary.write_all(&file.blob)?;
                index.replace(file, *state, temporary)?;
            }
            freed += file.manifest.size.saturating_sub(file.blob.len() as u64);
            dropped += 1;
        }
        if dry_run {
            check_index(&root, &planned)?;
        }
        evict_verified(&plan.candidates, &verified, &backends, dry_run)
    })();
    let refreshed = index.map(IndexUpdate::finish).transpose();
    let chunk_bytes = result?;
    refreshed?;
    let verb = if dry_run { "would drop" } else { "dropped" };
    eprintln!(
        "git-cdc: {verb} {dropped} files ({} skipped), {} local chunks; {freed} bytes of worktree + {chunk_bytes} bytes of chunks freed",
        plan.skipped,
        verified.len()
    );
    if dropped > 0 && !dry_run {
        eprintln!("git-cdc: run `git cdc pull` to restore file content");
    }
    Ok(())
}

fn worktree_roots(root: &Path) -> Result<Vec<PathBuf>> {
    let listed = git_bytes(root, &["worktree", "list", "--porcelain", "-z"])?;
    listed
        .split(|b| *b == 0)
        .filter_map(|field| field.strip_prefix(b"worktree "))
        .map(|path| Ok(PathBuf::from(std::str::from_utf8(path)?)))
        .collect()
}

#[derive(PartialEq, Eq)]
struct ProtectionSnapshot {
    refs: Vec<u8>,
    worktrees: Vec<u8>,
    indexes: Vec<Vec<u8>>,
    stashes: Vec<u8>,
}

fn protection_snapshot(root: &Path) -> Result<ProtectionSnapshot> {
    let indexes = worktree_roots(root)?
        .iter()
        .map(|worktree| git_bytes(worktree, &["ls-files", "--stage", "-z"]))
        .collect::<Result<_>>()?;
    let stashes = if git_out(&["rev-parse", "--verify", "--quiet", "refs/stash"]).is_ok() {
        git_bytes(root, &["reflog", "show", "--format=%H", "refs/stash"])?
    } else {
        Vec::new()
    };
    Ok(ProtectionSnapshot {
        refs: git_bytes(root, &["for-each-ref", "--format=%(objectname) %(refname)"])?,
        worktrees: git_bytes(root, &["worktree", "list", "--porcelain", "-z"])?,
        indexes,
        stashes,
    })
}

pub fn prune(dry_run: bool) -> Result<()> {
    let root = repo_root()?;
    let snapshot = protection_snapshot(&root)?;
    let mut known = HashMap::new();
    let mut scan = ManifestScan::new()?;
    scan.visit(&["--all".into()], |manifest| {
        add_objects(&mut known, manifest)
    })?;
    let mut protected = HashMap::new();
    for worktree in worktree_roots(&root)? {
        for file in index_files(&worktree)? {
            add_objects(&mut protected, &file.manifest)?;
            add_objects(&mut known, &file.manifest)?;
        }
        let head = git_bytes(&worktree, &["rev-parse", "--verify", "HEAD"])?;
        let head = String::from_utf8(head)?.trim().to_owned();
        scan.visit(&["--no-walk".into(), head], |manifest| {
            add_objects(&mut protected, manifest)
        })?;
    }
    // Reachability alone is not permission to evict: preserve local-only
    // histories, plus every stash reflog entry (not just refs/stash's tip).
    scan.visit(
        &["--all".into(), "--not".into(), "--remotes=origin".into()],
        |manifest| add_objects(&mut protected, manifest),
    )?;
    if git_out(&["rev-parse", "--verify", "--quiet", "refs/stash"]).is_ok() {
        let stash = git_out(&["reflog", "show", "--format=%H", "refs/stash"])?;
        let mut revisions = vec!["--no-walk".to_owned()];
        for oid in stash.lines() {
            revisions.push(oid.to_owned());
            // Stash parents after the base hold staged and optional untracked
            // snapshots. Retain those trees, not every ancestor of the stash.
            let parents = git_out(&["show", "--no-patch", "--format=%P", oid])?;
            revisions.extend(parents.split_whitespace().skip(1).map(str::to_owned));
        }
        scan.visit(&revisions, |manifest| {
            add_objects(&mut protected, manifest)?;
            add_objects(&mut known, manifest)
        })?;
    }
    let backends = Backends::new()?;
    let mut candidates = HashMap::new();
    for kind in [BackendKind::Legacy, BackendKind::Lfs] {
        // Empty/unpublished content is never guessed from file names. Only
        // objects with recipe metadata can become eviction candidates.
        if !known.keys().any(|id| id.backend() == kind) {
            continue;
        }
        for cached in backends.get(kind).inventory()? {
            if !protected.contains_key(&cached.id)
                && let Some(object) = known.get(&cached.id)
            {
                candidates.insert(
                    cached.id,
                    EvictionCandidate {
                        object: object.clone(),
                        stored_bytes: cached.stored_bytes,
                    },
                );
            }
        }
    }
    let verified = verify(&candidates, &backends, dry_run)?;
    ensure!(
        protection_snapshot(&root)? == snapshot,
        "refs, indexes, or worktrees changed during cache planning; retry prune"
    );
    let bytes = evict_verified(&candidates, &verified, &backends, dry_run)?;
    let verb = if dry_run { "would prune" } else { "pruned" };
    eprintln!(
        "git-cdc: {verb} {} local chunks ({bytes} bytes); working files unchanged",
        verified.len()
    );
    Ok(())
}
