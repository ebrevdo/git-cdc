//! Disposable legacy push metadata. Remote presence is always negotiated
//! again; this cache only avoids rereading recipes in unchanged history.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use git_cdc_core::manifest::Manifest;
use serde_json::{Value, json};

use crate::git::{ManifestScan, git_bytes, git_dir, git_out, git_with_input, input_file};

const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

struct Snapshot {
    roots: BTreeMap<String, String>,
    chunks: HashMap<blake3::Hash, u64>,
}

fn is_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Snapshot {
    fn read(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_CACHE_BYTES,
            "oversized history cache"
        );
        let envelope: Value = serde_json::from_slice(&bytes)?;
        ensure!(envelope["version"] == 1, "unknown history cache version");
        let payload = envelope["payload"]
            .as_str()
            .context("missing history cache payload")?;
        ensure!(
            envelope["blake3"] == blake3::hash(payload.as_bytes()).to_hex().as_str(),
            "corrupt history cache"
        );
        let payload: Value = serde_json::from_str(payload)?;
        let mut roots = BTreeMap::new();
        for (name, oid) in payload["roots"]
            .as_object()
            .context("missing history roots")?
        {
            let oid = oid.as_str().context("invalid history root")?;
            ensure!(is_oid(oid), "invalid history root");
            roots.insert(name.clone(), oid.to_owned());
        }
        ensure!(!roots.is_empty(), "empty history roots");
        let mut chunks = HashMap::new();
        for entry in payload["chunks"]
            .as_array()
            .context("missing history chunks")?
        {
            let hash = blake3::Hash::from_hex(entry[0].as_str().context("invalid chunk hash")?)?;
            let size = entry[1].as_u64().context("invalid chunk size")?;
            ensure!(
                size > 0 && size <= git_cdc_core::chunker::CEILING as u64,
                "invalid chunk size"
            );
            if let Some(previous) = chunks.insert(hash, size) {
                ensure!(previous == size, "inconsistent chunk size");
            }
        }
        Ok(Self { roots, chunks })
    }

    fn covered_by(&self, roots: &BTreeMap<String, String>) -> Result<bool> {
        let tips: BTreeSet<_> = roots.values().collect();
        let previous: BTreeSet<_> = self
            .roots
            .values()
            .filter(|oid| !tips.contains(oid))
            .collect();
        if previous.is_empty() {
            return Ok(true);
        }
        // The cache is reusable only if every old tip is still reachable.
        // Check the whole set at once; non-commit or missing old tips force a
        // scan. Current roots can name commits, trees, blobs or annotated tags.
        let mut input = Vec::new();
        for oid in previous {
            writeln!(input, "{oid}^{{commit}}")?;
        }
        for oid in tips {
            writeln!(input, "^{oid}")?;
        }
        let output = Command::new("git")
            .args(["rev-list", "--max-count=1", "--stdin"])
            .stdin(input_file(&input)?)
            .stderr(Stdio::null())
            .output()?;
        Ok(output.status.success() && output.stdout.is_empty())
    }

    fn write(&self, path: &Path) -> Result<()> {
        let chunks: Vec<_> = self
            .chunks
            .iter()
            .map(|(hash, size)| json!([hash.to_hex().as_str(), size]))
            .collect();
        let payload = json!({"roots": self.roots, "chunks": chunks}).to_string();
        let envelope = json!({
            "version": 1,
            "blake3": blake3::hash(payload.as_bytes()).to_hex().as_str(),
            "payload": payload,
        });
        let directory = path.parent().unwrap();
        fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut file, &envelope)?;
        file.flush()?;
        file.persist(path)?;
        Ok(())
    }
}

fn collect_refs(
    root: &Path,
    namespace: &[u8],
    roots: &mut BTreeMap<String, String>,
) -> Result<bool> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["for-each-ref", "--format=%(objectname) %(refname)"]);
    if !namespace.is_empty() {
        command.args(["refs/bisect/", "refs/worktree/", "refs/rewritten/"]);
        // Hooks export the source repository's paths. A linked worktree's
        // private refs must be resolved in that worktree instead.
        for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
            command.env_remove(key);
        }
    }
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "cannot read history refs: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let refs = output.stdout;
    let mut cacheable = true;
    for line in refs.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let space = line
            .iter()
            .position(|b| *b == b' ')
            .context("invalid ref record")?;
        let oid = std::str::from_utf8(&line[..space])?;
        ensure!(is_oid(oid), "invalid ref object id");
        let name = &line[space + 1..];
        cacheable &= !name.starts_with(b"refs/replace/");
        let mut identity = namespace.to_vec();
        identity.push(0);
        identity.extend_from_slice(name);
        roots.insert(blake3::hash(&identity).to_hex().to_string(), oid.to_owned());
    }
    Ok(cacheable)
}

