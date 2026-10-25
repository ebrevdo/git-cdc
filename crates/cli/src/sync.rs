//! Chunk movement between local and remote stores: push, pull, gc.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use git_cdc_core::protocol::{ObjectSpec, Operation};
use git_cdc_core::store::ChunkStore;

use crate::git::{all_manifests, local_store, oid_str};
use crate::remote::{Remote, remote};

/// Chunks the local store has but a remote (identified by its own listing)
/// does not — the upload diff shared by every serverless remote kind.
fn pending_uploads(
    chunks: &HashMap<blake3::Hash, u64>,
    present: &HashSet<blake3::Hash>,
) -> Vec<blake3::Hash> {
    chunks
        .keys()
        .filter(|h| !present.contains(*h))
        .copied()
        .collect()
}

/// Which of `chunks` the configured remote already holds — the safety gate
/// for `drop --chunks`: a chunk the remote has is refetchable by `git cdc
/// pull`, so deleting the local copy is reversible no matter which other
/// files or commits share it. Unlike `cmd_gc`, a missing remote is an error
/// here, not a skipped step.
pub(crate) fn remote_present(chunks: &HashMap<blake3::Hash, u64>) -> Result<HashSet<blake3::Hash>> {
    if chunks.is_empty() {
        return Ok(HashSet::new());
    }
    // The serverless remotes answer with one listing, same as `cmd_push`.
    let intersect = |present: HashSet<blake3::Hash>| -> HashSet<blake3::Hash> {
        present
            .into_iter()
            .filter(|h| chunks.contains_key(h))
            .collect()
    };
    Ok(match remote()? {
        Remote::Http(client) => {
            let objects: Vec<ObjectSpec> = chunks
                .iter()
                .map(|(h, size)| ObjectSpec {
                    oid: oid_str(h),
                    size: *size,
                })
                .collect();
            let resp = client.batch(Operation::Download, objects)?;
            let mut present = HashSet::new();
            for obj in &resp.objects {
                // An error or a missing download action both mean "not there".
                if obj.error.is_some()
                    || obj
                        .actions
                        .as_ref()
                        .and_then(|a| a.download.as_ref())
                        .is_none()
                {
                    continue;
                }
                present.insert(git_cdc_core::manifest::parse_hash(&obj.oid)?);
            }
            present
        }
        Remote::Opendal { store, rt } => rt.block_on(async {
            anyhow::Ok(intersect(
                store.list().await?.into_iter().map(|(h, _)| h).collect(),
            ))
        })?,
        Remote::Ssh(mut ssh) => intersect(ssh.list()?.into_iter().map(|(h, _)| h).collect()),
    })
}

/// What a GC sweep should do with one chunk: shared mark-and-sweep + grace
/// rule applied by the local sweep and every serverless remote sweep alike.
/// An unreadable/missing mtime always keeps the chunk — GC must never delete
/// something whose age it cannot confirm.
enum SweepAction {
    KeepLive,
    KeepGrace,
    Delete,
}

fn sweep_decision(
    is_live: bool,
    modified: Option<SystemTime>,
    grace: Duration,
    now: SystemTime,
) -> SweepAction {
    if is_live {
        return SweepAction::KeepLive;
    }
    let old_enough = modified
        .and_then(|mtime| now.duration_since(mtime).ok())
        .is_some_and(|age| age >= grace);
    if old_enough {
        SweepAction::Delete
    } else {
        SweepAction::KeepGrace
    }
}

