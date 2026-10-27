//! Backend-independent lifecycle behavior, exercised with the implemented
//! legacy adapter. LFS acceptance contracts live beside the real LFS fixture.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use git_cdc_core::chunker::test_util::test_data;
use git_cdc_core::manifest::Manifest;
use git_cdc_core::store::DiskStore;

mod utils;
use utils::{BIN, cdc, git};

struct Fixture {
    temp: tempfile::TempDir,
    repo: PathBuf,
    remote: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let remote = temp.path().join("objects");
        let git_remote = temp.path().join("origin.git");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&remote).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &["init", "--bare", "-q", git_remote.to_str().unwrap()],
        );
        utils::base_setup_repo(&repo);
        git(
            &repo,
            &["remote", "add", "origin", git_remote.to_str().unwrap()],
        );
        git(&repo, &["config", "cdc.store.scheme", "fs"]);
        git(
            &repo,
            &[
                "config",
                "cdc.store.option",
                &format!("root={}", remote.display()),
            ],
        );
        git(&repo, &["config", "cdc.chunk.min", "64k"]);
        git(&repo, &["config", "cdc.chunk.avg", "256k"]);
        git(&repo, &["config", "cdc.chunk.max", "1m"]);
        cdc(&repo, &["track", "*.bin"]);
        Self { temp, repo, remote }
    }

    fn version(&self, data: &[u8]) -> String {
        fs::write(self.repo.join("asset.bin"), data).unwrap();
        git(&self.repo, &["add", ".gitattributes", "asset.bin"]);
        git(&self.repo, &["commit", "-qm", "version"]);
        git(&self.repo, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    fn publish(&self) {
        cdc(&self.repo, &["push"]);
        git(&self.repo, &["push", "--no-verify", "origin", "main"]);
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args);
        command
    }

    fn traced(&self, args: &[&str], label: &str) -> Vec<Vec<String>> {
        let trace = self.temp.path().join(format!("{label}.jsonl"));
        let out = self
            .command(args)
            .env("GIT_TRACE2_EVENT", &trace)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        utils::traced_git_commands(&trace)
    }

    #[cfg(unix)]
    fn blob_requests(&self, args: &[&str]) -> Vec<String> {
        let mut command = self.command(args);
        let (out, requests) = utils::record_blob_requests(&mut command, self.temp.path());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        requests
    }

    fn chunks(&self) -> HashSet<blake3::Hash> {
        DiskStore::new(self.repo.join(".git/cdc/objects"))
            .list()
            .unwrap()
            .into_iter()
            .collect()
    }

    fn recipe_chunks(&self, revision: &str) -> HashSet<blake3::Hash> {
        Manifest::parse(git(&self.repo, &["show", revision]).as_bytes())
            .unwrap()
            .chunks
            .into_iter()
            .map(|chunk| chunk.hash)
            .collect()
    }
}

