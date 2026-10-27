//! Local cache lifecycle and read-only backing checks through stock Git LFS.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, ensure};

use super::{Store, context::DownloadContext, is_sha256, retention};

pub(crate) struct Cache {
    store: Store,
    context: OnceCell<DownloadContext>,
    verified: RefCell<HashSet<String>>,
    risk_confirmed: Cell<bool>,
}

impl Cache {
    pub fn new() -> Result<Self> {
        Ok(Self {
            store: Store::new()?,
            context: OnceCell::new(),
            verified: RefCell::new(HashSet::new()),
            risk_confirmed: Cell::new(false),
        })
    }

    fn context(&self) -> Result<&DownloadContext> {
        if let Some(context) = self.context.get() {
            return Ok(context);
        }
        let context = DownloadContext::new()?;
        Ok(self.context.get_or_init(|| context))
    }

    fn checked_path(&self, oid: &str) -> Result<PathBuf> {
        ensure!(is_sha256(oid), "invalid LFS cache object ID");
        let root = self.object_directory()?;
        for directory in [
            root.clone(),
            root.join(&oid[..2]),
            root.join(&oid[..2]).join(&oid[2..4]),
        ] {
            check_directory(&directory, "LFS cache shard is not a directory")?;
        }
        Ok(self.store.path(oid))
    }

    fn object_directory(&self) -> Result<PathBuf> {
        for directory in [
            self.store.root.parent().context("LFS cache parent")?,
            self.store.root.as_path(),
        ] {
            check_directory(directory, "LFS cache directory must not be a symlink")?;
        }
        Ok(self.store.root.join("objects"))
    }