pub fn cmd_push() -> Result<()> {
    let store = local_store()?;

    let mut chunks: std::collections::HashMap<blake3::Hash, u64> = Default::default();
    for m in all_manifests()? {
        for c in &m.chunks {
            chunks.insert(c.hash, c.length as u64);
        }
    }
    if chunks.is_empty() {
        eprintln!("git-cdc: no manifests found, nothing to push");
        return Ok(());
    }
    let total = chunks.len();

    let uploaded = match remote()? {
        Remote::Http(client) => {
            let objects: Vec<ObjectSpec> = chunks
                .iter()
                .map(|(h, size)| ObjectSpec { oid: oid_str(h), size: *size })
                .collect();
            let resp = client.batch(Operation::Upload, objects)?;
            let mut pending: Vec<(String, blake3::Hash)> = Vec::new();
            for obj in &resp.objects {
                let Some(action) = obj.actions.as_ref().and_then(|a| a.upload.as_ref()) else {
                    continue; // server already has it — the dedup win
                };
                let hash = git_cdc_core::manifest::parse_hash(&obj.oid)?;
                if !store.has(&hash) {
                    bail!(
                        "server wants {} but it is not in the local store — run `git cdc pull` first (a dropped chunk from `git cdc drop --chunks` needs refetching)",
                        obj.oid
                    );
                }
                pending.push((action.href.clone(), hash));
            }

            // Bounded concurrency: chunks are ~MBs, so round-trips dominate
            // on high-latency links; a few workers pulling from a shared
            // index overlap them without flooding the server.
            const UPLOAD_WORKERS: usize = 4;
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|scope| -> Result<()> {
                let workers: Vec<_> = (0..UPLOAD_WORKERS.min(pending.len()))
                    .map(|_| {
                        scope.spawn(|| -> Result<()> {
                            loop {
                                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let Some((href, hash)) = pending.get(i) else {
                                    return Ok(());
                                };
                                client.upload(href, store.get_encoded(hash)?)?;
                            }
                        })
                    })
                    .collect();
                for w in workers {
                    w.join().expect("upload worker panicked")?;
                }
                Ok(())
            })?;
            pending.len()
        }
        Remote::Opendal { store: remote_store, rt } => rt.block_on(async {
            // One paginated listing beats a HeadObject per chunk.
            let present: HashSet<blake3::Hash> =
                remote_store.list().await?.into_iter().map(|(h, _)| h).collect();
            let pending = pending_uploads(&chunks, &present);
            for hash in &pending {
                let data = store.get_encoded(hash).with_context(|| {
                    format!("remote needs {} but it is not in the local store — run `git cdc pull` first (a dropped chunk from `git cdc drop --chunks` needs refetching)", oid_str(hash))
                })?;
                remote_store.put_encoded(hash, data).await?;
            }
            anyhow::Ok(pending.len())
        })?,
        Remote::Ssh(mut ssh) => {
            // Same shape as the opendal remote: one listing, then upload the diff.
            let present: HashSet<blake3::Hash> =
                ssh.list()?.into_iter().map(|(h, _)| h).collect();
            let pending = pending_uploads(&chunks, &present);
            for hash in &pending {
                let data = store.get_encoded(hash).with_context(|| {
                    format!(
                        "ssh remote needs {} but it is not in the local store — run `git cdc pull` first (a dropped chunk from `git cdc drop --chunks` needs refetching)",
                        oid_str(hash)
                    )
                })?;
                ssh.put_encoded(hash, &data)?;
            }
            pending.len()
        }
    };
    eprintln!(
        "git-cdc: pushed {uploaded} of {total} chunks ({} already remote)",
        total - uploaded
    );
    Ok(())
}

pub(crate) fn fetch_legacy(chunks: &HashMap<blake3::Hash, u64>) -> Result<()> {
    let store = local_store()?;
    let missing: HashMap<_, _> = chunks
        .iter()
        .filter(|(hash, _)| !store.has(hash))
        .map(|(hash, size)| (*hash, *size))
        .collect();
    if !missing.is_empty() {
        match remote()? {
            Remote::Http(client) => {
                let objects = missing
                    .iter()
                    .map(|(h, size)| ObjectSpec {
                        oid: oid_str(h),
                        size: *size,
                    })
                    .collect();
                let resp = client.batch(Operation::Download, objects)?;
                for obj in &resp.objects {
                    if let Some(err) = &obj.error {
                        bail!(
                            "server cannot provide {}: {} {}",
                            obj.oid,
                            err.code,
                            err.message
                        );
                    }
                    let href = &obj
                        .actions
                        .as_ref()
                        .and_then(|a| a.download.as_ref())
                        .with_context(|| format!("no download action for {}", obj.oid))?
                        .href;
                    let hash = git_cdc_core::manifest::parse_hash(&obj.oid)?;
                    let data = client.download(href)?;
                    store.put_encoded(&hash, &data)?; // decodes envelope, verifies hash
                }
            }
            Remote::Opendal {
                store: remote_store,
                rt,
            } => rt.block_on(async {
                for hash in missing.keys() {
                    let data = remote_store.get_encoded(hash).await?;
                    store.put_encoded(hash, &data)?; // decodes envelope, verifies hash
                }
                anyhow::Ok(())
            })?,
            Remote::Ssh(mut ssh) => {
                for hash in missing.keys() {
                    let data = ssh.get_encoded(hash)?;
                    store.put_encoded(hash, &data)?; // decodes envelope, verifies hash
                }
            }
        }
        eprintln!("git-cdc: fetched {} chunks", missing.len());
    }

    Ok(())
}

