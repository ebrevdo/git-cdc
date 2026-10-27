//! Shared e2e harness: run git / git-cdc against a scratch repo. Each test
//! file layers its remote-specific config (http token / s3 / ssh) on top of
//! `base_setup_repo`.

// Compiled once per test binary; not every binary uses every helper.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

pub const BIN: &str = env!("CARGO_BIN_EXE_git-cdc");

/// Run git without asserting success (e.g. to test a blocked push).
/// Hooks invoke `git cdc push` via $PATH — put the freshly built binary first.
pub fn git_cmd(repo: &Path, args: &[&str]) -> std::process::Output {
    let bin_dir = Path::new(BIN).parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap());
    Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("PATH", path)
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap()
}

pub fn git(repo: &Path, args: &[&str]) -> String {
    let out = git_cmd(repo, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run git-cdc without asserting success (e.g. to test a refused drop).
pub fn cdc_cmd(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap()
}

pub fn cdc(repo: &Path, args: &[&str]) -> String {
    let out = cdc_cmd(repo, args);
    assert!(
        out.status.success(),
        "git-cdc {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Identity, hooks, and filter wiring common to every suite. `install`
/// writes `git-cdc clean` expecting the binary on $PATH; tests must run the
/// freshly built binary, so point the filter config at it directly.
pub fn base_setup_repo(repo: &Path) {
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test"]);
    cdc(repo, &["install"]);
    git(
        repo,
        &["config", "filter.cdc.clean", &format!("{BIN} clean %f")],
    );
    git(
        repo,
        &["config", "filter.cdc.smudge", &format!("{BIN} smudge")],
    );
    git(
        repo,
        &[
            "config",
            "filter.cdc.process",
            &format!("{BIN} filter-process"),
        ],
    );
}

/// Run Git subprocesses through a test wrapper while preserving the caller's
/// configured PATH and forwarding ordinary commands to the real executable.
#[cfg(unix)]
pub fn wrap_git(command: &mut Command, directory: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = command
        .get_envs()
        .find(|(key, _)| *key == "PATH")
        .and_then(|(_, value)| value.map(std::ffi::OsStr::to_owned))
        .unwrap_or_else(|| std::env::var_os("PATH").unwrap());
    let real_git = std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrapper_dir = directory.join("git-wrapper");
    std::fs::create_dir_all(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("git");
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    command
        .env(
            "PATH",
            std::env::join_paths(std::iter::once(wrapper_dir).chain(std::env::split_paths(&path)))
                .unwrap(),
        )
        .env("CDC_TEST_REAL_GIT", real_git);
}

/// Record content requests, independently of size/type checks. This verifies
/// history scans avoid reading skipped blobs without measuring elapsed time.
#[cfg(unix)]
pub fn record_blob_requests(
    command: &mut Command,
    directory: &Path,
) -> (std::process::Output, Vec<String>) {
    wrap_git(
        command,
        directory,
        r#"#!/bin/sh
for arg in "$@"; do
    if [ "$CDC_TEST_BATCH_FILES_REQUIRED" = 1 ] && { [ "$arg" = --batch ] || [ "$arg" = --batch-check ]; } && [ -p /dev/stdin ]; then
        echo 'Git object requests must be queued before reading responses' >&2
        exit 91
    fi
    if [ "$arg" = --batch ]; then
        tee -a "$CDC_TEST_REQUESTS" | "$CDC_TEST_REAL_GIT" "$@"
        exit $?
    fi
done
exec "$CDC_TEST_REAL_GIT" "$@"
"#,
    );
    let requests = directory.join("blob-requests");
    let _ = std::fs::remove_file(&requests);
    let output = command
        .env("CDC_TEST_REQUESTS", &requests)
        .output()
        .unwrap();
    let requests = std::fs::read_to_string(requests)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (output, requests)
}

pub fn traced_git_commands(path: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            (event["event"] == "start").then(|| {
                event["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|arg| arg.as_str().unwrap().to_owned())
                    .collect()
            })
        })
        .collect()
}

pub fn cache_write_marker(path: &Path) -> std::time::SystemTime {
    // A fixed old timestamp detects replacement without measuring elapsed time.
    let marker = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_234_567);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(marker))
        .unwrap();
    marker
}
