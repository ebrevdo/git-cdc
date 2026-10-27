//! Disposable metadata for one complete history scan. This caches recipe
//! validation only: every push still negotiates all chunks at its destination.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::transfer::{Object, add_object};
use super::{Store, chunk_oid, git_without_replacements, is_git_oid, is_sha256, sha256, trace};

const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

struct Snapshot {
    tips: BTreeSet<String>,
    objects: BTreeMap<String, Object>,
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
            envelope["sha256"] == sha256(payload.as_bytes()),
            "corrupt history cache"
        );
        let payload: Value = serde_json::from_str(payload)?;
        let mut tips = BTreeSet::new();
        for tip in payload["tips"]
            .as_array()
            .context("missing history cache tips")?
        {
            let tip = tip.as_str().context("invalid history cache tip")?;
            ensure!(is_git_oid(tip), "invalid history cache tip");
            tips.insert(tip.to_owned());
        }
        ensure!(!tips.is_empty(), "empty history cache tips");
        let mut objects = BTreeMap::new();
        for entry in payload["objects"]
            .as_array()
            .context("missing history cache objects")?
        {
            let oid = entry[0].as_str().context("invalid cached chunk id")?;
            let size = entry[1].as_u64().context("invalid cached chunk size")?;
            let hash = entry[2].as_str().context("invalid cached chunk hash")?;
            ensure!(
                is_sha256(oid) && size > 0 && size <= git_cdc_core::chunker::CEILING as u64,
                "invalid cached chunk metadata"
            );
            add_object(&mut objects, oid, size, Some(blake3::Hash::from_hex(hash)?))?;
        }
        Ok(Self { tips, objects })
    }

    fn covers_only_ancestors_of(&self, tips: &BTreeSet<String>) -> Result<bool> {
        for previous in &self.tips {
            let mut ancestor = false;
            for tip in tips {
                // Missing/pruned objects and non-commit tag targets simply
                // disable reuse. A different branch must not inherit extras.
                if git_without_replacements()
                    .args(["merge-base", "--is-ancestor", previous, tip])
                    .stderr(std::process::Stdio::null())
                    .status()?
                    .success()
                {
                    ancestor = true;
                    break;
                }
            }
            if !ancestor {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn write(&self, path: &Path) -> Result<()> {
        let payload = json!({
            "tips": self.tips,
            "objects": self.objects.iter().map(|(oid, object)|
                json!([oid, object.size, object.hash.unwrap().to_hex().as_str()]))
                .collect::<Vec<_>>()
        })
        .to_string();
        let envelope =
            json!({"version": 1, "sha256": sha256(payload.as_bytes()), "payload": payload});
        let directory = path.parent().unwrap();
        fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temporary, &envelope)?;
        temporary.flush()?;
        temporary.persist(path)?;
        Ok(())
    }
}

pub(super) fn collect(tips: &BTreeSet<String>) -> Result<BTreeMap<String, Object>> {
    let started = Instant::now();
    let path = Store::new()?.root.join("history-v1.json");
    let previous = Snapshot::read(&path).ok();
    let previous = match previous {
        Some(previous) if previous.covers_only_ancestors_of(tips)? => Some(previous),
        _ => None,
    };
    let (excluded, mut objects) = previous
        .map(|snapshot| (snapshot.tips, snapshot.objects))
        .unwrap_or_default();
    let reused = objects.len();
    super::visit_manifests(tips, &excluded, |manifest| {
        for chunk in &manifest.chunks {
            add_object(
                &mut objects,
                chunk_oid(&manifest, chunk)?,
                chunk.length as u64,
                Some(chunk.hash),
            )?;
        }
        Ok(())
    })?;
    let snapshot = Snapshot {
        tips: tips.clone(),
        objects,
    };
    // Cache loss, concurrent writers, or an unwritable cache never establishes
    // remote presence and must not prevent a correctly verified publication.
    if let Err(error) = snapshot.write(&path) {
        trace(
            "history cache",
            started,
            format_args!("could not save metadata: {error}"),
        );
    }
    trace(
        "history",
        started,
        format_args!(
            "{} chunks, {reused} reused, {} excluded tips",
            snapshot.objects.len(),
            excluded.len()
        ),
    );
    Ok(snapshot.objects)
}