    pub fn cached_bytes(&self, oid: &str) -> Result<Option<u64>> {
        let path = self.checked_path(oid)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn read(&self, oid: &str, size: u64) -> Result<Vec<u8>> {
        ensure!(
            self.cached_bytes(oid)?.is_some(),
            "LFS chunk {oid} is not cached"
        );
        let _lease = self.store.lease(oid, false)?;
        ensure!(
            self.cached_bytes(oid)?.is_some(),
            "LFS chunk {oid} is not cached"
        );
        self.store.read_cached_leased(oid, size)
    }

    pub fn fetch(&self, objects: &[(String, u64)]) -> Result<()> {
        const BATCH_OBJECTS: usize = 128;
        const BATCH_BYTES: u64 = 64 * 1024 * 1024;
        let mut pending = Vec::new();
        let mut pending_bytes = 0;
        for (oid, size) in objects {
            if self.cached_bytes(oid)?.is_some() {
                continue;
            }
            if !pending.is_empty()
                && (pending.len() == BATCH_OBJECTS || pending_bytes + size > BATCH_BYTES)
            {
                self.fetch_batch(&pending)?;
                pending.clear();
                pending_bytes = 0;
            }
            pending.push((oid.clone(), *size));
            pending_bytes += size;
        }
        if !pending.is_empty() {
            self.fetch_batch(&pending)?;
        }
        Ok(())
    }

    fn fetch_batch(&self, objects: &[(String, u64)]) -> Result<()> {
        self.context()?.download_batch(objects, |oid, bytes| {
            ensure!(
                self.store.put(&bytes)? == oid,
                "LFS download object ID mismatch"
            );
            Ok(())
        })
    }

    pub fn inventory(&self) -> Result<Vec<(String, u64)>> {
        let root = self.object_directory()?;
        if !root.exists() {
            return Ok(Vec::new());
        }
        ensure!(
            fs::symlink_metadata(&root)?.is_dir(),
            "LFS cache root is not a directory"
        );
        let mut objects = Vec::new();
        for first in fs::read_dir(&root)? {
            let first = first?;
            if !first.file_type()?.is_dir() {
                continue;
            }
            for second in fs::read_dir(first.path())? {
                let second = second?;
                if !second.file_type()?.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(second.path())? {
                    let entry = entry?;
                    let Some(oid) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if !is_sha256(&oid)
                        || entry.path() != self.store.path(&oid)
                        || !entry.file_type()?.is_file()
                    {
                        continue;
                    }
                    objects.push((oid, entry.metadata()?.len()));
                }
            }
        }
        Ok(objects)
    }

    pub fn remote_presence(&self, objects: &[(String, u64)]) -> Result<HashSet<String>> {
        self.verified.borrow_mut().clear();
        if objects.is_empty() {
            return Ok(HashSet::new());
        }
        let context = self.context()?;
        let roots = fetch_retention_roots(context)?;
        if roots.is_empty() {
            return Ok(HashSet::new());
        }
        let wanted: BTreeMap<_, _> = objects
            .iter()
            .map(|(oid, size)| (oid.as_str(), *size))
            .collect();
        let mut rooted = BTreeSet::new();
        retention::visit_candidates_with(
            &roots,
            &wanted,
            |oid, size| {
                if wanted.get(oid) == Some(&size) {
                    rooted.insert(oid.to_owned());
                }
                Ok(())
            },
            || context.command(),
        )?;
        let rooted: Vec<_> = objects
            .iter()
            .filter(|(oid, _)| rooted.contains(oid))
            .cloned()
            .collect();
        let verified = context.verify_remote(&rooted)?;
        *self.verified.borrow_mut() = verified.clone();
        Ok(verified)
    }

    pub fn confirm_eviction(&self) -> Result<()> {
        if self.risk_confirmed.get() {
            return Ok(());
        }
        let context = self.context()?;
        let Some(risk) = &context.verification_risk else {
            return Ok(());
        };
        eprintln!(
            "git-cdc: warning: Git LFS prune verification can falsely succeed for {risk}. Evicting cached chunks could lose their only copy."
        );
        let label = risk.label();
        ensure!(
            io::stdin().is_terminal(),
            "{label} cache eviction requires interactive confirmation; no chunks evicted"
        );
        eprint!("Continue with {label} cache eviction? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        ensure!(
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
            "{label} cache eviction cancelled"
        );
        self.risk_confirmed.set(true);
        Ok(())
    }

    pub fn evict(&self, oid: &str) -> Result<()> {
        let path = self.checked_path(oid)?;
        ensure!(
            self.verified.borrow().contains(oid),
            "LFS object has no remote eviction proof"
        );
        ensure!(
            self.context()?.verification_risk.is_none() || self.risk_confirmed.get(),
            "LFS eviction risk was not confirmed"
        );
        let _lease = self.store.lease(oid, true)?;
        self.checked_path(oid)?;
        if self.cached_bytes(oid)?.is_none() {
            return Ok(());
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("evicting LFS cache object"),
        }
    }
}

fn check_directory(path: &Path, message: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(metadata.is_dir(), "{message}"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn fetch_retention_roots(context: &DownloadContext) -> Result<BTreeSet<String>> {
    let destination = context.destination()?;
    context.share_source_objects()?;
    let output = context
        .command()
        .args([
            "ls-remote",
            "--refs",
            destination,
            &format!("{}*", retention::PREFIX),
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "cannot discover remote LFS retention roots"
    );
    let (roots, references): (BTreeSet<_>, Vec<_>) =
        retention::parse_remote_roots(std::str::from_utf8(&output.stdout)?)?
            .into_iter()
            .unzip();
    if references.is_empty() {
        return Ok(roots);
    }
    let mut input = tempfile::tempfile()?;
    for reference in references {
        writeln!(input, "{reference}")?;
    }
    input.rewind()?;
    let status = context
        .command()
        .args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--stdin",
            destination,
        ])
        .stdin(Stdio::from(input))
        .stdout(Stdio::null())
        .status()?;
    ensure!(
        status.success(),
        "cannot fetch remote LFS retention metadata"
    );
    Ok(roots)
}