pub fn cmd_gc(dry_run: bool, grace_secs: u64) -> Result<()> {
    let store = local_store()?;
    let grace = std::time::Duration::from_secs(grace_secs);
    let live: std::collections::HashSet<blake3::Hash> = all_manifests()?
        .iter()
        .flat_map(|m| m.chunks.iter().map(|c| c.hash))
        .collect();

    // Local sweep: same mark-and-sweep + grace rule the server applies.
    let now = SystemTime::now();
    let mut swept = 0usize;
    for hash in store.list()? {
        let modified = fs::metadata(store.path_for(&hash))
            .and_then(|md| md.modified())
            .ok();
        if !matches!(
            sweep_decision(live.contains(&hash), modified, grace, now),
            SweepAction::Delete
        ) {
            continue;
        }
        if dry_run {
            eprintln!("would remove local {}", oid_str(&hash));
        } else {
            store.remove(&hash)?;
        }
        swept += 1;
    }
    eprintln!(
        "git-cdc: local gc {} {swept} unreferenced chunks ({} live)",
        if dry_run { "would remove" } else { "removed" },
        live.len()
    );

    // Remote sweep, if a remote is configured.
    match remote() {
        Ok(Remote::Http(client)) => {
            // Server owns the remote grace period (its --grace-secs).
            let resp = client.gc(live.iter().map(oid_str).collect(), dry_run)?;
            eprintln!(
                "git-cdc: remote gc {} {} chunks ({} live, {} in grace period)",
                if dry_run { "would remove" } else { "removed" },
                resp.deleted.len(),
                resp.kept_live,
                resp.kept_grace
            );
        }
        Ok(Remote::Opendal {
            store: remote_store,
            rt,
        }) => {
            // Serverless: no server to own the sweep — the CLI's grace applies.
            let (deleted, kept_live, kept_grace) = rt.block_on(async {
                let (mut deleted, mut kept_live, mut kept_grace) = (0usize, 0usize, 0usize);
                for (hash, modified) in remote_store.list().await? {
                    match sweep_decision(live.contains(&hash), modified, grace, now) {
                        SweepAction::KeepLive => kept_live += 1,
                        SweepAction::KeepGrace => kept_grace += 1,
                        SweepAction::Delete => {
                            if !dry_run {
                                remote_store.remove(&hash).await?;
                            }
                            deleted += 1;
                        }
                    }
                }
                anyhow::Ok((deleted, kept_live, kept_grace))
            })?;
            eprintln!(
                "git-cdc: remote gc {} {deleted} chunks ({kept_live} live, {kept_grace} in grace period)",
                if dry_run { "would remove" } else { "removed" },
            );
        }
        Ok(Remote::Ssh(mut ssh)) => {
            // Like the opendal remote: no server owns the sweep — CLI grace + remote mtimes.
            let (mut deleted, mut kept_live, mut kept_grace) = (0usize, 0usize, 0usize);
            for (hash, modified) in ssh.list()? {
                match sweep_decision(live.contains(&hash), modified, grace, now) {
                    SweepAction::KeepLive => kept_live += 1,
                    SweepAction::KeepGrace => kept_grace += 1,
                    SweepAction::Delete => {
                        if !dry_run {
                            ssh.remove(&hash)?;
                        }
                        deleted += 1;
                    }
                }
            }
            eprintln!(
                "git-cdc: ssh gc {} {deleted} chunks ({kept_live} live, {kept_grace} in grace period)",
                if dry_run { "would remove" } else { "removed" },
            );
        }
        Err(_) => eprintln!("git-cdc: no remote configured, skipped remote gc"),
    }
    Ok(())
}