#[test]
fn diff_batches_index_and_head_lookups_with_literal_paths() {
    let f = Fixture::new();
    let names = ["a.bin", "space name.bin", "line\nbreak.bin", "café.bin"];
    let data = test_data(128 * 1024, 961);
    for name in names {
        fs::write(f.repo.join(name), &data).unwrap();
    }
    for i in 0..32 {
        fs::write(f.repo.join(format!("note-{i}.txt")), "ordinary content").unwrap();
    }
    git(&f.repo, &["add", "."]);
    // An unborn HEAD reports all indexed chunks as added.
    let out = utils::cdc_cmd(&f.repo, &["diff"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for name in names {
        assert!(text.contains(&format!("{name}:")), "{text}");
    }
    git(&f.repo, &["commit", "-qm", "diff baseline"]);
    for name in names {
        fs::write(f.repo.join(name), test_data(128 * 1024, 963)).unwrap();
    }
    git(&f.repo, &["add", "."]);
    fs::create_dir(f.repo.join("subdir")).unwrap();
    let out = utils::cdc_cmd(&f.repo.join("subdir"), &["diff"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for name in names {
        assert!(text.contains(&format!("{name}:")), "{text}");
    }
    let commands = f.traced(&["diff"], "diff-batches");
    assert_eq!(
        commands
            .iter()
            .filter(|args| args.iter().any(|arg| arg == "check-attr"))
            .count(),
        1
    );
    assert_eq!(
        commands
            .iter()
            .filter(|args| args.iter().any(|arg| arg == "ls-tree"))
            .count(),
        1
    );
    assert!(
        !commands
            .iter()
            .any(|args| args.iter().any(|arg| arg == "show"))
    );
    // Diff retains its working-tree attribute semantics.
    fs::write(f.repo.join(".gitattributes"), "*.bin -filter\n").unwrap();
    let out = utils::cdc_cmd(&f.repo, &["diff"]);
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("no tracked files changed")
    );
}

#[cfg(unix)]
#[test]
fn legacy_push_reuses_history_but_renegotiates_remote_presence() {
    let f = Fixture::new();
    let first = f.version(&test_data(128 * 1024, 965));
    let first_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
    let first_chunks = f.recipe_chunks(":asset.bin");
    // This must never enter the content pipe, even on a cold history scan.
    fs::write(f.repo.join("large.dat"), vec![b'x'; 17 * 1024 * 1024]).unwrap();
    git(&f.repo, &["add", "large.dat"]);
    git(&f.repo, &["commit", "-qm", "ordinary large blob"]);
    let large = git(&f.repo, &["rev-parse", ":large.dat"]).trim().to_owned();
    let requests = f.blob_requests(&["push"]);
    assert!(requests.contains(&first_recipe));
    assert!(!requests.contains(&large));
    let cache = f.repo.join(".git/cdc/history-v1.json");
    let marker = utils::cache_write_marker(&cache);
    assert!(f.blob_requests(&["push"]).is_empty());
    assert_eq!(fs::metadata(&cache).unwrap().modified().unwrap(), marker);

    f.version(&test_data(128 * 1024, 967));
    let second_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
    let second_chunks = f.recipe_chunks(":asset.bin");
    let requests = f.blob_requests(&["push"]);
    assert!(requests.contains(&second_recipe));
    assert!(!requests.contains(&first_recipe));
    assert_ne!(fs::metadata(&cache).unwrap().modified().unwrap(), marker);
    let marker = utils::cache_write_marker(&cache);
    let other_remote = f.temp.path().join("other-objects");
    fs::create_dir(&other_remote).unwrap();
    git(
        &f.repo,
        &[
            "config",
            "cdc.store.option",
            &format!("root={}", other_remote.display()),
        ],
    );
    assert!(f.blob_requests(&["push"]).is_empty());
    assert_eq!(fs::metadata(&cache).unwrap().modified().unwrap(), marker);
    let uploaded: HashSet<_> = fs::read_dir(other_remote.join("chunks"))
        .unwrap()
        .map(|entry| blake3::Hash::from_hex(entry.unwrap().file_name().as_encoded_bytes()).unwrap())
        .collect();
    assert!(first_chunks.is_subset(&uploaded));
    assert!(second_chunks.is_subset(&uploaded));

    fs::write(f.repo.join(".git/cdc/history-v1.json"), "corrupt metadata").unwrap();
    let requests = f.blob_requests(&["push"]);
    assert!(requests.contains(&first_recipe));
    assert!(requests.contains(&second_recipe));
    assert!(!requests.contains(&large));

    // Rewound refs must discard unreachable cached chunks, including ones
    // which no longer exist locally and cannot be uploaded to a new remote.
    git(&f.repo, &["reset", "--hard", &first]);
    let store = DiskStore::new(f.repo.join(".git/cdc/objects"));
    for hash in second_chunks.difference(&first_chunks) {
        store.remove(hash).unwrap();
    }
    fs::remove_dir_all(&other_remote).unwrap();
    fs::create_dir(&other_remote).unwrap();
    let requests = f.blob_requests(&["push"]);
    assert!(requests.contains(&first_recipe));
    assert!(!requests.contains(&second_recipe));
    let uploaded: HashSet<_> = fs::read_dir(other_remote.join("chunks"))
        .unwrap()
        .map(|entry| blake3::Hash::from_hex(entry.unwrap().file_name().as_encoded_bytes()).unwrap())
        .collect();
    assert_eq!(uploaded, first_chunks);
}

#[test]
fn history_cache_checks_changed_refs_in_one_walk() {
    let f = Fixture::new();
    f.version(&test_data(128 * 1024, 995));
    for i in 0..32 {
        git(
            &f.repo,
            &["commit", "--allow-empty", "-qm", "advance history"],
        );
        git(&f.repo, &["branch", &format!("topic-{i}")]);
    }
    cdc(&f.repo, &["push"]);
    f.version(&test_data(128 * 1024, 997));
    for i in 0..32 {
        git(&f.repo, &["branch", "-f", &format!("topic-{i}"), "HEAD"]);
    }
    let commands = f.traced(&["push"], "changed-history-roots");
    assert!(
        !commands
            .iter()
            .any(|args| args.iter().any(|arg| arg == "merge-base"))
    );
    assert_eq!(
        commands
            .iter()
            .filter(|args| args.iter().any(|arg| arg == "--max-count=1"))
            .count(),
        1
    );
    let commands = f.traced(&["push"], "unchanged-history-roots");
    assert!(
        !commands
            .iter()
            .any(|args| args.iter().any(|arg| arg == "--max-count=1"))
    );
}

#[cfg(unix)]
#[test]
fn history_cache_reuses_history_with_blob_and_tree_refs() {
    for target in ["blob", "tree"] {
        let f = Fixture::new();
        f.version(&test_data(128 * 1024, 1003));
        let first_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
        let revision = if target == "blob" {
            "HEAD:asset.bin"
        } else {
            "HEAD^{tree}"
        };
        let object = git(&f.repo, &["rev-parse", revision]).trim().to_owned();
        git(&f.repo, &["update-ref", "refs/tags/retained", &object]);
        cdc(&f.repo, &["push"]);

        let second = f.version(&test_data(128 * 1024, 1005));
        let second_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
        let requests = f.blob_requests(&["push"]);
        assert!(requests.contains(&second_recipe), "{target}");
        assert!(
            !requests.contains(&first_recipe),
            "{target} ref forced a full scan"
        );

        // Newly added blob/tree roots must still contribute recipes that no
        // commit ref reaches; ignoring them would silently omit their chunks.
        f.version(&test_data(128 * 1024, 1007));
        let third_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
        let third_chunks = f.recipe_chunks(":asset.bin");
        let object = git(&f.repo, &["rev-parse", revision]).trim().to_owned();
        git(&f.repo, &["update-ref", "refs/tags/new-root", &object]);
        git(&f.repo, &["reset", "--hard", &second]);
        let requests = f.blob_requests(&["push"]);
        assert!(
            requests.contains(&third_recipe),
            "new {target} root was skipped"
        );
        assert!(!requests.contains(&first_recipe), "{target}");
        assert!(!requests.contains(&second_recipe), "{target}");
        let uploaded: HashSet<_> = fs::read_dir(f.remote.join("chunks"))
            .unwrap()
            .map(|entry| {
                blake3::Hash::from_hex(entry.unwrap().file_name().as_encoded_bytes()).unwrap()
            })
            .collect();
        assert!(third_chunks.is_subset(&uploaded), "{target}");
    }
}

#[cfg(unix)]
#[test]
fn history_cache_discards_missing_and_non_commit_tips() {
    for target in ["blob", "tree", "missing-commit"] {
        let f = Fixture::new();
        let first = f.version(&test_data(128 * 1024, 999));
        let first_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
        let first_chunks = f.recipe_chunks(":asset.bin");
        let second = f.version(&test_data(128 * 1024, 1001));
        let second_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
        let second_chunks = f.recipe_chunks(":asset.bin");
        let old_tip = match target {
            "blob" => second_recipe.clone(),
            "tree" => git(&f.repo, &["rev-parse", "HEAD^{tree}"])
                .trim()
                .to_owned(),
            _ => second.clone(),
        };
        git(&f.repo, &["update-ref", "refs/tags/extra", &old_tip]);
        git(&f.repo, &["reset", "--hard", &first]);
        cdc(&f.repo, &["push"]);

        if target != "missing-commit" {
            let first_tip = if target == "blob" {
                first_recipe.clone()
            } else {
                git(&f.repo, &["rev-parse", "HEAD^{tree}"])
                    .trim()
                    .to_owned()
            };
            git(&f.repo, &["update-ref", "refs/tags/extra", &first_tip]);
        } else {
            git(&f.repo, &["update-ref", "-d", "refs/tags/extra"]);
            git(&f.repo, &["reflog", "expire", "--expire=now", "--all"]);
            git(&f.repo, &["prune", "--expire=now"]);
            assert!(
                !utils::git_cmd(&f.repo, &["cat-file", "-e", &second])
                    .status
                    .success()
            );
        }
        let store = DiskStore::new(f.repo.join(".git/cdc/objects"));
        for hash in second_chunks.difference(&first_chunks) {
            store.remove(hash).unwrap();
        }
        fs::remove_dir_all(&f.remote).unwrap();
        fs::create_dir(&f.remote).unwrap();

        let requests = f.blob_requests(&["push"]);
        assert!(requests.contains(&first_recipe), "{target}");
        assert!(!requests.contains(&second_recipe), "{target}");
        assert_eq!(count_files(&f.remote), first_chunks.len(), "{target}");
    }
}

#[cfg(unix)]
#[test]
fn cold_history_scans_queue_git_requests_in_batches() {
    let f = Fixture::new();
    f.version(&test_data(128 * 1024, 993));
    let recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
    for i in 0..32 {
        fs::write(
            f.repo.join(format!("ordinary-{i}.txt")),
            format!("ordinary content {i}\n"),
        )
        .unwrap();
    }
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "cold history batch fixture"]);
    for args in [&["push"][..], &["gc", "--dry-run"], &["prune", "--dry-run"]] {
        let mut command = f.command(&[]);
        command.env("CDC_TEST_BATCH_FILES_REQUIRED", "1").args(args);
        let (output, requests) = utils::record_blob_requests(&mut command, f.temp.path());
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(requests.contains(&recipe), "{args:?}");
    }
}

#[cfg(unix)]
#[test]
fn legacy_history_cache_covers_detached_linked_worktrees() {
    let f = Fixture::new();
    let first = f.version(&test_data(128 * 1024, 979));
    let first_chunks = f.recipe_chunks(":asset.bin");
    let second = f.version(&test_data(128 * 1024, 981));
    let second_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
    let linked = f.temp.path().join("detached worktree");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            &second,
        ],
    );
    git(&f.repo, &["reset", "--hard", &first]);
    assert!(f.blob_requests(&["push"]).contains(&second_recipe));
    assert!(f.blob_requests(&["push"]).is_empty());
    fs::write(linked.join("asset.bin"), test_data(128 * 1024, 983)).unwrap();
    git(&linked, &["add", "asset.bin"]);
    git(&linked, &["commit", "-qm", "advance detached worktree"]);
    // Legacy chunk stores are per worktree. Publish its new chunks before
    // asking the main worktree to cover that detached history.
    cdc(&linked, &["push"]);
    let third_recipe = git(&linked, &["rev-parse", ":asset.bin"]).trim().to_owned();
    let requests = f.blob_requests(&["push"]);
    assert!(requests.contains(&third_recipe));
    assert!(!requests.contains(&second_recipe));
    git(&f.repo, &["worktree", "remove", linked.to_str().unwrap()]);
    let store = DiskStore::new(f.repo.join(".git/cdc/objects"));
    for hash in f.chunks().difference(&first_chunks) {
        store.remove(hash).unwrap();
    }
    fs::remove_dir_all(&f.remote).unwrap();
    fs::create_dir(&f.remote).unwrap();
    let requests = f.blob_requests(&["push"]);
    assert!(!requests.contains(&second_recipe));
    assert!(!requests.contains(&third_recipe));
    assert_eq!(count_files(&f.remote), first_chunks.len());
}

