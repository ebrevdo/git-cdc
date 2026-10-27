//! SSH-transport e2e: the full track → push → clone → pull → gc cycle over
//! the stdio protocol. `cdc.ssh.command` runs the stdio server as a local
//! subprocess — the identical code path real `ssh <host> git-cdc stdio`
//! takes, minus the network hop.

use std::fs;
use std::path::Path;

use git_cdc_core::chunker::test_util::test_data;
use git_cdc_core::store::DiskStore;

mod utils;

use utils::{BIN, cdc, git};

fn setup_repo(repo: &Path, remote_root: &Path) {
    utils::base_setup_repo(repo);
    git(
        repo,
        &[
            "config",
            "cdc.ssh.command",
            &format!("{BIN} stdio --root {}", remote_root.display()),
        ],
    );
}

fn remote_chunk_count(root: &Path) -> usize {
    DiskStore::new(root).list().unwrap().len()
}

#[test]
fn ssh_push_clone_pull_gc() {
    let tmp = tempfile::tempdir().unwrap();
    let remote_root = tmp.path().join("ssh-chunks");

    // origin: v1 + v2, push over the stdio transport.
    let repo = tmp.path().join("origin");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    setup_repo(&repo, &remote_root);
    cdc(&repo, &["track", "*.bin"]);

    let mut data = test_data(12 * 1024 * 1024, 31);
    fs::write(repo.join("asset.bin"), &data).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "v1"]);
    cdc(&repo, &["push"]);
    let count_v1 = remote_chunk_count(&remote_root);
    assert!(count_v1 >= 2, "12 MiB should be several chunks");

    data[6_000_000] ^= 0xFF;
    fs::write(repo.join("asset.bin"), &data).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "v2"]);
    let log = cdc(&repo, &["push"]);
    assert!(
        remote_chunk_count(&remote_root) - count_v1 <= 2,
        "1-byte edit should upload few chunks ({log})"
    );

    // fresh clone via bare remote: passthrough, then pull over ssh.
    let bare = tmp.path().join("remote.git");
    git(
        tmp.path(),
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--bare",
            &bare.to_string_lossy(),
        ],
    );
    git(&repo, &["remote", "add", "origin", &bare.to_string_lossy()]);
    git(&repo, &["push", "-q", "origin", "main"]);

    let clone = tmp.path().join("clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            &bare.to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    );
    setup_repo(&clone, &remote_root);
    assert!(
        fs::read(clone.join("asset.bin"))
            .unwrap()
            .starts_with(b"version git-cdc/spec/v1\n")
    );
    cdc(&clone, &["pull"]);
    assert_eq!(
        fs::read(clone.join("asset.bin")).unwrap(),
        data,
        "v2 materialized over ssh"
    );

    // gc: drop v2 everywhere; the ssh sweep removes its unique chunks.
    git(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    git(&repo, &["push", "-q", "--force", "origin", "main"]);
    let before = remote_chunk_count(&remote_root);
    cdc(&repo, &["gc", "--dry-run", "--grace-secs", "0"]);
    assert_eq!(
        remote_chunk_count(&remote_root),
        before,
        "dry run deletes nothing"
    );
    cdc(&repo, &["gc", "--grace-secs", "0"]);
    assert_eq!(
        remote_chunk_count(&remote_root),
        count_v1,
        "remote back to the v1 chunk set"
    );
}

#[test]
fn compressible_content_stores_smaller_than_raw() {
    let tmp = tempfile::tempdir().unwrap();
    let remote_root = tmp.path().join("ssh-chunks");
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    setup_repo(&repo, &remote_root);
    cdc(&repo, &["track", "*.bin"]);

    // Highly compressible: repeated text.
    let raw: Vec<u8> = b"the same line of text over and over\n"
        .iter()
        .cycle()
        .take(8 * 1024 * 1024)
        .copied()
        .collect();
    fs::write(repo.join("log.bin"), &raw).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "log"]);
    cdc(&repo, &["push"]);

    let stored: u64 = walk_size(&remote_root);
    assert!(
        stored < raw.len() as u64 / 10,
        "8 MiB of repeated text should store < 10% of raw, got {stored}"
    );

    // And it still round-trips byte-identically.
    fs::remove_dir_all(repo.join(".git/cdc")).unwrap();
    fs::remove_file(repo.join("log.bin")).unwrap();
    git(&repo, &["checkout", "--", "log.bin"]);
    cdc(&repo, &["pull"]);
    assert_eq!(fs::read(repo.join("log.bin")).unwrap(), raw);
}

fn walk_size(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            total += walk_size(&path);
        } else {
            total += path.metadata().unwrap().len();
        }
    }
    total
}

#[cfg(unix)]
#[test]
fn gc_pipelines_deletes_while_draining_remote_replies() {
    use git_cdc_core::pktline::{write_flush, write_text};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    utils::base_setup_repo(&repo);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "empty live set"],
    );
    let listing = tmp.path().join("listing");
    let mut output = fs::File::create(&listing).unwrap();
    for i in 0u64..4096 {
        write_text(
            &mut output,
            &format!("chunk {} 1", blake3::hash(&i.to_le_bytes()).to_hex()),
        )
        .unwrap();
    }
    write_flush(&mut output).unwrap();
    let peer = tmp.path().join("peer.sh");
    fs::write(
        &peer,
        r#"set -eu
read_packet() {
    IFS= read -r -N 4 prefix
    IFS= read -r -N "$((16#$prefix - 4))" payload
}
read_packet
printf '0007ok\n'
read_packet
cat "$1"
# Reply in windows large enough to fill a pipe. A lockstep client cannot
# reach the first reply, and a write-all-then-read client fills both pipes.
for ((i = 0; i < 4096; i++)); do
    read_packet
    [[ "$payload" == remove\ * ]]
    if ((i % 1024 == 1023)); then
        for ((j = 0; j < 1024; j++)); do printf '0007ok\n'; done
    fi
done
"#,
    )
    .unwrap();
    git(
        &repo,
        &[
            "config",
            "cdc.ssh.command",
            &format!("bash {} {}", peer.display(), listing.display()),
        ],
    );
    let mut child = Command::new(BIN)
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["gc", "--grace-secs", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("SSH removal pipeline stalled");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("ssh gc removed 4096 chunks"));
}
