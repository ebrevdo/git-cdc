//! Standard pointer roots in the same Git repository. Every new immutable
//! root retains the previous roots through Git ancestry, including during a
//! mirror push that can delete previously advertised internal refs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Seek, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use super::{
    LFS_POINTER_VERSION, PushRemote, git_out, git_without_replacements, is_git_oid, is_sha256,
    lfs_pointer, sha256, visit_blobs_with,
};

pub(super) const PREFIX: &str = "refs/notes/git-cdc-lfs/";
pub(super) const ATTRIBUTES: &str = "chunks/** filter=lfs -diff -merge -text\n";

pub(super) fn pointer_path(oid: &str) -> String {
    format!("chunks/{}/{}.lfs", &oid[..2], &oid[2..])
}

pub(super) fn parse_remote_roots(output: &str) -> Result<Vec<(String, String)>> {
    output
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let [oid, reference] = fields.as_slice() else {
                bail!("invalid remote retention ref");
            };
            ensure!(
                reference.starts_with(PREFIX) && is_git_oid(oid),
                "invalid remote retention ref"
            );
            Ok(((*oid).to_owned(), (*reference).to_owned()))
        })
        .collect()
}

pub(super) fn visit_objects(
    tips: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
    visit: impl FnMut(&str, u64) -> Result<()>,
) -> Result<()> {
    visit_pointers_with(tips, excluded, None, visit, git_without_replacements)
}

pub(super) fn visit_candidates_with(
    tips: &BTreeSet<String>,
    wanted: &BTreeMap<&str, u64>,
    visit: impl FnMut(&str, u64) -> Result<()>,
    command: impl FnMut() -> Command,
) -> Result<()> {
    let paths: BTreeSet<_> = wanted.keys().map(|oid| pointer_path(oid)).collect();
    visit_pointers_with(tips, &BTreeSet::new(), Some(&paths), visit, command)
}

fn visit_pointers_with(
    tips: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
    paths: Option<&BTreeSet<String>>,
    mut visit: impl FnMut(&str, u64) -> Result<()>,
    command: impl FnMut() -> Command,
) -> Result<()> {
    visit_blobs_with(
        tips,
        excluded,
        None,
        paths,
        |head| head.starts_with(LFS_POINTER_VERSION.as_bytes()),
        |body| {
            // Our managed roots always contain canonical three-line SHA-256
            // pointers, and their object sizes are bounded by the chunker.
            let mut lines = std::str::from_utf8(body)?.lines();
            ensure!(
                lines.next() == Some(LFS_POINTER_VERSION),
                "invalid retention pointer version"
            );
            let oid = lines
                .next()
                .and_then(|line| line.strip_prefix("oid sha256:"))
                .context("missing retention object id")?;
            let size: u64 = lines
                .next()
                .and_then(|line| line.strip_prefix("size "))
                .context("missing retention object size")?
                .parse()?;
            ensure!(
                is_sha256(oid)
                    && size <= git_cdc_core::chunker::CEILING as u64
                    && body == lfs_pointer(oid, size).as_bytes(),
                "invalid retention pointer"
            );
            visit(oid, size)
        },
        command,
    )
}

fn previous_roots(target: &PushRemote, advertised: &[(String, String)]) -> Result<Vec<String>> {
    // Named remotes can fetch from a different repository than they push to.
    // The hook supplies the destination whose existing roots must survive.
    let refs: BTreeMap<_, _> = advertised.iter().cloned().collect();
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = tempfile::tempfile()?;
    for oid in refs.keys() {
        writeln!(input, "{oid}")?;
    }
    input.rewind()?;
    let mut checked = tempfile::tempfile()?;
    let status = git_without_replacements()
        .args(["cat-file", "--batch-check"])
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(checked.try_clone()?))
        .status()?;
    ensure!(status.success(), "cannot inspect local retention objects");
    checked.rewind()?;
    let mut missing = tempfile::tempfile()?;
    let mut fetch = false;
    for line in BufReader::new(checked).lines() {
        let line = line?;
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.get(1) == Some(&"missing") {
            let name = refs
                .get(fields[0])
                .ok_or_else(|| anyhow::anyhow!("unknown retention object"))?;
            writeln!(missing, "{name}")?;
            fetch = true;
        } else {
            ensure!(
                fields.get(1) == Some(&"commit"),
                "retention ref must point to a commit"
            );
        }
    }
    if fetch {
        missing.rewind()?;
        let status = target
            .command()
            .args([
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--stdin",
                &target.destination,
            ])
            .stdin(Stdio::from(missing))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()?;
        ensure!(
            status.success(),
            "cannot retrieve previous retention metadata"
        );
    }
    // Most refs point to ancestors of the latest roots. Reduce in batches to
    // keep command lines small; concurrent publishers can leave several tips.
    let all: Vec<_> = refs.into_keys().collect();
    let mut frontier = Vec::new();
    for group in all.chunks(128) {
        frontier.extend(group.iter().cloned());
        ensure!(
            frontier.len() <= 4096,
            "too many independent retention roots"
        );
        let out = git_without_replacements()
            .args(["merge-base", "--independent"])
            .args(&frontier)
            .output()?;
        ensure!(out.status.success(), "cannot resolve retention ancestry");
        frontier = String::from_utf8(out.stdout)?
            .lines()
            .map(str::to_owned)
            .collect();
    }
    frontier.sort_unstable();
    Ok(frontier)
}