#[test]
fn installed_legacy_pre_push_hook_caches_history() {
    let f = Fixture::new();
    let first = f.version(&test_data(128 * 1024, 987));
    git(&f.repo, &["push", "origin", "main"]);
    let cache = f.repo.join(".git/cdc/history-v1.json");
    let snapshot: serde_json::Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    assert!(snapshot["payload"].as_str().unwrap().contains(&first));
    let second = f.version(&test_data(128 * 1024, 989));
    git(&f.repo, &["push", "origin", "main"]);
    let snapshot: serde_json::Value = serde_json::from_slice(&fs::read(cache).unwrap()).unwrap();
    assert!(snapshot["payload"].as_str().unwrap().contains(&second));
}

#[test]
fn legacy_push_supports_bare_and_unborn_repositories() {
    let f = Fixture::new();
    cdc(&f.repo, &["push"]);
    f.version(&test_data(128 * 1024, 985));
    cdc(&f.repo, &["push"]);
    let out = f
        .command(&[])
        .env("GIT_DIR", ".git")
        .arg("push")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bare = f.temp.path().join("bare.git");
    git(
        &f.repo,
        &[
            "clone",
            "--bare",
            f.repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(&bare, &["config", "cdc.store.scheme", "fs"]);
    git(
        &bare,
        &[
            "config",
            "cdc.store.option",
            &format!("root={}", f.remote.display()),
        ],
    );
    cdc(&bare, &["push"]);
    assert!(bare.join("cdc/history-v1.json").is_file());
}

#[cfg(unix)]
#[test]
fn legacy_history_cache_is_disabled_for_replacement_objects() {
    let f = Fixture::new();
    let first = f.version(&test_data(128 * 1024, 975));
    let first_recipe = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
    let first_chunks = f.recipe_chunks(":asset.bin");
    let second = f.version(&test_data(128 * 1024, 977));
    let second_chunks = f.recipe_chunks(":asset.bin");
    cdc(&f.repo, &["push"]);
    let cache = f.repo.join(".git/cdc/history-v1.json");
    let metadata = fs::read(&cache).unwrap();
    git(&f.repo, &["replace", &second, &first]);
    let store = DiskStore::new(f.repo.join(".git/cdc/objects"));
    for hash in second_chunks.difference(&first_chunks) {
        store.remove(hash).unwrap();
    }
    fs::remove_dir_all(&f.remote).unwrap();
    fs::create_dir(&f.remote).unwrap();
    let requests = f.blob_requests(&["push"]);
    assert!(requests.contains(&first_recipe));
    assert_eq!(fs::read(cache).unwrap(), metadata);
    assert_eq!(count_files(&f.remote), first_chunks.len());
}

#[cfg(unix)]
#[test]
fn prune_reads_each_historical_blob_once_across_protection_sets() {
    let f = Fixture::new();
    f.version(&test_data(128 * 1024, 969));
    f.publish();
    f.version(&test_data(128 * 1024, 971));
    f.publish();
    git(&f.repo, &["switch", "-c", "unpublished"]);
    f.version(&test_data(128 * 1024, 973));
    let requests = f.blob_requests(&["prune", "--dry-run"]);
    let current = git(&f.repo, &["rev-parse", ":asset.bin"]).trim().to_owned();
    // Once from the index and once from history; HEAD and unpushed-history
    // protection reuse the parsed recipe from the first history walk.
    assert_eq!(requests.iter().filter(|oid| **oid == current).count(), 2);
    let historical: Vec<_> = requests.iter().filter(|oid| **oid != current).collect();
    assert_eq!(
        historical.iter().collect::<HashSet<_>>().len(),
        historical.len()
    );
}

#[test]
fn worktree_updates_batch_git_queries_and_noop_pull_needs_no_chunks() {
    let f = Fixture::new();
    let mut content = Vec::new();
    for name in [
        "a.bin",
        "space name.bin",
        "line\nbreak.bin",
        "caf\u{e9}.bin",
    ] {
        let data = test_data(128 * 1024, 901 + content.len() as u64 * 2);
        fs::write(f.repo.join(name), &data).unwrap();
        content.push((name, data));
    }
    for i in 0..32 {
        fs::write(f.repo.join(format!("note-{i}.txt")), "ordinary content\n").unwrap();
    }
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "batch lifecycle fixture"]);
    let index = git(&f.repo, &["ls-files", "--stage", "-z"]);
    for (args, label) in [(&["drop", "--all"][..], "drop"), (&["pull"][..], "pull")] {
        let commands = f.traced(args, label);
        assert_eq!(
            commands
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "check-attr"))
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "update-index"))
                .count(),
            2
        );
        assert_eq!(
            commands
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "--refresh"))
                .count(),
            1
        );
        assert_eq!(git(&f.repo, &["ls-files", "--stage", "-z"]), index);
    }
    for (name, data) in &content {
        assert_eq!(fs::read(f.repo.join(name)).unwrap(), *data);
    }

    // Neither intact nor edited worktree content needs its cached chunks.
    fs::remove_dir_all(f.repo.join(".git/cdc/objects")).unwrap();
    fs::remove_dir_all(&f.remote).unwrap();
    content[0].1[0] ^= 0xff;
    fs::write(f.repo.join(content[0].0), &content[0].1).unwrap();
    let commands = f.traced(&["pull"], "noop");
    assert_eq!(
        commands
            .iter()
            .filter(|args| args.iter().any(|arg| arg == "check-attr"))
            .count(),
        1
    );
    assert!(
        !commands
            .iter()
            .any(|args| args.iter().any(|arg| arg == "update-index"))
    );
    assert!(!f.repo.join(".git/cdc/objects").exists());
    for (name, data) in &content {
        assert_eq!(fs::read(f.repo.join(name)).unwrap(), *data);
    }
    assert_eq!(git(&f.repo, &["ls-files", "--stage", "-z"]), index);
}

