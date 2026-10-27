//! Negotiate standard-pointer batches before fetching their content. Keep
//! credentials, endpoint discovery and transfer adapters in the Git LFS client.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::{PushRemote, Store, git_out, lfs_pointer, repo_root, retention};

const BATCH_OBJECTS: usize = 128;

fn batch_bytes() -> Result<u64> {
    let key = "cdc.lfs.transferBatchBytes";
    let out = Command::new("git")
        .args(["config", "--type=int", "--get", key])
        .output()?;
    if out.status.code() == Some(1) {
        return Ok(64 * 1024 * 1024);
    }
    ensure!(
        out.status.success(),
        "cannot read {key}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let bytes = String::from_utf8(out.stdout)?
        .trim()
        .parse::<u64>()
        .with_context(|| format!("{key} must be a positive byte size"))?;
    ensure!(bytes > 0, "{key} must be a positive byte size");
    Ok(bytes)
}

pub(super) struct Object {
    pub size: u64,
    pub hash: Option<blake3::Hash>,
}

pub(super) fn add_object(
    objects: &mut BTreeMap<String, Object>,
    oid: &str,
    size: u64,
    hash: Option<blake3::Hash>,
) -> Result<()> {
    let entry = objects
        .entry(oid.to_owned())
        .or_insert(Object { size, hash });
    ensure!(entry.size == size, "inconsistent LFS chunk size");
    if let Some(hash) = hash {
        ensure!(
            entry.hash.is_none_or(|previous| previous == hash),
            "inconsistent LFS chunk hash"
        );
        entry.hash = Some(hash);
    }
    Ok(())
}

fn pointer_objects(batch: &[(&str, &Object)]) -> BTreeMap<String, u64> {
    batch
        .iter()
        .map(|(oid, object)| ((*oid).to_owned(), object.size))
        .collect()
}

pub(super) fn publish(
    store: &Store,
    target: &PushRemote,
    objects: &BTreeMap<String, Object>,
) -> Result<()> {
    let mut session = TransferSession {
        store,
        target,
        batch_bytes: batch_bytes()?,
        file: FileUpload::start(store, target)?,
        publication: retention::Publication::new(target)?,
        pending: BTreeMap::new(),
    };
    let mut batch = Vec::new();
    for (oid, object) in objects {
        if batch.len() == BATCH_OBJECTS {
            session.publish_batch(&batch)?;
            batch.clear();
        }
        batch.push((oid.as_str(), object));
    }
    if !batch.is_empty() {
        session.publish_batch(&batch)?;
    }
    if !session.pending.is_empty() {
        session.publication.publish(&session.pending)?;
    }
    Ok(())
}

struct TransferSession<'a> {
    store: &'a Store,
    target: &'a PushRemote,
    batch_bytes: u64,
    file: Option<FileUpload>,
    publication: retention::Publication<'a>,
    pending: BTreeMap<String, u64>,
}