fn write_blob(output: &mut impl Write, mark: usize, contents: &str) -> Result<()> {
    writeln!(output, "blob\nmark :{mark}\ndata {}", contents.len())?;
    output.write_all(contents.as_bytes())?;
    writeln!(output)?;
    Ok(())
}

pub(super) fn stage(objects: &BTreeMap<String, u64>, parents: &[String]) -> Result<String> {
    stage_with(objects, parents, git_without_replacements())
}

pub(super) fn stage_with(
    objects: &BTreeMap<String, u64>,
    parents: &[String],
    mut command: Command,
) -> Result<String> {
    let staging = tempfile::tempdir()?;
    let temporary = format!(
        "{PREFIX}pending-{}",
        sha256(staging.path().as_os_str().as_encoded_bytes())
    );
    // Fast-import writes standard blobs and trees in one process without a
    // staging index. A unique publication ID keeps each pin outside an outer
    // mirror push's already-negotiated ref updates, even for identical data.
    let mut input = tempfile::tempfile()?;
    for (index, (oid, size)) in objects.iter().enumerate() {
        write_blob(&mut input, index + 1, &lfs_pointer(oid, *size))?;
    }
    let attributes_mark = objects.len() + 1;
    write_blob(&mut input, attributes_mark, ATTRIBUTES)?;
    let message = format!("git-cdc LFS retention v1\n\nPublication: {temporary}\n");
    let commit_mark = attributes_mark + 1;
    writeln!(
        input,
        "commit {temporary}\nmark :{commit_mark}\nauthor git-cdc <git-cdc@localhost> 1 +0000\ncommitter git-cdc <git-cdc@localhost> 1 +0000\ndata {}\n{message}",
        message.len()
    )?;
    if let Some(first) = parents.first() {
        writeln!(input, "from {first}")?;
        for parent in &parents[1..] {
            writeln!(input, "merge {parent}")?;
        }
    }
    writeln!(
        input,
        "deleteall\nM 100644 :{attributes_mark} .gitattributes"
    )?;
    for (index, oid) in objects.keys().enumerate() {
        writeln!(input, "M 100644 :{} {}", index + 1, pointer_path(oid))?;
    }
    // Export the commit through marks, then reset the importer branch before
    // it writes refs. Preparation creates objects only; no temporary ref leaks
    // into another push's snapshot or invokes user hooks under cache leases.
    writeln!(input, "\nreset {temporary}\n\ndone")?;
    input.rewind()?;
    let marks = staging.path().join("marks");
    let mut hooks = std::ffi::OsString::from("core.hooksPath=");
    hooks.push(staging.path());
    let mut export = std::ffi::OsString::from("--export-marks=");
    export.push(&marks);
    let status = command
        .arg("-c")
        .arg(hooks)
        .args(["fast-import", "--quiet", "--done"])
        .arg(export)
        .stdin(Stdio::from(input))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()?;
    ensure!(status.success(), "cannot create LFS retention metadata");
    let mark = format!(":{commit_mark} ");
    std::fs::read_to_string(marks)?
        .lines()
        .find_map(|line| line.strip_prefix(&mark).map(str::to_owned))
        .context("missing retention commit mark")
}

/// Discover remote roots once per push. Checkpoints extend the last successful
/// publication; concurrent publishers keep their own immutable refs reachable.
pub(super) struct Publication<'a> {
    target: &'a PushRemote,
    parents: Vec<String>,
}

impl<'a> Publication<'a> {
    pub fn new(target: &'a PushRemote, advertised: &[(String, String)]) -> Result<Self> {
        let started = std::time::Instant::now();
        let parents = previous_roots(target, advertised)?;
        super::trace(
            "retention discovery",
            started,
            format_args!("{} independent roots", parents.len()),
        );
        Ok(Self { target, parents })
    }

    pub fn publish(&mut self, objects: &BTreeMap<String, u64>) -> Result<()> {
        if objects.is_empty() && self.parents.is_empty() {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let commit = stage(objects, &self.parents)?;
        super::trace(
            "retention preparation",
            started,
            format_args!("{} pointers", objects.len()),
        );
        let started = std::time::Instant::now();
        publish_commit(self.target, &commit)?;
        super::trace(
            "retention publication",
            started,
            format_args!("{} pointers", objects.len()),
        );
        self.parents = vec![commit];
        Ok(())
    }
}

fn publish_commit(target: &PushRemote, commit: &str) -> Result<()> {
    let reference = format!("{PREFIX}{commit}");
    git_out(&["update-ref", &reference, commit])?;
    // The outer operation has already run the user's hook and transferred all
    // required chunks. Bypass only the client's recursive pre-push invocation;
    // preserve server hooks and normal ref policies. Never force-update a pin.
    // Use the destination URL directly: named remotes can fan out to multiple
    // push URLs or configure mirror mode, neither applies to this single pin.
    let status = target
        .command()
        .args([
            "push",
            "--quiet",
            "--no-verify",
            "--",
            &target.destination,
            &format!("{commit}:{reference}"),
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        // A creation race is successful only if the exact same immutable
        // commit won. All other rejections block the outer user ref updates.
        let raced = target
            .command()
            .args(["ls-remote", "--exit-code", &target.destination, &reference])
            .output()?;
        ensure!(
            raced.status.success()
                && String::from_utf8(raced.stdout)?.split_whitespace().next() == Some(commit),
            "cannot publish LFS retention metadata; user refs were not published"
        );
    }
    Ok(())
}
