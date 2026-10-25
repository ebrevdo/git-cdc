//! Git plumbing: running git, resolving repo paths/config, and walking
//! history/index for manifests.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::Command as Git;

use anyhow::{Context, Result, bail};
use git_cdc_core::chunker::ChunkParams;
use git_cdc_core::manifest::{Manifest, is_manifest};
use git_cdc_core::store::DiskStore;

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

/// Manifests are ~90 bytes per chunk line; even a 100 GB file stays well
/// under this cap. Anything bigger can't be ours — skip without reading.
const MAX_MANIFEST_SIZE: u64 = 16 * 1024 * 1024;

/// Git quotes paths outside ASCII unless told not to; every path we read
/// back from git is used as a real filesystem path, so it must be literal.
pub const QUOTE_PATH: &str = "core.quotePath=false";

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
/// Lookups go through one long-lived `git cat-file --batch`, so a filter
/// process handling thousands of files spawns one child, not thousands.
pub struct ChunkParamsFor {
    local: ChunkParams,
    inherit: bool,
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
            batch: None,
        }
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
        // Address a newline-bearing path through argv, then send its object
        // id on the batch protocol. It still inherits the index's parameters.
        let revision = if path.contains('\n') {
            git_out(&["rev-parse", "--verify", &format!(":{path}")]).ok()?
        } else {
            format!(":{path}")
        };
        if self.batch.is_none() {
            let mut child = Git::new("git")
                .args(["cat-file", "--batch"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            self.batch = Some(CatFile {
                stdin: BufWriter::new(child.stdin.take()?),
                stdout: BufReader::new(child.stdout.take()?),
                child,
            });
        }
        let batch = self.batch.as_mut()?;

        writeln!(batch.stdin, "{revision}").ok()?;
        batch.stdin.flush().ok()?;
        let mut header = String::new();
        batch.stdout.read_line(&mut header).ok()?;
        // "<oid> <type> <size>" for a hit; "<rev> missing" otherwise.
        let mut parts = header.split_whitespace();
        let (_oid, typ, size) = (parts.next()?, parts.next()?, parts.next());
        if typ == "missing" {
            return None;
        }
        let size: u64 = size?.parse().ok()?;
        let size = size as usize;
        if typ != "blob" || size as u64 > MAX_MANIFEST_SIZE {
            // Still must drain the body, or the stream desynchronises.
            std::io::copy(
                &mut (&mut batch.stdout).take(size as u64 + 1),
                &mut std::io::sink(),
            )
            .ok()?;
            return None;
        }
        let mut body = vec![0u8; size + 1]; // content + trailing LF
        batch.stdout.read_exact(&mut body).ok()?;
        body.pop();
        if !is_manifest(&body) {
            return None;
        }
        let m = Manifest::parse(&body).ok()?;
        // A manifest committed with values fastcdc rejects must not be
        // inherited — chunking on them is undefined in a release build.
        ChunkParams {
            min: m.chunk_min,
            avg: m.chunk_avg,
            max: m.chunk_max,
        }
        .validate()
        .ok()
    }
}

impl Drop for ChunkParamsFor {
    fn drop(&mut self) {
        let Some(batch) = self.batch.take() else {
            return;
        };
        let CatFile {
            mut child,
            stdin,
            stdout,
        } = batch;
        drop(stdin); // closing the pipe is what tells cat-file to exit
        drop(stdout);
        let _ = child.wait();
    }
}

pub fn oid_str(hash: &blake3::Hash) -> String {
    format!("blake3:{}", hash.to_hex())
}

/// Every legacy manifest blob across history. Walk object IDs with rev-list,
/// then inspect bounded blobs using cat-file. Recipe signatures identify
/// historical and renamed files without relying on current attributes.
pub fn all_manifests() -> Result<Vec<Manifest>> {
    Ok(manifests_from(&["--all".into()])?
        .into_iter()
        .filter(|m| m.extra.get("storage").map(String::as_str) != Some("git-lfs-v1"))
        .collect())
}

/// Read recipe metadata for a revision set, including both CDC backends.
/// Used by local cache policy; legacy publication/GC keep their own filter.
pub(crate) fn manifests_from(revisions: &[String]) -> Result<Vec<Manifest>> {
    use std::process::Stdio;

    let mut args = vec!["rev-list", "--objects", "--no-object-names"];
    args.extend(revisions.iter().map(String::as_str));
    let list = git_out(&args)?;
    let mut shas: Vec<&str> = list.lines().filter(|s| !s.is_empty()).collect();
    shas.sort_unstable();
    shas.dedup();

    let mut cat = Git::new("git")
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning git cat-file")?;
    let mut to_git = BufWriter::new(cat.stdin.take().unwrap());
    let mut from_git = BufReader::new(cat.stdout.take().unwrap());

    let mut manifests = Vec::new();
    for sha in shas {
        // Sequential request/response per object — no pipe deadlock.
        writeln!(to_git, "{sha}")?;
        to_git.flush()?;
        let mut header = String::new();
        from_git.read_line(&mut header)?;
        let mut parts = header.split_whitespace();
        let (_sha, typ, size) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        if typ == "missing" {
            continue;
        }
        let size: u64 = size
            .parse()
            .with_context(|| format!("bad cat-file header: {header:?}"))?;
        if typ != "blob" || size > MAX_MANIFEST_SIZE {
            std::io::copy(&mut (&mut from_git).take(size + 1), &mut std::io::sink())?;
            continue;
        }
        let mut body = vec![0u8; size as usize + 1]; // content + trailing LF
        from_git.read_exact(&mut body)?;
        body.pop();
        if is_manifest(&body) {
            manifests.push(Manifest::parse(&body)?);
        }
    }
    drop(to_git);
    cat.wait()?;
    Ok(manifests)
}

/// Tracked (filter=cdc) paths in the index, with their staged manifests.
///
/// Every git call runs from the repo root, so the paths returned are always
/// repo-relative. Run from a subdirectory, a bare `ls-files` would list only
/// that subtree and name it relative to the cwd, which callers then join onto
/// `repo_root()` — silently wrong.
pub fn index_manifests() -> Result<Vec<(String, Manifest)>> {
    let root = repo_root()?;
    let root = root.to_string_lossy().into_owned();
    // core.quotePath=false: git C-quotes non-ASCII paths by default
    // ("caf\303\251.bin"), and the quoted form matches no file on disk.
    let files = git_out(&["-c", QUOTE_PATH, "-C", &root, "ls-files"])?;
    let mut out = Vec::new();
    for path in files.lines() {
        let attr = git_out(&[
            "-c",
            QUOTE_PATH,
            "-C",
            &root,
            "check-attr",
            "filter",
            "--",
            path,
        ])?;
        if !attr.ends_with(": filter: cdc") {
            continue;
        }
        let blob = Git::new("git")
            .args(["-C", &root, "show", &format!(":{path}")])
            .output()?;
        if !blob.status.success() {
            continue;
        }
        if is_manifest(&blob.stdout)
            && let Ok(m) = Manifest::parse(&blob.stdout)
        {
            out.push((path.to_string(), m));
        }
    }
    Ok(out)
}