fn history_roots(root: &Path) -> Result<(BTreeMap<String, String>, bool)> {
    let common = git_out(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    // These can change reachability without changing refs. A full scan keeps
    // Git's existing replacement, graft and shallow-history semantics.
    let mut cacheable = !Path::new(&common).join("shallow").exists()
        && !Path::new(&common).join("info/grafts").exists()
        && ["GIT_REPLACE_REF_BASE", "GIT_GRAFT_FILE", "GIT_SHALLOW_FILE"]
            .iter()
            .all(|key| std::env::var_os(key).is_none());
    let mut roots = BTreeMap::new();
    cacheable &= collect_refs(root, b"", &mut roots)?;
    if let Ok(head) = git_out(&["rev-parse", "--verify", "HEAD"]) {
        ensure!(is_oid(&head), "invalid HEAD object id");
        roots.insert("HEAD".into(), head);
    }
    // --all includes detached HEADs and private refs in linked worktrees.
    // Snapshot those roots too, so cache reuse cannot hide their recipes.
    let worktrees = if cacheable {
        git_bytes(root, &["worktree", "list", "--porcelain", "-z"])?
    } else {
        Vec::new()
    };
    let mut worktree = None;
    for field in worktrees.split(|b| *b == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            worktree = Some(path);
            let directory = Path::new(std::str::from_utf8(path)?);
            if directory.is_dir() {
                cacheable &= collect_refs(directory, path, &mut roots)?;
            } else {
                cacheable = false;
            }
        } else if let Some(oid) = field.strip_prefix(b"HEAD ") {
            let oid = std::str::from_utf8(oid)?;
            ensure!(is_oid(oid), "invalid worktree HEAD");
            if oid.bytes().all(|b| b == b'0') {
                continue;
            }
            let path = worktree.context("missing worktree path")?;
            roots.insert(
                format!("worktree:{}", blake3::hash(path).to_hex()),
                oid.to_owned(),
            );
        }
    }
    // Validate roots even on a completely unchanged push: a disposable cache
    // must not conceal a missing tip in a damaged object database.
    let input = roots
        .values()
        .map(|oid| format!("{oid}\n"))
        .collect::<String>();
    let metadata = String::from_utf8(git_with_input(
        root,
        &["cat-file", "--batch-check"],
        Some(input.as_bytes()),
    )?)?;
    cacheable &= metadata.lines().count() == roots.len()
        && metadata
            .lines()
            .all(|line| line.split_whitespace().count() == 3);
    Ok((roots, cacheable))
}

pub(crate) fn collect() -> Result<HashMap<blake3::Hash, u64>> {
    let root = std::env::current_dir()?;
    let (roots, cacheable) = history_roots(&root)?;
    let path = git_dir()?.join("cdc/history-v1.json");
    let previous = if cacheable {
        Snapshot::read(&path).ok()
    } else {
        None
    };
    let previous = match previous {
        Some(previous) if previous.covered_by(&roots)? => Some(previous),
        _ => None,
    };
    // Equal roots cover the same immutable object graph, so the validated
    // chunk set also stays unchanged. Remote presence is still checked later.
    let unchanged = previous
        .as_ref()
        .is_some_and(|snapshot| snapshot.roots == roots);
    let (excluded, mut chunks) = previous
        .map(|snapshot| {
            (
                snapshot.roots.into_values().collect::<BTreeSet<_>>(),
                snapshot.chunks,
            )
        })
        .unwrap_or_default();
    let tips = roots.values().cloned().collect::<Vec<_>>();
    let visit = |manifest: &Manifest| {
        if manifest.extra.get("storage").map(String::as_str) == Some("git-lfs-v1") {
            return Ok(());
        }
        for chunk in &manifest.chunks {
            let size = chunk.length as u64;
            if let Some(previous) = chunks.insert(chunk.hash, size) {
                ensure!(previous == size, "inconsistent CDC chunk size");
            }
        }
        Ok(())
    };
    let mut scan = ManifestScan::new()?;
    if cacheable {
        scan.visit_tips(&tips, &excluded, visit)?;
    } else {
        scan.visit(&["--all".into()], visit)?;
    }
    let snapshot = Snapshot { roots, chunks };
    if cacheable && !unchanged {
        // Unwritable or lost metadata never blocks publication.
        let _ = snapshot.write(&path);
    }
    Ok(snapshot.chunks)
}
