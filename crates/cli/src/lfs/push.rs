//! Scope publication to objects the destination does not already retain.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Seek, Write};
use std::process::{Command, Stdio};

use anyhow::{Result, bail, ensure};

use super::{PushRemote, git_out, git_without_replacements, is_git_oid, retention};

pub(super) fn full_check() -> Result<bool> {
    let key = "cdc.lfs.fullPush";
    let output = Command::new("git")
        .args(["config", "--type=bool", "--get", key])
        .output()?;
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    ensure!(
        output.status.success(),
        "cannot read {key}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout == b"true\n")
}

pub(super) struct Destination {
    pub excluded: BTreeSet<String>,
    // Full repairs also inspect available retention metadata; its presence
    // alone must never exclude recipes from an ordinary publication.
    pub available: BTreeSet<String>,
    pub roots: Vec<(String, String)>,
}

impl Destination {
    pub fn discover(target: &PushRemote, mut candidates: BTreeSet<String>) -> Result<Self> {
        let started = std::time::Instant::now();
        // Use the actual push URL, never possibly stale tracking refs or a
        // named remote's different fetch URL. Do not fetch missing tips just
        // for an optimization: including extra recipes is always safe.
        let output = target
            .command()
            .args([
                "ls-remote",
                "--refs",
                "--",
                &target.destination,
                "refs/heads/*",
                "refs/tags/*",
            ])
            .arg(format!("{}*", retention::PREFIX))
            .output()?;
        ensure!(
            output.status.success(),
            "cannot discover destination refs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let mut roots = Vec::new();
        for line in std::str::from_utf8(&output.stdout)?.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            let [oid, reference] = fields.as_slice() else {
                bail!("invalid destination ref");
            };
            ensure!(is_git_oid(oid), "invalid destination object id");
            // Provider-managed refs can point to fork commits whose chunks
            // were never published here. Only branches/tags and the hook's
            // supplied old values establish prior publication coverage.
            if reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/") {
                candidates.insert((*oid).to_owned());
            }
            if reference.starts_with(retention::PREFIX) {
                roots.push(((*oid).to_owned(), (*reference).to_owned()));
            }
        }
        let checked_tips: BTreeSet<_> = candidates
            .iter()
            .chain(roots.iter().map(|(oid, _)| oid))
            .collect();
        let mut available = BTreeSet::new();
        if !checked_tips.is_empty() {
            let mut input = tempfile::tempfile()?;
            for oid in checked_tips {
                writeln!(input, "{oid}")?;
            }
            input.rewind()?;
            let mut checked = tempfile::tempfile()?;
            let status = git_without_replacements()
                .args(["cat-file", "--batch-check"])
                .env("GIT_NO_LAZY_FETCH", "1")
                .stdin(Stdio::from(input))
                .stdout(Stdio::from(checked.try_clone()?))
                .status()?;
            ensure!(status.success(), "cannot inspect destination objects");
            checked.rewind()?;
            for line in BufReader::new(checked).lines() {
                let line = line?;
                let fields: Vec<_> = line.split_whitespace().collect();
                match fields.as_slice() {
                    [oid, "missing"] if is_git_oid(oid) => {}
                    [oid, "commit" | "tag" | "tree" | "blob", _] if is_git_oid(oid) => {
                        available.insert((*oid).to_owned());
                    }
                    _ => bail!("unreadable destination object: {line}"),
                }
            }
        }
        let excluded: BTreeSet<_> = available.intersection(&candidates).cloned().collect();
        super::trace(
            "destination refs",
            started,
            format_args!("{} locally available tips", excluded.len()),
        );
        Ok(Self {
            excluded,
            available,
            roots,
        })
    }

    pub fn needs_retention_guard(&self) -> Result<bool> {
        if self.roots.is_empty() {
            return Ok(false);
        }
        // Git can omit mirror deletions from pre-push input. A mirror keeps
        // all remote roots if local retention refs reach them, even when their
        // individual names are absent locally. Check the whole set in one walk.
        let local = git_out(&["for-each-ref", "--format=%(objectname)", retention::PREFIX])?;
        if local.is_empty() {
            return Ok(true);
        }
        let mut input = tempfile::tempfile()?;
        for (oid, _) in &self.roots {
            writeln!(input, "{oid}^{{commit}}")?;
        }
        for oid in local.lines() {
            writeln!(input, "^{oid}^{{commit}}")?;
        }
        input.rewind()?;
        let output = git_without_replacements()
            .args(["rev-list", "--max-count=1", "--stdin"])
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::from(input))
            .stderr(Stdio::null())
            .output()?;
        // Missing objects or invalid local tips conservatively require a new
        // root; preparation will retrieve and validate the remote metadata.
        Ok(!output.status.success() || !output.stdout.is_empty())
    }
}