#[test]
fn pull_respects_existing_index_locks_and_alternate_indexes() {
    let f = Fixture::new();
    let data = test_data(128 * 1024, 913);
    f.version(&data);
    cdc(&f.repo, &["drop", "--all"]);
    let recipe = fs::read(f.repo.join("asset.bin")).unwrap();
    let original_index = fs::read(f.repo.join(".git/index")).unwrap();
    let lock = f.repo.join(".git/index.lock");
    fs::write(&lock, "another writer owns this lock").unwrap();
    let out = utils::cdc_cmd(&f.repo, &["pull"]);
    assert!(!out.status.success());
    assert_eq!(fs::read(&lock).unwrap(), b"another writer owns this lock");
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), recipe);
    assert_eq!(fs::read(f.repo.join(".git/index")).unwrap(), original_index);
    fs::remove_file(lock).unwrap();

    let alternate = f.temp.path().join("alternate index ");
    fs::write(&alternate, &original_index).unwrap();
    let out = f
        .command(&[])
        .env("GIT_INDEX_FILE", &alternate)
        .arg("pull")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
    assert_eq!(fs::read(f.repo.join(".git/index")).unwrap(), original_index);
    let staged = utils::git_cmd(&f.repo, &["show", ":asset.bin"]);
    assert_eq!(staged.stdout, recipe);
    let mut lock = alternate.as_os_str().to_os_string();
    lock.push(".lock");
    assert!(!std::path::Path::new(&lock).exists());
}