impl TransferSession<'_> {
    fn publish_batch(&mut self, batch: &[(&str, &Object)]) -> Result<()> {
        // Sorted acquisition avoids lock-order cycles between linked worktrees.
        // Readers share leases; clean and eviction take exclusive leases.
        let leases = batch
            .iter()
            .map(|(oid, _)| self.store.lease(oid, false))
            .collect::<Result<Vec<_>>>()?;
        let missing_bytes: u64 = batch
            .iter()
            .filter(|(oid, _)| !self.store.path(oid).is_file())
            .map(|(_, object)| object.size)
            .sum();
        let started = std::time::Instant::now();
        for (oid, object) in batch {
            if self.store.path(oid).is_file() {
                self.verify(oid, object)?;
            }
        }
        super::trace(
            "LFS verification",
            started,
            format_args!("{} objects", batch.len()),
        );
        if missing_bytes > self.batch_bytes && batch.len() > 1 {
            // Cold history often already exists remotely. Negotiate metadata
            // first, keeping credentials in one client for up to 128 objects.
            // Only failed negotiations need smaller recovery batches.
            let started = std::time::Instant::now();
            if self.file.is_none() {
                let result = self.upload_with_client(batch)?;
                super::trace(
                    "LFS transfer",
                    started,
                    format_args!(
                        "{} objects, 0 recovered, negotiation {}",
                        batch.len(),
                        if result.status.success() {
                            "completed"
                        } else {
                            "requires recovery"
                        }
                    ),
                );
                if result.status.success() {
                    self.pending.extend(pointer_objects(batch));
                    return Ok(());
                }
            }
            // Check the budget under leases: concurrent eviction must not turn
            // a warm plan into an unbounded download.
            drop(leases);
            let (first, second) = batch.split_at(batch.len() / 2);
            self.publish_batch(first)?;
            return self.publish_batch(second);
        }
        let mut fetched = Vec::new();
        let started = std::time::Instant::now();
        self.upload(batch, &mut fetched)?;
        super::trace(
            "LFS transfer",
            started,
            format_args!("{} objects, {} recovered", batch.len(), fetched.len()),
        );
        self.pending.extend(pointer_objects(batch));
        // Publication invokes user reference-transaction/server hooks. Chunk
        // bytes are no longer in use, so release leases before those hooks run.
        drop(leases);
        // Warm pushes combine all pointers in one root. A cold transfer must
        // pin recovered bytes before evicting them to respect its disk budget.
        if fetched.is_empty() {
            return Ok(());
        }
        self.publication.publish(&self.pending)?;
        self.pending.clear();
        // Keep pre-existing and unpublished cache entries. Only content fetched
        // by this session is evicted, after its remote retention pin succeeds.
        for oid in fetched {
            let _lease = self.store.lease(&oid, true)?;
            match fs::remove_file(self.store.path(&oid)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn verify(&self, oid: &str, object: &Object) -> Result<()> {
        let bytes = self
            .store
            .get_object_leased(oid, object.size, Some(self.target))?;
        if let Some(hash) = object.hash {
            ensure!(blake3::hash(&bytes) == hash, "corrupt LFS chunk {oid}");
        }
        Ok(())
    }

    fn recover(&self, oid: &str, object: &Object, fetched: &mut Vec<String>) -> Result<()> {
        self.verify(oid, object)?;
        fetched.push(oid.to_owned());
        Ok(())
    }

    fn upload(&mut self, batch: &[(&str, &Object)], fetched: &mut Vec<String>) -> Result<()> {
        if let Some(mut file) = self.file.take() {
            self.upload_with_file(&mut file, batch, fetched)?;
            self.file = Some(file);
            return Ok(());
        }

        let out = self.upload_with_client(batch)?;
        if out.status.success() {
            return Ok(());
        }

        let missing_local = batch.iter().any(|(oid, _)| !self.store.path(oid).is_file());
        if !missing_local {
            bail!(
                "LFS upload failed; user refs were not published: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if batch.len() > 1 {
            // The CLI exposes success/failure, not a machine-readable missing
            // set. Split failed cold batches; never parse translated errors or
            // replace Git LFS's credential and transfer-adapter implementation.
            let (first_half, second_half) = batch.split_at(batch.len() / 2);
            self.upload(first_half, fetched)?;
            return self.upload(second_half, fetched);
        }
        let (oid, object) = batch[0];
        self.recover(oid, object, fetched)?;
        self.upload(batch, fetched)
    }

    fn upload_with_file(
        &self,
        file: &mut FileUpload,
        batch: &[(&str, &Object)],
        fetched: &mut Vec<String>,
    ) -> Result<()> {
        for (oid, object) in batch {
            let path = self.store.path(oid);
            if file.upload(oid, object.size, &path)? {
                continue;
            }
            if !path.is_file() {
                self.recover(oid, object, fetched)?;
            }
            ensure!(
                file.upload(oid, object.size, &path)?,
                "LFS upload failed for {oid}; user refs were not published"
            );
        }
        Ok(())
    }

    fn upload_with_client(&self, batch: &[(&str, &Object)]) -> Result<Output> {
        // --object-id stats every local file before negotiation. Give the stock
        // client a private pointer-only worktree instead: this is the same state
        // as a clone made with smudge disabled, with IDs and sizes available.
        let commit = retention::stage(&pointer_objects(batch), &[])?;
        let worktree = tempfile::tempdir()?;
        fs::write(
            worktree.path().join(".gitattributes"),
            retention::ATTRIBUTES,
        )?;
        for (oid, object) in batch {
            let path = worktree.path().join(retention::pointer_path(oid));
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, lfs_pointer(oid, object.size))?;
        }
        // Preserve an uncommitted .lfsconfig too. Otherwise Git LFS falls back
        // to the original repository's index/HEAD, using the same Git directory.
        if let Ok(root) = repo_root() {
            let config = root.join(".lfsconfig");
            if config.is_file() {
                fs::copy(config, worktree.path().join(".lfsconfig"))?;
            }
        }
        let mut command = self.store.command();
        self.target.apply(&mut command);
        Ok(command
            .arg("--work-tree")
            .arg(worktree.path())
            .args(["lfs", "push", "--all", &self.target.name, &commit])
            .output()?)
    }
}

/// The stock queue rejects absent source files before invoking a standalone
/// adapter. Use the installed file adapter's documented JSON protocol directly
/// so its existing-object check runs first. It also resolves .lfsconfig and
/// remote-specific endpoints; HTTP/SSH endpoints fall back to the stock queue.
struct FileUpload {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl FileUpload {
    fn start(store: &Store, target: &PushRemote) -> Result<Option<Self>> {
        // Explicit custom standalone agents must retain control of transfers.
        if git_out(&[
            "config",
            "--get-regexp",
            "^lfs\\..*standalonetransferagent$",
        ])
        .is_ok()
        {
            return Ok(None);
        }
        let mut command = store.command();
        target.apply(&mut command);
        let mut child = command
            .args(["lfs", "standalone-file"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut client = Self {
            input: child.stdin.take(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let response = client.request(json!({
            "event": "init",
            "operation": "upload",
            "remote": target.name,
            "concurrent": false,
            "concurrenttransfers": 1
        }))?;
        if response.get("error").is_some() {
            return Ok(None);
        }
        Ok(Some(client))
    }

    fn request(&mut self, request: Value) -> Result<Value> {
        let input = self.input.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &request)?;
        writeln!(input)?;
        input.flush()?;
        let mut line = String::new();
        ensure!(
            self.output.read_line(&mut line)? > 0,
            "Git LFS file adapter closed its response stream"
        );
        serde_json::from_str(&line).context("invalid Git LFS file-adapter response")
    }

    fn upload(&mut self, oid: &str, size: u64, path: &Path) -> Result<bool> {
        let response = self.request(json!({
            "event": "upload", "oid": oid, "size": size, "path": path
        }))?;
        ensure!(
            response["event"] == "complete" && response["oid"] == oid,
            "unexpected Git LFS file-adapter response"
        );
        Ok(response.get("error").is_none())
    }
}

impl Drop for FileUpload {
    fn drop(&mut self) {
        if let Some(mut input) = self.input.take() {
            let _ = writeln!(input, "{{\"event\":\"terminate\"}}");
        }
        let _ = self.child.wait();
    }
}
