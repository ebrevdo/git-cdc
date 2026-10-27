//! Disposable metadata for one complete history scan. This caches recipe
//! validation only. Incremental pushes scan their destination's object difference.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::git::input_file;

use super::transfer::{Object, add_object};
use super::{
    Store, chunk_oid, git_out, git_without_replacements, is_git_oid, is_sha256, sha256, trace,
};

const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
// Earlier snapshots may have been populated under grafted reachability.
const CACHE_VERSION: u64 = 2;

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
        ensure!(
            envelope["version"] == CACHE_VERSION,
            "unknown history cache version"
        );
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
        // Reuse requires every cached commit tip to remain reachable from a
        // pushed root. Check their set difference in one walk; missing or
        // non-commit cached tips conservatively disable reuse.
        let mut input = Vec::new();
        for previous in &self.tips {
            writeln!(input, "{previous}^{{commit}}")?;
        }
        for tip in tips {
            writeln!(input, "^{tip}")?;
        }
        let output = git_without_replacements()
            .args(["rev-list", "--max-count=1", "--stdin"])
            .stdin(input_file(&input)?)
            .stderr(Stdio::null())
            .output()?;
        Ok(output.status.success() && output.stdout.is_empty())
    }

    fn write(&self, path: &Path) -> Result<()> {
        let payload = json!({
            "tips": self.tips,
            "objects": self.objects.iter().map(|(oid, object)|
                json!([oid, object.size, object.hash.unwrap().to_hex().as_str()]))
                .collect::<Vec<_>>()
        })
        .to_string();
        let envelope = json!({
            "version": CACHE_VERSION,
            "sha256": sha256(payload.as_bytes()),
            "payload": payload,
        });
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
    let path = Store::new()?
        .root
        .join(format!("history-v{CACHE_VERSION}.json"));
    // Grafts change ancestry without changing tip IDs. Resolve the shared
    // path for linked worktrees, and conservatively honor any override.
    let grafts = git_out(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "info/grafts",
    ])?;
    let cacheable = !Path::new(&grafts).exists() && std::env::var_os("GIT_GRAFT_FILE").is_none();
    let previous = if cacheable {
        Snapshot::read(&path).ok()
    } else {
        None
    };
    let previous = match previous {
        Some(previous) if previous.covers_only_ancestors_of(tips)? => Some(previous),
        _ => None,
    };
    let unchanged = previous
        .as_ref()
        .is_some_and(|snapshot| snapshot.tips == *tips);
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
    if cacheable
        && !unchanged
        && let Err(error) = snapshot.write(&path)
    {
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

pub(super) fn collect_new(
    tips: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
) -> Result<BTreeMap<String, Object>> {
    let started = Instant::now();
    // A full-history snapshot cannot establish which recipes are new to this
    // destination. Scan the difference without loading historical chunk lists
    // or replacing the complete snapshot with an incomplete one.
    let mut objects = BTreeMap::new();
    super::visit_manifests(tips, excluded, |manifest| {
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
    trace(
        "history",
        started,
        format_args!(
            "{} chunks from new recipes, {} excluded tips",
            objects.len(),
            excluded.len()
        ),
    );
    Ok(objects)
}