#[test]
fn unmatched_pathspecs_abort_before_materializing_any_file() {
    let f = Fixture::new();
    f.version(b"original content\n");
    cdc(&f.repo, &["drop", "--all"]);
    let recipe = fs::read(f.repo.join("asset.bin")).unwrap();
    let out = utils::cdc_cmd(&f.repo, &["pull", "--", "asset.bin", "missing.bin"]);
    assert!(!out.status.success());
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), recipe);
    assert_eq!(
        utils::git_cmd(&f.repo, &["show", ":asset.bin"]).stdout,
        recipe
    );
}

#[cfg(unix)]
#[test]
fn interrupted_pull_and_drop_release_only_their_index_lock() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    fn wait_until(context: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {context}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    for operation in ["pull", "drop"] {
        for phase in ["locked", "refresh", "refresh-exit", "released"] {
            if operation == "drop" && phase == "released" {
                continue;
            }
            for (signal, number) in [("HUP", 1), ("INT", 2), ("TERM", 15)] {
                let context = format!("{operation}/{phase}/{signal}");
                let f = Fixture::new();
                let data = test_data(128 * 1024, 991);
                f.version(&data);
                if operation == "pull" {
                    cdc(&f.repo, &["drop", "--all"]);
                }
                let index = fs::read(f.repo.join(".git/index")).unwrap();
                let recipe = utils::git_cmd(&f.repo, &["show", ":asset.bin"]).stdout;
                let ready = f.temp.path().join("ready");
                let release = f.temp.path().join("release");
                let done = f.temp.path().join("done");
                let lock = f.repo.join(".git/index.lock");
                let mut command = f.command(&[operation]);
                utils::wrap_git(
                    &mut command,
                    f.temp.path(),
                    r#"#!/bin/sh
pause=false
for arg in "$@"; do
    case "$CDC_TEST_PHASE:$arg" in
        locked:ls-files) if [ -f "$CDC_TEST_LOCK" ]; then pause=true; fi ;;
        refresh:--refresh|refresh-exit:--refresh|released:cdc.chunk.min) pause=true ;;
    esac
done
if "$pause"; then
    : > "$CDC_TEST_READY"
    while [ ! -f "$CDC_TEST_RELEASE" ]; do sleep 0.01; done
    "$CDC_TEST_REAL_GIT" "$@"
    status=$?
    : > "$CDC_TEST_DONE"
    exit "$status"
fi
exec "$CDC_TEST_REAL_GIT" "$@"
"#,
                );
                command
                    .env("CDC_TEST_PHASE", phase)
                    .env("CDC_TEST_LOCK", &lock)
                    .env("CDC_TEST_READY", &ready)
                    .env("CDC_TEST_RELEASE", &release)
                    .env("CDC_TEST_DONE", &done)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if operation == "drop" {
                    command.arg("--all");
                }
                let mut child = command.spawn().unwrap();
                wait_until(&context, || ready.exists());
                if phase == "released" {
                    assert!(!lock.exists());
                    fs::write(&lock, "another writer owns this lock").unwrap();
                } else {
                    assert!(lock.exists());
                }
                assert!(
                    Command::new("kill")
                        .args(["-s", signal, &child.id().to_string()])
                        .status()
                        .unwrap()
                        .success()
                );
                if phase == "refresh-exit" {
                    fs::write(&release, "race refresh completion against signal delivery").unwrap();
                }
                wait_until(&context, || child.try_wait().unwrap().is_some());
                let status = child.wait().unwrap();
                assert_eq!(
                    status.signal(),
                    Some(number),
                    "{operation}/{phase}/{signal}"
                );
                if phase == "released" {
                    assert_eq!(fs::read(&lock).unwrap(), b"another writer owns this lock");
                    fs::remove_file(&lock).unwrap();
                } else {
                    assert!(!lock.exists(), "signal left the index locked");
                }
                if phase == "locked" || phase == "refresh" {
                    assert_eq!(fs::read(f.repo.join(".git/index")).unwrap(), index);
                }
                if phase == "locked" {
                    assert!(fs::read_dir(f.repo.join(".git")).unwrap().all(|entry| {
                        !entry
                            .unwrap()
                            .file_name()
                            .as_encoded_bytes()
                            .starts_with(b".git-cdc-index-")
                    }));
                }
                fs::write(&release, "resume the surviving Git child").unwrap();
                wait_until(&context, || done.exists());
                assert!(!lock.exists(), "a surviving Git child recreated index.lock");
                assert_eq!(
                    utils::git_cmd(&f.repo, &["show", ":asset.bin"]).stdout,
                    recipe
                );
                // An ordinary Git write must work immediately after interruption.
                git(&f.repo, &["add", "asset.bin"]);
                assert_eq!(
                    utils::git_cmd(&f.repo, &["show", ":asset.bin"]).stdout,
                    recipe
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn pull_rejects_concurrent_index_and_worktree_changes() {
    for change in ["index", "worktree"] {
        let f = Fixture::new();
        f.version(&test_data(128 * 1024, 951));
        cdc(&f.repo, &["drop", "--all"]);
        let recipe = fs::read(f.repo.join("asset.bin")).unwrap();
        let index = fs::read(f.repo.join(".git/index")).unwrap();
        f.version(&test_data(128 * 1024, 953));
        let new_recipe = utils::git_cmd(&f.repo, &["show", ":asset.bin"]).stdout;
        let new_oid = git(&f.repo, &["rev-parse", ":asset.bin"]);
        fs::write(f.repo.join(".git/index"), &index).unwrap();
        fs::write(f.repo.join("asset.bin"), &recipe).unwrap();

        let mut command = f.command(&["pull"]);
        utils::wrap_git(
            &mut command,
            f.temp.path(),
            r#"#!/bin/sh
for arg in "$@"; do
    if [ "$CDC_TEST_CHANGE" = index ] && [ "$arg" = --batch ]; then
        "$CDC_TEST_REAL_GIT" update-index --cacheinfo 100644 "$CDC_TEST_NEW_OID" asset.bin || exit
    fi
    if [ "$CDC_TEST_CHANGE" = worktree ] && [ "$arg" = ls-files ] && [ -f "$CDC_TEST_LOCK" ]; then
        printf '%s' 'concurrent user edits' > "$CDC_TEST_FILE"
    fi
done
exec "$CDC_TEST_REAL_GIT" "$@"
"#,
        );
        let out = command
            .env("CDC_TEST_CHANGE", change)
            .env("CDC_TEST_NEW_OID", new_oid.trim())
            .env("CDC_TEST_LOCK", f.repo.join(".git/index.lock"))
            .env("CDC_TEST_FILE", f.repo.join("asset.bin"))
            .output()
            .unwrap();
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        let (expected_file, expected_recipe, expected_error) = if change == "index" {
            (recipe.as_slice(), new_recipe.as_slice(), "index changed")
        } else {
            (
                &b"concurrent user edits"[..],
                recipe.as_slice(),
                "working file changed",
            )
        };
        assert!(error.contains(expected_error), "{error}");
        assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), expected_file);
        assert_eq!(
            utils::git_cmd(&f.repo, &["show", ":asset.bin"]).stdout,
            expected_recipe
        );
        assert!(!f.repo.join(".git/index.lock").exists());
    }
}

#[test]
fn prefix_content_in_history_and_index_does_not_block_push_drop_or_diff() {
    let f = Fixture::new();
    f.version(b"tracked content\n");
    let data = b"version git-cdc/spec/v1\nthis is ordinary data\n";
    fs::write(f.repo.join("notes.txt"), data).unwrap();
    fs::write(f.repo.join("raw.bin"), data).unwrap();
    git(
        &f.repo,
        &[
            "-c",
            "filter.cdc.process=",
            "-c",
            "filter.cdc.clean=cat",
            "add",
            "notes.txt",
            "raw.bin",
        ],
    );
    git(&f.repo, &["commit", "-qm", "old raw content"]);
    fs::remove_file(f.repo.join("raw.bin")).unwrap();
    git(&f.repo, &["checkout", "--", "raw.bin"]);
    assert_eq!(fs::read(f.repo.join("raw.bin")).unwrap(), data);
    f.publish();
    cdc(&f.repo, &["drop", "--all"]);
    assert_eq!(fs::read(f.repo.join("raw.bin")).unwrap(), data);
    git(&f.repo, &["add", "raw.bin"]);
    cdc(&f.repo, &["diff"]);
    cdc(&f.repo, &["prune", "--dry-run"]);
}

#[test]
fn push_skips_plain_git_version_prefixed_blobs_and_drop_readds_the_same_recipe() {
    let f = Fixture::new();
    f.version(b"tracked content\n");
    let recipe = git(&f.repo, &["show", ":asset.bin"]);
    for (name, size) in [("notes.txt", 64), ("large.txt", 16 * 1024 * 1024 + 1)] {
        let mut data = b"version git-cdc/spec/v1\nthis is not a manifest\n".to_vec();
        data.resize(size, b'x');
        fs::write(f.repo.join(name), data).unwrap();
        git(&f.repo, &["add", name]);
    }
    git(&f.repo, &["commit", "-qm", "ordinary Git content"]);
    f.publish();
    cdc(&f.repo, &["gc", "--dry-run"]);
    cdc(&f.repo, &["drop", "asset.bin"]);
    assert_eq!(
        fs::read(f.repo.join("asset.bin")).unwrap(),
        recipe.as_bytes()
    );
    git(&f.repo, &["add", "asset.bin"]);
    assert_eq!(git(&f.repo, &["show", ":asset.bin"]), recipe);
}

#[test]
fn selected_drop_and_pull_preserve_staged_snapshot_mode_and_newline_path() {
    let f = Fixture::new();
    let name = "sub/line\nbreak.bin";
    fs::create_dir(f.repo.join("sub")).unwrap();
    fs::write(f.repo.join(name), b"committed contents\n").unwrap();
    fs::write(f.repo.join("other.bin"), b"original sibling\n").unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "base"]);
    f.publish();
    let staged = test_data(2 * 1024 * 1024, 801);
    fs::write(f.repo.join(name), &staged).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(f.repo.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&f.repo, &["add", "--", name]);
    fs::write(f.repo.join("other.bin"), b"unstaged sibling edits\n").unwrap();
    let index = git(&f.repo, &["ls-files", "--stage", "-z"]);
    let staged_diff = git(&f.repo, &["diff", "--cached", "--raw"]);
    let recipe = git(&f.repo, &["show", &format!(":{name}")]);

    cdc(&f.repo, &["drop", "--dry-run", "--all"]);
    assert_eq!(fs::read(f.repo.join(name)).unwrap(), staged);
    cdc(&f.repo, &["drop", "--all"]);
    assert_eq!(fs::read(f.repo.join(name)).unwrap(), recipe.as_bytes());
    git(&f.repo, &["add", "--", name]);
    assert_eq!(git(&f.repo, &["show", &format!(":{name}")]), recipe);
    assert_eq!(git(&f.repo, &["ls-files", "--stage", "-z"]), index);
    assert_eq!(git(&f.repo, &["diff", "--cached", "--raw"]), staged_diff);
    assert_eq!(
        fs::read(f.repo.join("other.bin")).unwrap(),
        b"unstaged sibling edits\n"
    );

    cdc(&f.repo.join("sub"), &["pull", "--", "line\nbreak.bin"]);
    assert_eq!(fs::read(f.repo.join(name)).unwrap(), staged);
    assert_eq!(git(&f.repo, &["ls-files", "--stage", "-z"]), index);
    assert_eq!(git(&f.repo, &["diff", "--cached", "--raw"]), staged_diff);
    assert_eq!(
        fs::read(f.repo.join("other.bin")).unwrap(),
        b"unstaged sibling edits\n"
    );
}

