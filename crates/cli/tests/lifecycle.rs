//! Backend-independent lifecycle behavior, exercised with the implemented
//! legacy adapter. LFS acceptance contracts live beside the real LFS fixture.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use git_cdc_core::chunker::test_util::test_data;
use git_cdc_core::manifest::Manifest;
use git_cdc_core::store::DiskStore;

mod utils;
use utils::{cdc, git};

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