#[test]
fn prune_reclaims_published_history_but_keeps_staged_stashed_and_unpushed_objects() {
    let f = Fixture::new();
    f.version(&test_data(1024 * 1024, 803));
    f.publish();
    let old = f.recipe_chunks("HEAD:asset.bin");
    f.version(&test_data(1024 * 1024, 805));
    f.publish();
    let current = f.recipe_chunks("HEAD:asset.bin");
    git(&f.repo, &["switch", "-c", "unpushed"]);
    f.version(&test_data(1024 * 1024, 807));
    let unpushed = f.recipe_chunks("HEAD:asset.bin");
    git(&f.repo, &["switch", "main"]);
    fs::write(f.repo.join("asset.bin"), test_data(1024 * 1024, 809)).unwrap();
    git(&f.repo, &["add", "asset.bin"]);
    let stashed = f.recipe_chunks(":asset.bin");
    git(&f.repo, &["stash", "push", "-m", "local-only contents"]);
    let staged_data = test_data(1024 * 1024, 811);
    fs::write(f.repo.join("asset.bin"), &staged_data).unwrap();
    git(&f.repo, &["add", "asset.bin"]);
    let staged = f.recipe_chunks(":asset.bin");
    let before = f.chunks();
    let index = git(&f.repo, &["ls-files", "--stage"]);
    let refs = git(
        &f.repo,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );
    let remote_files = count_files(&f.remote);

    cdc(&f.repo, &["prune", "--dry-run"]);
    assert_eq!(f.chunks(), before);
    cdc(&f.repo, &["prune"]);
    let remaining = f.chunks();
    assert!(
        old.is_disjoint(&remaining),
        "published historical chunks are prunable"
    );
    for protected in [current, unpushed, stashed, staged] {
        assert!(protected.is_subset(&remaining));
    }
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), staged_data);
    assert_eq!(git(&f.repo, &["ls-files", "--stage"]), index);
    assert_eq!(
        git(
            &f.repo,
            &["for-each-ref", "--format=%(objectname) %(refname)"]
        ),
        refs
    );
    assert_eq!(count_files(&f.remote), remote_files);
}

#[test]
fn prune_respects_other_worktree_checkouts() {
    let f = Fixture::new();
    let first = f.version(&test_data(1024 * 1024, 813));
    f.publish();
    let old = f.recipe_chunks("HEAD:asset.bin");
    f.version(&test_data(1024 * 1024, 815));
    f.publish();
    let worktree = f.temp.path().join("linked worktree");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
            &first,
        ],
    );

    cdc(&f.repo, &["prune"]);
    assert!(old.is_subset(&f.chunks()));
    git(&f.repo, &["worktree", "remove", worktree.to_str().unwrap()]);
    cdc(&f.repo, &["prune"]);
    assert!(old.is_disjoint(&f.chunks()));
}

#[test]
fn pointer_only_legacy_checkout_and_explicit_pull_are_independent() {
    let f = Fixture::new();
    let data = test_data(1024 * 1024, 817);
    f.version(&data);
    f.publish();
    let recipe = git(&f.repo, &["show", ":asset.bin"]);
    git(&f.repo, &["config", "cdc.hydrate", "false"]);
    fs::remove_file(f.repo.join("asset.bin")).unwrap();
    fs::remove_dir_all(f.repo.join(".git/cdc/objects")).unwrap();
    git(&f.repo, &["checkout", "--", "asset.bin"]);
    assert_eq!(
        fs::read(f.repo.join("asset.bin")).unwrap(),
        recipe.as_bytes()
    );
    assert!(f.chunks().is_empty());
    assert!(git(&f.repo, &["status", "--porcelain"]).is_empty());

    cdc(&f.repo, &["pull", "--", "asset.bin"]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
    assert!(git(&f.repo, &["status", "--porcelain"]).is_empty());
    assert_eq!(git(&f.repo, &["show", ":asset.bin"]), recipe);
}

fn count_files(path: &std::path::Path) -> usize {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}
