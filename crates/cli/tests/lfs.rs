//! Real Git + the installed, unmodified Git LFS client. By default use its
//! standalone file transport. With GIT_CDC_LFS_TEST_SERVER set, every test
//! uses an unmodified github.com/git-lfs/lfs-test-server HTTP server instead.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use git_cdc_core::chunker::test_util::test_data;
use git_cdc_core::manifest::Manifest;

const BIN: &str = env!("CARGO_BIN_EXE_git-cdc");
const RETENTION_PREFIX: &str = "refs/notes/git-cdc-lfs";

#[path = "lfs_support/proxy.rs"]
mod proxy;

struct Fixture {
    temp: tempfile::TempDir,
    repo: PathBuf,
    remote: PathBuf,
    config: PathBuf,
    content: PathBuf,
    server: Option<Child>,
    endpoint: Option<String>,
    proxy: Option<proxy::Proxy>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = &mut self.server {
            let _ = server.kill();
            let _ = server.wait();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        Self::with_proxy(false)
    }

    fn with_proxy(faults: bool) -> Self {
        Self::with_server(faults, std::env::var_os("GIT_CDC_LFS_TEST_SERVER"))
    }

    fn http() -> Self {
        Self::http_with_format("sha1")
    }

    fn http_with_format(object_format: &str) -> Self {
        let binary = std::env::var_os("GIT_CDC_LFS_REFERENCE_SERVER")
            .or_else(|| std::env::var_os("GIT_CDC_LFS_TEST_SERVER"))
            .unwrap_or_else(|| "lfs-test-server".into());
        Self::with_format(false, Some(binary), object_format)
    }

    fn file() -> Self {
        Self::with_server(false, None)
    }

    fn with_server(faults: bool, binary: Option<std::ffi::OsString>) -> Self {
        Self::with_format(faults, binary, "sha1")
    }

    fn with_format(faults: bool, binary: Option<std::ffi::OsString>, object_format: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        let config = temp.path().join("client.gitconfig");
        let mut f = Self {
            temp,
            repo,
            remote,
            config,
            content: PathBuf::new(),
            server: None,
            endpoint: None,
            proxy: None,
        };
        if let Some(binary) = binary {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let host = listener.local_addr().unwrap().to_string();
            drop(listener);
            let advertised = if faults {
                f.proxy = Some(proxy::Proxy::start(host.clone()));
                f.proxy.as_ref().unwrap().address.clone()
            } else {
                host.clone()
            };
            f.content = f.temp.path().join("server-content");
            let log = File::create(f.temp.path().join("server.log")).unwrap();
            f.server = Some(
                Command::new(binary)
                    .env("LFS_LISTEN", format!("tcp://{host}"))
                    .env("LFS_HOST", &advertised)
                    .env("LFS_PUBLIC", "true")
                    .env("LFS_METADB", f.temp.path().join("server.db"))
                    .env("LFS_CONTENTPATH", &f.content)
                    .stdout(log.try_clone().unwrap())
                    .stderr(log)
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            while TcpStream::connect(&host).is_err() {
                assert!(Instant::now() < deadline, "reference server did not start");
                assert!(
                    f.server.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "reference server exited"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            f.endpoint = Some(format!("http://{advertised}/"));
        } else {
            assert!(!faults, "fault proxy requires GIT_CDC_LFS_TEST_SERVER");
            f.content = f.remote.join("cdc/lfs/objects");
        }
        f.cdc(f.temp.path(), &["install", "--lfs", "--global"]);
        f.git(
            f.temp.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                &format!("--object-format={object_format}"),
                f.remote.to_str().unwrap(),
            ],
        );
        f.git(
            f.temp.path(),
            &[
                "init",
                "--initial-branch=main",
                &format!("--object-format={object_format}"),
                f.repo.to_str().unwrap(),
            ],
        );
        f.identity(&f.repo);
        f.git(
            &f.repo,
            &["remote", "add", "origin", f.remote.to_str().unwrap()],
        );
        f.git(&f.repo, &["config", "cdc.chunk.min", "64k"]);
        f.git(&f.repo, &["config", "cdc.chunk.avg", "256k"]);
        f.git(&f.repo, &["config", "cdc.chunk.max", "1m"]);
        if let Some(endpoint) = &f.endpoint {
            fs::write(
                f.repo.join(".lfsconfig"),
                format!("[lfs]\n\turl = {endpoint}\n"),
            )
            .unwrap();
            f.git(&f.repo, &["add", ".lfsconfig"]);
        }
        f.cdc(&f.repo, &["track", "*.bin"]);
        f.git(&f.repo, &["add", ".gitattributes"]);
        f
    }

    fn command(&self, repo: &Path, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", &self.config)
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    Path::new(BIN).parent().unwrap().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("GIT_TERMINAL_PROMPT", "0");
        cmd
    }

    fn run(&self, repo: &Path, args: &[&str]) -> Output {
        self.command(repo, "git").args(args).output().unwrap()
    }

    fn git(&self, repo: &Path, args: &[&str]) -> String {
        let out = self.run(repo, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn refs(&self, repo: &Path, prefix: &str) -> Vec<String> {
        self.git(repo, &["for-each-ref", "--format=%(refname)", prefix])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn cdc(&self, repo: &Path, args: &[&str]) {
        let out = self.command(repo, BIN).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "cdc {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    fn cdc_interactive(&self, args: &[&str], answer: &str) -> Output {
        let command = std::iter::once(BIN)
            .chain(args.iter().copied())
            .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        let mut child = self
            .command(&self.repo, "script")
            .args(["--quiet", "--return", "--command", &command, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn identity(&self, repo: &Path) {
        self.git(repo, &["config", "user.name", "Test"]);
        self.git(repo, &["config", "user.email", "test@example.com"]);
    }

    fn write(&self, path: &str, data: &[u8]) {
        fs::write(self.repo.join(path), data).unwrap();
    }

    fn publish_file(&self, path: &str, data: &[u8], message: &str) {
        self.write(path, data);
        self.git(&self.repo, &["add", path]);
        self.commit(message);
        self.push();
    }

    fn assert_cold_restoration(&self, path: &str, data: &[u8]) {
        for stock_restore in [true, false] {
            self.cdc(&self.repo, &["drop", path]);
            fs::remove_dir_all(self.repo.join(".git/cdc/lfs/objects")).unwrap();
            if stock_restore {
                fs::remove_file(self.repo.join(path)).unwrap();
                self.git(&self.repo, &["restore", "--worktree", "--", path]);
            } else {
                self.cdc(&self.repo, &["pull", "--", path]);
            }
            assert_eq!(fs::read(self.repo.join(path)).unwrap(), data);
        }
    }

    #[cfg(unix)]
    fn standalone_download_agent(&self, relative: bool) {
        let agent = self.repo.join("transfer.py");
        let script = r#"#!/usr/bin/env python3
import json, pathlib, shutil, sys, tempfile
root = pathlib.Path(BACKING_DIRECTORY)
downloads = pathlib.Path(__file__).with_name('agent-downloads')
downloads.mkdir(exist_ok=True)
for line in sys.stdin:
    event = json.loads(line)
    if event['event'] == 'init':
        print('{}', flush=True)
    elif event['event'] == 'terminate':
        break
    else:
        oid = event['oid']
        path = root / oid[:2] / oid[2:4] / oid
        result = {'event': 'complete', 'oid': oid}
        if path.is_file():
            with path.open('rb') as source, tempfile.NamedTemporaryFile(dir=downloads, delete=False) as target:
                shutil.copyfileobj(source, target)
            result['path'] = target.name
        else:
            result['error'] = {'code': 404, 'message': 'missing object'}
        print(json.dumps(result), flush=True)
"#;
        fs::write(
            &agent,
            script.replace(
                "BACKING_DIRECTORY",
                &serde_json::to_string(&self.content).unwrap(),
            ),
        )
        .unwrap();
        make_executable(&agent);
        self.git(
            &self.repo,
            &["config", "lfs.url", "https://standalone.invalid/objects"],
        );
        self.git(
            &self.repo,
            &["config", "lfs.standalonetransferagent", "good"],
        );
        self.git(
            &self.repo,
            &[
                "config",
                "lfs.customtransfer.good.path",
                if relative {
                    "./transfer.py"
                } else {
                    agent.to_str().unwrap()
                },
            ],
        );
    }
    fn commit(&self, msg: &str) -> String {
        self.git(&self.repo, &["commit", "-qm", msg]);
        self.git(&self.repo, &["rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }
    fn push(&self) {
        self.git(&self.repo, &["push", "-u", "origin", "main"]);
    }
    fn clone_repo(&self, name: &str) -> PathBuf {
        let clone = self.temp.path().join(name);
        self.git(
            self.temp.path(),
            &[
                "clone",
                self.remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        self.identity(&clone);
        clone
    }
    fn manifest(&self, rev: &str) -> Manifest {
        Manifest::parse(self.git(&self.repo, &["show", rev]).as_bytes()).unwrap()
    }
    fn objects(&self) -> BTreeMap<String, u64> {
        objects_at(&self.content)
    }
    fn upload_count(&self) -> Option<usize> {
        self.endpoint.as_ref().map(|_| {
            fs::read_to_string(self.temp.path().join("server.log"))
                .unwrap()
                .lines()
                .filter(|line| line.contains("method=PUT"))
                .count()
        })
    }
}

fn objects_at(dir: &Path) -> BTreeMap<String, u64> {
    let mut result = BTreeMap::new();
    if dir.exists() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.extend(objects_at(&path));
            } else if !path.to_string_lossy().ends_with(".tmp") {
                result.insert(
                    path.to_string_lossy().into_owned(),
                    path.metadata().unwrap().len(),
                );
            }
        }
    }
    result
}

#[test]
fn incremental_push_fresh_clone_and_every_historical_version() {
    let f = Fixture::new();
    let mut data = test_data(12 * 1024 * 1024, 41);
    f.write("a.bin", &data);
    f.write("duplicate.bin", &data);
    f.git(&f.repo, &["add", "a.bin", "duplicate.bin"]);
    let first = f.commit("initial identical files");
    assert_eq!(
        f.git(&f.repo, &["show", "HEAD:a.bin"]),
        f.git(&f.repo, &["show", "HEAD:duplicate.bin"])
    );
    f.push();
    let objects = f.objects();
    assert_eq!(objects.values().sum::<u64>(), data.len() as u64);
    // Stock LFS pruning must not sweep the separate CDC cache.
    f.git(&f.repo, &["lfs", "prune"]);
    let uploads = f.upload_count();
    let clone = f.clone_repo("fresh");
    assert_eq!(fs::read(clone.join("a.bin")).unwrap(), data);
    assert!(
        f.git(&clone, &["status", "--porcelain"]).is_empty(),
        "chunk parameters inherited from index"
    );

    data[6_000_000] ^= 255;
    f.write("a.bin", &data);
    f.git(&f.repo, &["add", "a.bin"]);
    let second = f.commit("localized edit");
    let old = f.manifest(&format!("{first}:a.bin"));
    let new = f.manifest("HEAD:a.bin");
    let new_chunks: Vec<_> = new
        .chunks
        .iter()
        .filter(|c| !old.chunks.iter().any(|a| a.hash == c.hash))
        .collect();
    assert!(new_chunks.len() <= 2);
    let expected_new: u64 = new_chunks.iter().map(|c| c.length as u64).sum();
    f.push();
    assert_eq!(
        f.objects().values().sum::<u64>() - objects.values().sum::<u64>(),
        expected_new
    );
    if let Some(uploads) = uploads {
        assert_eq!(f.upload_count().unwrap() - uploads, new_chunks.len());
    }
    assert!(expected_new < data.len() as u64 / 4);
    println!(
        "PERF file={} initial_objects={} edit_new_objects={} edit_upload_bytes={expected_new}",
        data.len(),
        objects.len(),
        new_chunks.len()
    );
    f.git(&clone, &["pull", "--ff-only"]);
    assert_eq!(fs::read(clone.join("a.bin")).unwrap(), data);
    f.git(&f.repo, &["rm", "a.bin", "duplicate.bin"]);
    f.commit("delete files");
    f.push();
    let historical = f.clone_repo("all-history");
    // Prove that nothing below depends on the author's cache.
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    f.git(&historical, &["checkout", &second]);
    assert_eq!(fs::read(historical.join("a.bin")).unwrap(), data);
    f.git(&historical, &["checkout", &first]);
    data[6_000_000] ^= 255;
    assert_eq!(fs::read(historical.join("a.bin")).unwrap(), data);
    assert_eq!(fs::read(historical.join("duplicate.bin")).unwrap(), data);
    f.git(&historical, &["switch", "main"]);
    // Push from a clone whose cache never contained historical data: the
    // hook fetches required old chunks before copying to another LFS remote.
    f.publish_file("note.txt", b"not a CDC file\n", "cache evicted");
}

#[test]
fn staged_snapshot_unrelated_changes_and_alternate_index() {
    let f = Fixture::new();
    f.write("a.bin", b"original\n");
    f.write("b.bin", b"unrelated original\n");
    f.git(&f.repo, &["add", "a.bin", "b.bin"]);
    f.commit("base");
    f.write("a.bin", b"intended staged content\n");
    f.git(&f.repo, &["add", "a.bin"]);
    let staged = f.git(&f.repo, &["show", ":a.bin"]);
    f.write("a.bin", b"later unstaged content\n");
    f.write("b.bin", b"unrelated dirty content\n");
    assert_eq!(
        f.git(&f.repo, &["diff", "--cached", "--name-only"]),
        "a.bin\n"
    );
    assert_eq!(f.git(&f.repo, &["show", ":a.bin"]), staged);
    assert!(
        f.git(&f.repo, &["diff", "--", "a.bin"])
            .contains("later unstaged content")
    );
    f.commit("only staged contents");
    assert_eq!(f.git(&f.repo, &["show", "HEAD:a.bin"]), staged);
    let status = f.git(&f.repo, &["status", "--porcelain"]);
    assert!(
        status.contains(" M a.bin") && status.contains(" M b.bin"),
        "{status}"
    );
    f.push();
    let clone = f.clone_repo("staged-clone");
    assert_eq!(
        fs::read(clone.join("a.bin")).unwrap(),
        b"intended staged content\n"
    );
    assert_eq!(
        fs::read(clone.join("b.bin")).unwrap(),
        b"unrelated original\n"
    );
    let other_index = f.temp.path().join("alternate.index");
    let out = f
        .command(&f.repo, "git")
        .env("GIT_INDEX_FILE", &other_index)
        .args(["read-tree", "HEAD"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = f
        .command(&f.repo, "git")
        .env("GIT_INDEX_FILE", &other_index)
        .args(["add", "b.bin"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        f.git(&f.repo, &["diff", "--cached", "--name-only"])
            .is_empty()
    );
    // Git cannot apply textconv hunks to transformed binary recipes; it must
    // refuse instead of creating a recipe mixing unrelated chunk sequences.
    let out = f
        .command(&f.repo, "git")
        .args(["add", "-p", "a.bin"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        f.git(&f.repo, &["diff", "--cached", "--name-only"])
            .is_empty()
    );
}

#[test]
fn branch_merge_conflict_rebase_pick_revert_reset_and_worktrees() {
    let f = Fixture::new();
    f.write("a.bin", b"base a\n");
    f.write("b.bin", b"base b\n");
    f.git(&f.repo, &["add", "a.bin", "b.bin"]);
    let base = f.commit("base");
    f.git(&f.repo, &["switch", "-c", "topic"]);
    f.write("a.bin", b"topic a\n");
    f.git(&f.repo, &["add", "a.bin"]);
    let topic = f.commit("topic a");
    f.git(&f.repo, &["switch", "main"]);
    f.write("b.bin", b"main b\n");
    f.git(&f.repo, &["add", "b.bin"]);
    f.commit("main b");
    f.git(&f.repo, &["switch", "topic"]);
    f.git(&f.repo, &["rebase", "main"]);
    f.git(&f.repo, &["switch", "main"]);
    f.git(&f.repo, &["merge", "--no-edit", "topic"]);
    assert_eq!(fs::read(f.repo.join("a.bin")).unwrap(), b"topic a\n");
    assert_eq!(fs::read(f.repo.join("b.bin")).unwrap(), b"main b\n");
    f.git(&f.repo, &["switch", "topic"]);
    f.git(&f.repo, &["rebase", "main"]);
    f.git(&f.repo, &["checkout", "--detach", &base]);
    f.git(&f.repo, &["cherry-pick", &topic]);
    assert_eq!(fs::read(f.repo.join("a.bin")).unwrap(), b"topic a\n");
    f.git(&f.repo, &["revert", "--no-edit", "HEAD"]);
    assert_eq!(fs::read(f.repo.join("a.bin")).unwrap(), b"base a\n");
    f.git(&f.repo, &["reset", "--hard", &topic]);
    f.write("a.bin", b"unstaged\n");
    f.git(&f.repo, &["restore", "a.bin"]);
    assert_eq!(fs::read(f.repo.join("a.bin")).unwrap(), b"topic a\n");
    let worktree = f.temp.path().join("linked tree");
    f.git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
            &base,
        ],
    );
    assert_eq!(fs::read(worktree.join("a.bin")).unwrap(), b"base a\n");
    f.write("a.bin", b"one side\n");
    f.git(&f.repo, &["add", "a.bin"]);
    let left = f.commit("one side");
    f.git(&f.repo, &["checkout", "--detach", &topic]);
    f.write("a.bin", b"other side\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("other side");
    let out = f.run(&f.repo, &["merge", &left]);
    assert!(!out.status.success(), "same binary path must conflict");
    let unmerged = f.git(&f.repo, &["ls-files", "-u"]);
    assert_eq!(unmerged.lines().count(), 3);
    assert!(
        !fs::read(f.repo.join("a.bin"))
            .unwrap()
            .starts_with(b"version ")
    );
    f.write("a.bin", b"user resolves conflict\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("resolved explicitly");
    f.git(&f.repo, &["push", "origin", "HEAD:refs/heads/resolved"]);
    let clone = f.clone_repo("resolved-clone");
    f.git(&clone, &["switch", "resolved"]);
    assert_eq!(
        fs::read(clone.join("a.bin")).unwrap(),
        b"user resolves conflict\n"
    );
}

#[test]
fn missing_and_corrupt_data_block_checkout_and_push_without_emitting_bytes() {
    let f = Fixture::new();
    let data = test_data(3 * 1024 * 1024, 83);
    f.write("a.bin", &data);
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("not yet published");
    let m = f.manifest("HEAD:a.bin");
    let c = &m.chunks[0];
    let sha = &m.extra[&format!("lfs.{}", c.hash.to_hex())];
    let path = f
        .repo
        .join(".git/cdc/lfs/objects")
        .join(&sha[..2])
        .join(&sha[2..4])
        .join(sha);
    fs::write(&path, b"corrupt").unwrap();
    let out = f.run(&f.repo, &["push", "origin", "main"]);
    assert!(!out.status.success());
    assert!(
        f.git(&f.remote, &["for-each-ref", "--format=%(refname)"])
            .is_empty()
    );
    let mut manifest = tempfile::tempfile().unwrap();
    manifest.write_all(m.encode().as_bytes()).unwrap();
    manifest.rewind().unwrap();
    let out = f
        .command(&f.repo, BIN)
        .arg("lfs-smudge")
        .stdin(Stdio::from(manifest))
        .output()
        .unwrap();
    assert!(
        !out.status.success() && out.stdout.is_empty(),
        "corrupt content must never be emitted"
    );
    fs::remove_file(f.repo.join("a.bin")).unwrap();
    let out = f.run(&f.repo, &["checkout", "--", "a.bin"]);
    assert!(!out.status.success());
    // Re-cleaning verified original bytes repairs a damaged cache entry.
    f.write("a.bin", &data);
    f.git(&f.repo, &["add", "--renormalize", "a.bin"]);
    fs::remove_file(&path).unwrap();
    let out = f.run(&f.repo, &["push", "origin", "main"]);
    assert!(!out.status.success());
    // A failed batch may leave uploaded objects, but cannot publish Git refs.
    assert!(f.refs(&f.remote, "refs").is_empty());
    f.git(&f.repo, &["add", "--renormalize", "a.bin"]);
    f.push();
    let clone = f.clone_repo("healthy");
    assert_eq!(fs::read(clone.join("a.bin")).unwrap(), data);
    let clone_path = clone
        .join(".git/cdc/lfs/objects")
        .join(&sha[..2])
        .join(&sha[2..4])
        .join(sha);
    fs::remove_file(clone_path).unwrap();
    let server_path = if f.endpoint.is_some() {
        f.content.join(&sha[..2]).join(&sha[2..4]).join(&sha[4..])
    } else {
        f.content.join(&sha[..2]).join(&sha[2..4]).join(sha)
    };
    fs::write(server_path, b"server-corruption").unwrap();
    fs::remove_file(clone.join("a.bin")).unwrap();
    assert!(!f.run(&clone, &["restore", "a.bin"]).status.success());
}

#[test]
fn raw_passthrough_empty_files_quoted_paths_and_concurrent_cleans() {
    let f = Fixture::new();
    // Exercise the one-shot fallback as well as path quoting/option boundaries.
    f.git(
        &f.repo,
        &["config", "--global", "--unset", "filter.cdc-lfs.process"],
    );
    for name in [
        "empty.bin",
        "space name.bin",
        "caf\u{e9}.bin",
        "line\nbreak.bin",
        "-leading.bin",
    ] {
        f.write(
            name,
            if name == "empty.bin" {
                b""
            } else {
                b"shared content\n"
            },
        );
        f.git(&f.repo, &["add", "--", name]);
    }
    f.commit("paths");
    f.push();
    let clone = f.clone_repo("paths-clone");
    assert_eq!(
        fs::read(clone.join("line\nbreak.bin")).unwrap(),
        b"shared content\n"
    );
    assert_eq!(fs::read(clone.join("empty.bin")).unwrap(), b"");
    fs::write(clone.join("-leading.bin"), b"changed leading path\n").unwrap();
    assert!(
        f.git(&clone, &["diff", "--", "-leading.bin"])
            .contains("changed leading path")
    );
    let data = test_data(2 * 1024 * 1024, 16);
    let mut input = tempfile::tempfile().unwrap();
    input.write_all(&data).unwrap();
    input.rewind().unwrap();
    let mut children = Vec::new();
    // Independent file descriptions avoid sharing a seek offset.
    let source = f.temp.path().join("source");
    fs::write(&source, &data).unwrap();
    for _ in 0..4 {
        children.push(
            f.command(&f.repo, BIN)
                .arg("lfs-clean")
                .stdin(File::open(&source).unwrap())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let recipes: Vec<_> = children
        .into_iter()
        .map(|child| {
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            out.stdout
        })
        .collect();
    assert!(recipes.windows(2).all(|w| w[0] == w[1]));
    // Legacy files encountered on checkout before tracking stream unchanged.
    let out = f
        .command(&f.repo, BIN)
        .arg("lfs-smudge")
        .stdin(Stdio::from(input))
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, data);
}

#[test]
fn large_file_filter_process_under_address_space_limit() {
    use git_cdc_core::pktline::{PktReader, PktWriter, read_text, write_flush, write_text};
    let f = Fixture::new();
    let block = test_data(1024 * 1024, 6);
    let mut file = File::create(f.repo.join("large.bin")).unwrap();
    for _ in 0..256 {
        file.write_all(&block).unwrap();
    }
    drop(file);
    let constrained = |args: &[&str]| {
        let out = f.command(&f.repo, "python3").args(["-c",
            "import os,resource,sys; resource.setrlimit(resource.RLIMIT_AS,(192*1024*1024,192*1024*1024)); os.execvp('git',['git']+sys.argv[1:])"])
            .args(args).output().unwrap();
        assert!(
            out.status.success(),
            "bounded-memory git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    constrained(&["add", "large.bin"]);
    f.commit("256 MiB with memory cap");
    f.push();
    fs::remove_file(f.repo.join("large.bin")).unwrap();
    // Git itself buffers smudge output. Check its byte-correct checkout
    // without constraining Git, then constrain the filter protocol process.
    f.git(&f.repo, &["restore", "large.bin"]);
    let mut restored = File::open(f.repo.join("large.bin")).unwrap();
    let mut expected = blake3::Hasher::new();
    for _ in 0..256 {
        expected.update(&block);
    }
    let mut actual = blake3::Hasher::new();
    actual.update_reader(&mut restored).unwrap();
    assert_eq!(actual.finalize(), expected.finalize());
    let mut child = f.command(&f.repo, "python3").args(["-c",
        "import resource,subprocess,sys; resource.setrlimit(resource.RLIMIT_AS,(192*1024*1024,192*1024*1024)); p=subprocess.run(sys.argv[1:]); print('FILTER_MAX_RSS_KIB',resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss,file=sys.stderr); sys.exit(p.returncode)",
        BIN, "lfs-filter-process"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    write_text(&mut input, "git-filter-client").unwrap();
    write_text(&mut input, "version=2").unwrap();
    write_flush(&mut input).unwrap();
    assert_eq!(
        read_text(&mut output).unwrap().as_deref(),
        Some("git-filter-server")
    );
    assert_eq!(
        read_text(&mut output).unwrap().as_deref(),
        Some("version=2")
    );
    assert!(read_text(&mut output).unwrap().is_none());
    write_text(&mut input, "capability=smudge").unwrap();
    write_flush(&mut input).unwrap();
    while read_text(&mut output).unwrap().is_some() {}
    write_text(&mut input, "command=smudge").unwrap();
    write_text(&mut input, "pathname=large.bin").unwrap();
    write_flush(&mut input).unwrap();
    let recipe = f.git(&f.repo, &["show", "HEAD:large.bin"]);
    PktWriter::new(&mut input)
        .write_all(recipe.as_bytes())
        .unwrap();
    write_flush(&mut input).unwrap();
    assert_eq!(
        read_text(&mut output).unwrap().as_deref(),
        Some("status=success")
    );
    assert!(read_text(&mut output).unwrap().is_none());
    let mut streamed = blake3::Hasher::new();
    streamed.update_reader(PktReader::new(&mut output)).unwrap();
    assert_eq!(streamed.finalize(), expected.finalize());
    assert!(read_text(&mut output).unwrap().is_none());
    drop(input);
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    println!("{}", String::from_utf8_lossy(&result.stderr));
    println!(
        "PERF large_file_bytes={} address_space_limit_bytes={}",
        256 * 1024 * 1024,
        192 * 1024 * 1024
    );
}

#[test]
#[ignore = "set GIT_CDC_LFS_TEST_SERVER to the unmodified reference server binary"]
fn unmodified_http_reference_server() {
    assert!(std::env::var_os("GIT_CDC_LFS_TEST_SERVER").is_some());
    incremental_push_fresh_clone_and_every_historical_version();
}

#[test]
fn interrupted_clean_and_failed_upload_can_be_retried() {
    let mut f = Fixture::new();
    let data = test_data(4 * 1024 * 1024, 107);
    let mut child = f
        .command(&f.repo, BIN)
        .arg("lfs-clean")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(&data[..2 * 1024 * 1024]).unwrap();
    child.kill().unwrap();
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success() && out.stdout.is_empty());
    f.publish_file("a.bin", &data, "retry interrupted clean");
    let published = f.git(&f.remote, &["rev-parse", "refs/heads/main"]);
    let mut changed = data.clone();
    changed[2_000_000] ^= 255;
    f.write("a.bin", &changed);
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("pending upload");
    f.git(&f.repo, &["config", "lfs.transfer.maxretries", "1"]);
    f.git(&f.repo, &["config", "lfs.transfer.maxretrydelay", "1"]);
    let parked = f.temp.path().join("parked-content");
    if let Some(server) = &mut f.server {
        server.kill().unwrap();
        server.wait().unwrap();
    } else {
        fs::rename(&f.content, &parked).unwrap();
        fs::write(&f.content, b"temporarily unavailable storage").unwrap();
    }
    assert!(!f.run(&f.repo, &["push", "origin", "main"]).status.success());
    assert_eq!(
        f.git(&f.remote, &["rev-parse", "refs/heads/main"]),
        published
    );
    if let Some(endpoint) = &f.endpoint {
        let host = endpoint.trim_start_matches("http://").trim_end_matches('/');
        let log = File::options()
            .append(true)
            .open(f.temp.path().join("server.log"))
            .unwrap();
        f.server = Some(
            Command::new(std::env::var_os("GIT_CDC_LFS_TEST_SERVER").unwrap())
                .env("LFS_LISTEN", format!("tcp://{host}"))
                .env("LFS_HOST", host)
                .env("LFS_PUBLIC", "true")
                .env("LFS_METADB", f.temp.path().join("server.db"))
                .env("LFS_CONTENTPATH", &f.content)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(host).is_err() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
    } else {
        fs::remove_file(&f.content).unwrap();
        fs::rename(parked, &f.content).unwrap();
    }
    f.push();
    let clone = f.clone_repo("retry-clone");
    assert_eq!(fs::read(clone.join("a.bin")).unwrap(), changed);
}

#[test]
fn tag_only_history_direct_url_and_evicted_cache_to_new_remote() {
    let f = Fixture::new();
    f.write("a.bin", b"base version\n");
    f.git(&f.repo, &["add", "a.bin"]);
    let base = f.commit("base");
    f.write("a.bin", b"tagged version\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("tagged");
    f.git(&f.repo, &["tag", "-a", "release", "-m", "annotated tag"]);
    f.git(&f.repo, &["reset", "--hard", &base]);
    f.git(&f.repo, &["push", "origin", "refs/tags/release"]);
    f.git(
        &f.repo,
        &["push", f.remote.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    let clone = f.clone_repo("tag-clone");
    f.git(&clone, &["checkout", "--detach", "release"]);
    assert_eq!(fs::read(clone.join("a.bin")).unwrap(), b"tagged version\n");
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    f.git(&f.repo, &["push", "origin", ":refs/tags/release"]);
    // Standalone transport can independently exercise a second storage
    // endpoint. HTTP's .lfsconfig intentionally shares one endpoint per repo.
    if f.endpoint.is_none() {
        let backup = f.temp.path().join("backup.git");
        f.git(f.temp.path(), &["init", "--bare", backup.to_str().unwrap()]);
        f.git(
            &f.repo,
            &["remote", "add", "backup", backup.to_str().unwrap()],
        );
        f.git(&f.repo, &["push", "backup", "main"]);
        fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
        let recovered = f.temp.path().join("backup-clone");
        f.git(
            f.temp.path(),
            &[
                "clone",
                "-b",
                "main",
                backup.to_str().unwrap(),
                recovered.to_str().unwrap(),
            ],
        );
        assert_eq!(
            fs::read(recovered.join("a.bin")).unwrap(),
            b"base version\n"
        );
    }
}

#[test]
fn publication_binds_each_push_url_to_its_own_chunks_and_retention_roots() {
    let f = Fixture::new();
    if f.endpoint.is_some() {
        return; // Independent file remotes exercise separate LFS destinations.
    }
    let backup = f.temp.path().join("backup.git");
    f.git(
        f.temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            backup.to_str().unwrap(),
        ],
    );
    for destination in [&f.remote, &backup] {
        f.git(
            &f.repo,
            &[
                "config",
                "--add",
                "remote.origin.pushurl",
                destination.to_str().unwrap(),
            ],
        );
    }
    let configured = f.git(&f.repo, &["config", "--get-all", "remote.origin.pushurl"]);
    let data = test_data(1024 * 1024, 718);
    f.write("a.bin", &data);
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("publish independently to both destinations");
    f.git(&f.repo, &["push", "origin", "main"]);
    assert_eq!(
        f.git(&f.repo, &["config", "--get-all", "remote.origin.pushurl"]),
        configured
    );
    let roots = f.refs(&f.remote, RETENTION_PREFIX);
    let backup_roots = f.refs(&backup, RETENTION_PREFIX);
    assert!(!roots.is_empty() && !backup_roots.is_empty());
    assert!(roots.iter().all(|root| !backup_roots.contains(root)));
    for (index, destination) in [&f.remote, &backup].into_iter().enumerate() {
        let clone = f.temp.path().join(format!("destination-{index}"));
        f.git(
            f.temp.path(),
            &[
                "clone",
                destination.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        assert_eq!(fs::read(clone.join("a.bin")).unwrap(), data);
    }
}

#[test]
fn publication_preserves_remote_specific_lfs_endpoint_settings() {
    let f = Fixture::new();
    if f.endpoint.is_some() {
        return; // This fixture's .lfsconfig otherwise overrides remote settings.
    }
    let storage = f.temp.path().join("lfs-storage.git");
    f.git(
        f.temp.path(),
        &["init", "--bare", storage.to_str().unwrap()],
    );
    let endpoint = format!("file://{}", storage.display());
    for key in ["remote.origin.lfsurl", "remote.origin.lfspushurl"] {
        f.git(&f.repo, &["config", key, &endpoint]);
    }
    f.publish_file(
        "a.bin",
        b"explicit LFS endpoint\n",
        "remote-specific LFS endpoint",
    );
    assert!(!objects_at(&storage.join("cdc/lfs/objects")).is_empty());
    assert!(f.objects().is_empty());
    let clone = f.temp.path().join("explicit-endpoint-clone");
    f.git(
        f.temp.path(),
        &[
            "-c",
            &format!("remote.origin.lfsurl={endpoint}"),
            "clone",
            f.remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(
        fs::read(clone.join("a.bin")).unwrap(),
        b"explicit LFS endpoint\n"
    );
}

#[test]
fn publication_scans_original_commits_despite_replacements() {
    publication_with_replacement(true);
}

#[test]
fn publication_reads_original_blobs_despite_replacements() {
    publication_with_replacement(false);
}

fn publication_with_replacement(replace_commit: bool) {
    let f = Fixture::new();
    f.write("a.bin", b"original content must reach the destination\n");
    f.git(&f.repo, &["add", "a.bin"]);
    let tip = f.commit("original CDC history");
    let (original, replacement) = if replace_commit {
        let tree = f.git(&f.repo, &["mktree"]);
        (
            tip.clone(),
            f.git(
                &f.repo,
                &["commit-tree", tree.trim(), "-m", "empty replacement"],
            ),
        )
    } else {
        (
            f.git(&f.repo, &["rev-parse", "HEAD:a.bin"]),
            f.git(&f.repo, &["hash-object", "-w", "--stdin"]),
        )
    };
    f.git(&f.repo, &["replace", original.trim(), replacement.trim()]);
    f.push();
    assert_eq!(f.git(&f.remote, &["rev-parse", "main"]).trim(), tip);
    let clone = f.clone_repo("original-objects-clone");
    assert_eq!(
        fs::read(clone.join("a.bin")).unwrap(),
        b"original content must reach the destination\n"
    );
}

#[test]
fn publication_local_stock_lfs_hook_allows_mirror_pushes() {
    publication_with_stock_lfs_hook(false);
}

#[test]
fn publication_global_dispatch_of_stock_lfs_hook_allows_mirror_pushes() {
    publication_with_stock_lfs_hook(true);
}

fn publication_with_stock_lfs_hook(global_dispatch: bool) {
    let f = Fixture::new();
    f.git(&f.repo, &["lfs", "install", "--skip-repo"]);
    f.git(
        &f.repo,
        &[
            "-c",
            "core.hooksPath=.git/hooks",
            "lfs",
            "install",
            "--local",
        ],
    );
    if !global_dispatch {
        f.git(&f.repo, &["config", "core.hooksPath", ".git/hooks"]);
        f.cdc(&f.repo, &["install", "--lfs"]);
    }
    f.git(&f.repo, &["lfs", "track", "*.raw"]);
    f.write("a.bin", b"CDC mirror data\n");
    f.write("stock.raw", b"ordinary LFS mirror data\n");
    f.git(&f.repo, &["add", ".gitattributes", "a.bin", "stock.raw"]);
    f.commit("stock LFS hook with both file formats");
    f.push();
    let backup = f.temp.path().join("backup.git");
    f.git(
        f.temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            backup.to_str().unwrap(),
        ],
    );
    f.git(
        &f.repo,
        &["remote", "add", "backup", backup.to_str().unwrap()],
    );
    f.git(&f.repo, &["push", "--mirror", "backup"]);
    let clone = f.temp.path().join("mixed-mirror-clone");
    f.git(
        f.temp.path(),
        &["clone", backup.to_str().unwrap(), clone.to_str().unwrap()],
    );
    assert_eq!(fs::read(clone.join("a.bin")).unwrap(), b"CDC mirror data\n");
    assert_eq!(
        fs::read(clone.join("stock.raw")).unwrap(),
        b"ordinary LFS mirror data\n"
    );
}

#[test]
fn publication_chained_hook_keeps_its_veto_after_repository_relocation() {
    let mut f = Fixture::new();
    let hooks = f.repo.join("custom hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("pre-push");
    fs::write(&hook, "#!/bin/sh\ncat > hook-input\nexit 23\n").unwrap();
    make_executable(&hook);
    f.git(&f.repo, &["config", "core.hooksPath", "custom hooks"]);
    f.cdc(&f.repo, &["install", "--lfs"]);
    f.write("a.bin", b"veto must survive relocation\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("relocated repository");
    let moved = f.temp.path().join("moved repo");
    fs::rename(&f.repo, &moved).unwrap();
    f.repo = moved;
    assert!(!f.run(&f.repo, &["push", "origin", "main"]).status.success());
    assert!(
        fs::read_to_string(f.repo.join("hook-input"))
            .unwrap()
            .contains("refs/heads/main")
    );
    assert!(f.refs(&f.remote, "refs").is_empty());
    assert!(f.objects().is_empty());
}

#[test]
fn publication_customized_stock_lfs_hook_keeps_its_veto() {
    let f = Fixture::new();
    f.git(
        &f.repo,
        &[
            "-c",
            "core.hooksPath=.git/hooks",
            "lfs",
            "install",
            "--local",
        ],
    );
    let hook = f.repo.join(".git/hooks/pre-push");
    let mut contents = fs::read_to_string(&hook).unwrap();
    contents.push_str("\nexit 23\n");
    fs::write(&hook, contents).unwrap();
    f.git(&f.repo, &["config", "core.hooksPath", ".git/hooks"]);
    f.cdc(&f.repo, &["install", "--lfs"]);
    f.write("a.bin", b"custom hook must still run\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("customized stock LFS hook");
    assert!(!f.run(&f.repo, &["push", "origin", "main"]).status.success());
    assert!(f.refs(&f.remote, "refs").is_empty());
    assert!(f.objects().is_empty());
}

#[test]
fn existing_hook_and_custom_hook_directory_are_preserved() {
    let f = Fixture::new();
    let hooks = f.repo.join("custom hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("pre-push");
    let record = f.temp.path().join("hook-input");
    fs::write(&hook, format!("#!/bin/sh\ncat > '{}'\n", record.display())).unwrap();
    make_executable(&hook);
    f.git(&f.repo, &["config", "core.hooksPath", "custom hooks"]);
    f.cdc(&f.repo, &["install", "--lfs"]);
    f.cdc(&f.repo, &["install", "--lfs"]);
    f.publish_file("a.bin", b"hook test\n", "hook");
    assert!(
        fs::read_to_string(record)
            .unwrap()
            .contains("refs/heads/main")
    );
    assert!(hooks.join("pre-push.before-cdc-lfs").is_file());
    let published = f.git(&f.remote, &["rev-parse", "refs/heads/main"]);
    fs::write(hooks.join("pre-push.before-cdc-lfs"), "#!/bin/sh\nexit 1\n").unwrap();
    f.write("a.bin", b"hook refuses this commit\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("refused by original hook");
    assert!(!f.run(&f.repo, &["push", "origin", "main"]).status.success());
    assert_eq!(
        f.git(&f.remote, &["rev-parse", "refs/heads/main"]),
        published
    );
}

#[test]
fn standard_lfs_files_coexist_and_local_hooks_still_receive_updates() {
    let f = Fixture::new();
    for (key, value) in [
        ("filter.lfs.process", "git-lfs filter-process"),
        ("filter.lfs.clean", "git-lfs clean -- %f"),
        ("filter.lfs.smudge", "git-lfs smudge -- %f"),
        ("filter.lfs.required", "true"),
    ] {
        f.git(&f.repo, &["config", "--global", key, value]);
    }
    f.git(&f.repo, &["lfs", "track", "*.raw"]);
    f.write("a.bin", b"chunked file\n");
    f.write("stock.raw", b"ordinary LFS content\n");
    let local_hook = f.repo.join(".git/hooks/pre-push");
    let record = f.temp.path().join("local-hook-input");
    fs::write(
        &local_hook,
        format!("#!/bin/sh\ncat > '{}'\n", record.display()),
    )
    .unwrap();
    make_executable(&local_hook);
    f.git(&f.repo, &["add", ".gitattributes", "a.bin", "stock.raw"]);
    f.commit("mixed LFS and CDC");
    f.push();
    assert!(
        fs::read_to_string(record)
            .unwrap()
            .contains("refs/heads/main")
    );
    let clone = f.clone_repo("mixed-clone");
    assert_eq!(fs::read(clone.join("a.bin")).unwrap(), b"chunked file\n");
    assert_eq!(
        fs::read(clone.join("stock.raw")).unwrap(),
        b"ordinary LFS content\n"
    );
}

#[test]
fn invalid_recipes_preserve_index_and_checkout_checks_the_full_file_hash() {
    let f = Fixture::new();
    f.write("a.bin", b"checked content\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("valid");
    let original = f.git(&f.repo, &["show", ":a.bin"]);
    let mut malformed = f.manifest("HEAD:a.bin");
    malformed.chunks[0].offset = 99;
    f.write("a.bin", malformed.encode().as_bytes());
    assert!(!f.run(&f.repo, &["add", "a.bin"]).status.success());
    assert_eq!(f.git(&f.repo, &["show", ":a.bin"]), original);
    // Push validates metadata and transfers chunks without auditing every
    // historical file. Checkout must still reject a false whole-file digest.
    let mut bad_hash = f.manifest("HEAD:a.bin");
    bad_hash.oid = blake3::hash(b"not the content");
    let mut recipe = tempfile::tempfile().unwrap();
    recipe.write_all(bad_hash.encode().as_bytes()).unwrap();
    recipe.rewind().unwrap();
    let out = f
        .command(&f.repo, BIN)
        .arg("lfs-smudge")
        .stdin(Stdio::from(recipe.try_clone().unwrap()))
        .output()
        .unwrap();
    assert!(!out.status.success() && out.stdout.is_empty());
    recipe.rewind().unwrap();
    f.write("a.bin", b"checked content\n");
    let blob = f
        .command(&f.repo, "git")
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::from(recipe))
        .output()
        .unwrap();
    assert!(blob.status.success());
    let oid = String::from_utf8(blob.stdout).unwrap();
    f.git(
        &f.repo,
        &["update-index", "--cacheinfo", "100644", oid.trim(), "a.bin"],
    );
    f.commit("invalid digest inserted using plumbing");
    f.push();
    let recovered = f.temp.path().join("bad-file-digest");
    let out = f.run(
        f.temp.path(),
        &[
            "clone",
            f.remote.to_str().unwrap(),
            recovered.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("reassembled file does not match manifest oid")
    );
}

#[test]
fn default_chunk_parameters_and_one_bit_edit_transfer() {
    let f = Fixture::new();
    for key in ["cdc.chunk.min", "cdc.chunk.avg", "cdc.chunk.max"] {
        f.git(&f.repo, &["config", "--unset", key]);
    }
    let mut data = test_data(12 * 1024 * 1024, 41);
    f.write("a.bin", &data);
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("upstream default chunk parameters");
    let old = f.manifest("HEAD:a.bin");
    assert_eq!(
        (old.chunk_min, old.chunk_avg, old.chunk_max),
        (512 * 1024, 2 * 1024 * 1024, 8 * 1024 * 1024)
    );
    f.push();
    let initial = f.objects();
    let uploads = f.upload_count();
    data[6_000_000] ^= 1;
    f.write("a.bin", &data);
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("one bit changed");
    let new = f.manifest("HEAD:a.bin");
    let added: Vec<_> = new
        .chunks
        .iter()
        .filter(|c| !old.chunks.iter().any(|a| a.hash == c.hash))
        .collect();
    let bytes = added.iter().map(|c| c.length as u64).sum::<u64>();
    assert!(!added.is_empty() && added.len() <= 2);
    f.push();
    assert_eq!(
        f.objects().values().sum::<u64>() - initial.values().sum::<u64>(),
        bytes
    );
    if let Some(uploads) = uploads {
        assert_eq!(f.upload_count().unwrap() - uploads, added.len());
    }
    println!(
        "PERF_DEFAULT file={} initial_objects={} one_bit_new_objects={} one_bit_upload_bytes={bytes}",
        data.len(),
        initial.len(),
        added.len()
    );
}

#[test]
fn global_install_preserves_commit_checkout_and_stdin_hook_semantics() {
    let f = Fixture::new();
    let hooks = f.repo.join(".git/hooks");
    let record = f.temp.path().join("hook-events");
    let scripts = [
        (
            "pre-commit",
            format!(
                "#!/bin/sh\nprintf 'pre-commit:%s:%s\\n' \"$PWD\" \"$GIT_EDITOR\" >> '{}'\n",
                record.display()
            ),
        ),
        (
            "commit-msg",
            format!(
                "#!/bin/sh\nprintf '\\nmessage checked\\n' >> \"$1\"\nprintf 'commit-msg:%s\\n' \"$1\" >> '{}'\n",
                record.display()
            ),
        ),
        (
            "post-commit",
            format!(
                "#!/bin/sh\nprintf 'post-commit\\n' >> '{}'\n",
                record.display()
            ),
        ),
        (
            "post-checkout",
            format!(
                "#!/bin/sh\nprintf 'post-checkout:%s:%s:%s\\n' \"$1\" \"$2\" \"$3\" >> '{}'\n",
                record.display()
            ),
        ),
    ];
    for (name, contents) in scripts {
        fs::write(hooks.join(name), contents).unwrap();
        make_executable(&hooks.join(name));
    }
    f.write("a.bin", b"preserve hooks\n");
    f.git(&f.repo, &["add", "a.bin"]);
    let commit = f.commit("hook preservation");
    assert!(
        f.git(&f.repo, &["log", "-1", "--format=%B"])
            .contains("message checked")
    );
    f.git(&f.repo, &["checkout", "--detach", &commit]);
    let events = fs::read_to_string(&record).unwrap();
    assert!(
        events.contains(&format!("pre-commit:{}::", f.repo.display())),
        "{events}"
    );
    assert!(events.contains("commit-msg:") && events.contains("post-commit\n"));
    assert!(
        events.contains(&format!("post-checkout:{commit}:{commit}:1")),
        "{events}"
    );
    // Installation never rewrites a hook the user placed in the active
    // global directory, even when installation is repeated.
    let global_hooks = PathBuf::from(
        f.git(&f.repo, &["config", "--path", "core.hooksPath"])
            .trim(),
    );
    let custom = b"#!/bin/sh\nprintf 'custom global hook\\n'\n";
    fs::write(global_hooks.join("post-merge"), custom).unwrap();
    f.cdc(&f.repo, &["install", "--lfs", "--global"]);
    assert_eq!(fs::read(global_hooks.join("post-merge")).unwrap(), custom);
    // Binary stdin/stdout and a nonzero exit code survive the process
    // boundary unchanged. Non-executable hooks retain Git's ignored status.
    let stdin_hook = hooks.join("reference-transaction");
    fs::write(&stdin_hook, "#!/bin/sh\ncat\nexit 37\n").unwrap();
    make_executable(&stdin_hook);
    let mut input = tempfile::tempfile().unwrap();
    let bytes = b"binary\0input\nwith\xffbytes\n";
    input.write_all(bytes).unwrap();
    input.rewind().unwrap();
    let out = f
        .command(
            &f.repo,
            global_hooks.join("reference-transaction").to_str().unwrap(),
        )
        .arg("prepared")
        .stdin(Stdio::from(input))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(37));
    assert_eq!(out.stdout, bytes);
    fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 19\n").unwrap();
    let head = f.git(&f.repo, &["rev-parse", "HEAD"]);
    assert!(
        !f.run(
            &f.repo,
            &["commit", "--allow-empty", "-m", "must be vetoed"]
        )
        .status
        .success()
    );
    assert_eq!(f.git(&f.repo, &["rev-parse", "HEAD"]), head);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(hooks.join("pre-commit"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_file(stdin_hook).unwrap();
        f.git(
            &f.repo,
            &["commit", "--allow-empty", "-m", "inactive hook ignored"],
        );
    }
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn global_dispatch_preserves_an_original_hook_behind_a_local_cdc_wrapper() {
    let f = Fixture::new();
    let hook = f.repo.join(".git/hooks/pre-push");
    let record = f.temp.path().join("original-pre-push");
    fs::write(
        &hook,
        format!("#!/bin/sh\ncat > '{}'\nexit 9\n", record.display()),
    )
    .unwrap();
    make_executable(&hook);
    f.git(
        &f.repo,
        &["-c", "core.hooksPath=.git/hooks", "cdc", "install", "--lfs"],
    );
    f.write("a.bin", b"veto must survive wrapping\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("hook nesting");
    assert!(!f.run(&f.repo, &["push", "origin", "main"]).status.success());
    assert!(
        fs::read_to_string(record)
            .unwrap()
            .contains("refs/heads/main")
    );
    assert!(
        f.git(&f.remote, &["for-each-ref", "--format=%(refname)"])
            .is_empty()
    );
}

#[test]
#[ignore = "requires GIT_CDC_LFS_TEST_SERVER; interrupts an actual HTTP upload"]
fn interrupted_mid_upload_preserves_refs_and_retries_only_missing_objects() {
    use std::sync::atomic::Ordering;
    let f = Fixture::with_proxy(true);
    f.write("base.bin", b"already published\n");
    f.git(&f.repo, &["add", "base.bin"]);
    let base = f.commit("before interrupted upload");
    f.push();
    let initial_objects = f.objects().len();
    let data = test_data(4 * 1024 * 1024, 123);
    f.write("new.bin", &data);
    f.git(&f.repo, &["add", "new.bin"]);
    f.commit("new chunks");
    let recipe = f.manifest("HEAD:new.bin");
    let unique: std::collections::HashSet<_> = recipe.chunks.iter().map(|c| c.hash).collect();
    assert!(unique.len() > 2);
    f.git(&f.repo, &["config", "lfs.concurrenttransfers", "1"]);
    f.git(&f.repo, &["config", "lfs.transfer.maxretries", "1"]);
    f.git(&f.repo, &["config", "lfs.transfer.maxretrydelay", "1"]);
    let proxy = f.proxy.as_ref().unwrap();
    proxy.interrupt_nth_next_put(2, 8192);
    let out = f.run(&f.repo, &["push", "origin", "main"]);
    assert!(
        !out.status.success(),
        "the interrupted transfer must fail the push"
    );
    assert!(proxy.state.cuts.load(Ordering::SeqCst) >= 1);
    let sent = proxy.state.cut_bytes.load(Ordering::SeqCst);
    let length = proxy.state.cut_length.load(Ordering::SeqCst);
    assert!(
        sent > 0 && sent < length,
        "fault was not inside the upload: {sent}/{length}"
    );
    assert_eq!(
        f.git(&f.remote, &["rev-parse", "refs/heads/main"]).trim(),
        base
    );
    let stored_after_failure = f.objects().len();
    assert!(
        stored_after_failure > initial_objects,
        "a completed earlier upload should survive"
    );
    assert!(stored_after_failure < initial_objects + unique.len());
    proxy.clear_fault();
    let before_retry = proxy.state.puts.load(Ordering::SeqCst);
    f.push();
    let retried = proxy.state.puts.load(Ordering::SeqCst) - before_retry;
    assert_eq!(
        retried,
        initial_objects + unique.len() - stored_after_failure
    );
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    let recovered = f.clone_repo("after-mid-upload-retry");
    assert_eq!(fs::read(recovered.join("new.bin")).unwrap(), data);
    f.git(&recovered, &["checkout", &base]);
    assert_eq!(
        fs::read(recovered.join("base.bin")).unwrap(),
        b"already published\n"
    );
    assert!(!recovered.join("new.bin").exists());
    println!(
        "MID_UPLOAD cut_bytes={sent} object_bytes={length} retained_objects={} retry_puts={retried}",
        stored_after_failure - initial_objects
    );
}

fn concurrent_commands(mut a: Command, mut b: Command) -> (Output, Output) {
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let one = scope.spawn(|| {
            barrier.wait();
            a.output().unwrap()
        });
        let two = scope.spawn(|| {
            barrier.wait();
            b.output().unwrap()
        });
        (one.join().unwrap(), two.join().unwrap())
    })
}

#[test]
fn independent_clones_concurrent_uploads_ref_race_rebase_and_recovery() {
    use std::sync::atomic::Ordering;
    let http = std::env::var_os("GIT_CDC_LFS_TEST_SERVER").is_some();
    let f = Fixture::with_proxy(http);
    f.write("base.bin", b"base\n");
    f.git(&f.repo, &["add", "base.bin"]);
    let base = f.commit("base for concurrent clones");
    f.push();
    let a = f.clone_repo("independent-a");
    let b = f.clone_repo("independent-b");
    let a_data = test_data(1024 * 1024, 221);
    let b_data = test_data(1024 * 1024, 223);
    let shared = test_data(2 * 1024 * 1024, 225);
    for (repo, name, data, label) in [(&a, "a.bin", &a_data, "a"), (&b, "b.bin", &b_data, "b")] {
        fs::write(repo.join(name), data).unwrap();
        fs::write(repo.join("shared.bin"), &shared).unwrap();
        f.git(
            repo,
            &[
                "config",
                "http.extraHeader",
                &format!("X-Cdc-Test-Client: {label}"),
            ],
        );
        f.git(repo, &["add", name, "shared.bin"]);
        f.git(repo, &["commit", "-qm", label]);
    }
    if let Some(proxy) = &f.proxy {
        proxy.state.delay_ms.store(4, Ordering::SeqCst);
    }
    let mut push_a = f.command(&a, "git");
    push_a.args(["push", "origin", "HEAD:refs/heads/client-a"]);
    let mut push_b = f.command(&b, "git");
    push_b.args(["push", "origin", "HEAD:refs/heads/client-b"]);
    let (one, two) = concurrent_commands(push_a, push_b);
    assert!(
        one.status.success(),
        "clone a: {}",
        String::from_utf8_lossy(&one.stderr)
    );
    assert!(
        two.status.success(),
        "clone b: {}",
        String::from_utf8_lossy(&two.stderr)
    );
    if let Some(proxy) = &f.proxy {
        assert!(
            proxy.state.peak_clients.load(Ordering::SeqCst) >= 2,
            "uploads from both clones must actually overlap"
        );
        proxy.state.delay_ms.store(0, Ordering::SeqCst);
    }
    assert_eq!(
        f.objects().values().sum::<u64>(),
        (b"base\n".len() + a_data.len() + b_data.len() + shared.len()) as u64
    );
    // Both commits descend from the same base. Git must accept exactly one
    // simultaneous main update; content uploaded by the losing push is safe.
    let mut push_a = f.command(&a, "git");
    push_a.args(["push", "origin", "HEAD:refs/heads/main"]);
    let mut push_b = f.command(&b, "git");
    push_b.args(["push", "origin", "HEAD:refs/heads/main"]);
    let (one, two) = concurrent_commands(push_a, push_b);
    assert_ne!(
        one.status.success(),
        two.status.success(),
        "exactly one ref update must win: a={} b={}",
        String::from_utf8_lossy(&one.stderr),
        String::from_utf8_lossy(&two.stderr)
    );
    let loser = if one.status.success() { &b } else { &a };
    f.git(loser, &["fetch", "origin"]);
    f.git(loser, &["rebase", "origin/main"]);
    f.git(loser, &["push", "origin", "HEAD:refs/heads/main"]);
    for repo in [&f.repo, &a, &b] {
        fs::remove_dir_all(repo.join(".git/cdc/lfs")).unwrap();
    }
    let recovered = f.clone_repo("concurrent-recovery");
    assert_eq!(fs::read(recovered.join("a.bin")).unwrap(), a_data);
    assert_eq!(fs::read(recovered.join("b.bin")).unwrap(), b_data);
    assert_eq!(fs::read(recovered.join("shared.bin")).unwrap(), shared);
    f.git(&recovered, &["checkout", &base]);
    assert_eq!(fs::read(recovered.join("base.bin")).unwrap(), b"base\n");
    assert!(!recovered.join("a.bin").exists() && !recovered.join("b.bin").exists());
}

fn stock_lfs_roots(f: &Fixture) -> std::collections::BTreeSet<String> {
    f.git(&f.remote, &["lfs", "ls-files", "--all", "--long"])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect()
}

fn object_name(path: &str) -> String {
    let path = Path::new(path);
    let name = path.file_name().unwrap().to_str().unwrap();
    if name.len() == 64 {
        return name.to_owned();
    }
    let parent = path.parent().unwrap();
    let grandparent = parent.parent().unwrap();
    format!(
        "{}{}{name}",
        grandparent.file_name().unwrap().to_str().unwrap(),
        parent.file_name().unwrap().to_str().unwrap()
    )
}

#[test]
fn retention_standard_pointer_cleanup_keeps_current_and_deleted_history() {
    let f = Fixture::new();
    let mut data = test_data(3 * 1024 * 1024, 311);
    f.write("a.bin", &data);
    f.git(&f.repo, &["add", "a.bin"]);
    let first = f.commit("retention v1");
    f.push();
    data[1_234_567] ^= 1;
    f.write("a.bin", &data);
    f.git(&f.repo, &["add", "a.bin"]);
    let second = f.commit("retention v2");
    f.push();
    f.git(&f.repo, &["rm", "a.bin"]);
    f.commit("deleted file still has history");
    f.push();
    let roots = stock_lfs_roots(&f);
    let objects = f.objects();
    assert!(!roots.is_empty());
    for path in objects.keys() {
        assert!(roots.contains(&object_name(path)), "unrooted chunk {path}");
    }
    // Upload one real orphan using the standard LFS client. A stock-pointer
    // all-ref collector must distinguish it from all historical CDC chunks.
    let orphan_file = f.temp.path().join("orphan-source");
    fs::write(&orphan_file, b"uploaded but never referenced").unwrap();
    let out = f
        .command(&f.repo, BIN)
        .arg("lfs-clean")
        .stdin(File::open(orphan_file).unwrap())
        .output()
        .unwrap();
    assert!(out.status.success());
    let orphan = Manifest::parse(&out.stdout).unwrap();
    let sha = &orphan.extra[&format!("lfs.{}", orphan.chunks[0].hash.to_hex())];
    f.git(
        &f.repo,
        &[
            "-c",
            "lfs.storage=cdc/lfs",
            "lfs",
            "push",
            "--object-id",
            "origin",
            sha,
        ],
    );
    assert_eq!(f.objects().len(), objects.len() + 1);
    // This is a collector simulation using stock Git LFS discovery, not a
    // modified LFS server or a claim of live GitLab-hosted integration testing.
    let mut removed = 0;
    for path in f.objects().keys() {
        if !roots.contains(&object_name(path)) {
            fs::remove_file(path).unwrap();
            removed += 1;
        }
    }
    assert_eq!(removed, 1);
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    let recovered = f.clone_repo("after-pointer-only-cleanup");
    assert!(!recovered.join("a.bin").exists());
    f.git(&recovered, &["checkout", &second]);
    assert_eq!(fs::read(recovered.join("a.bin")).unwrap(), data);
    f.git(&recovered, &["checkout", &first]);
    data[1_234_567] ^= 1;
    assert_eq!(fs::read(recovered.join("a.bin")).unwrap(), data);
}

#[test]
fn retention_ref_rejection_blocks_user_refs_and_preserves_server_hooks() {
    let f = Fixture::new();
    let hook = f.remote.join("hooks/pre-receive");
    fs::write(
        &hook,
        r#"#!/bin/sh
while read old new ref; do
case "$ref" in refs/notes/git-cdc-lfs/*) echo 'retention rejected by server' >&2; exit 1;; esac
done
"#,
    )
    .unwrap();
    make_executable(&hook);
    f.write("a.bin", b"all chunks upload but pins must also succeed\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("server retention rejection");
    let out = f.run(&f.repo, &["push", "origin", "main"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("retention rejected by server"));
    assert!(
        f.git(&f.remote, &["for-each-ref", "--format=%(refname)"])
            .is_empty()
    );
    assert!(!f.objects().is_empty());
    fs::remove_file(hook).unwrap();
    f.push();
    let recovered = f.clone_repo("after-server-policy-restored");
    assert_eq!(
        fs::read(recovered.join("a.bin")).unwrap(),
        b"all chunks upload but pins must also succeed\n"
    );
}

#[test]
fn retention_mirror_from_normal_clone_preserves_even_deletion_only_pins() {
    retention_deletion_only_mirror(false);
}

#[test]
fn retention_mirror_with_separate_fetch_and_push_urls_preserves_pins() {
    retention_deletion_only_mirror(true);
}

fn retention_deletion_only_mirror(separate_fetch_url: bool) {
    let f = Fixture::new();
    f.publish_file("a.bin", b"mirror retained data\n", "mirror source");
    let original_roots = stock_lfs_roots(&f);
    // Use the transport even for a local path so unadvertised retention
    // objects are not copied through the local clone optimization.
    let clone = f.temp.path().join("ordinary-not-mirror-clone");
    f.git(
        f.temp.path(),
        &[
            "clone",
            "--no-local",
            f.remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    f.identity(&clone);
    for root in f
        .git(
            &f.remote,
            &["for-each-ref", "--format=%(objectname)", RETENTION_PREFIX],
        )
        .lines()
    {
        assert!(!f.run(&clone, &["cat-file", "-e", root]).status.success());
    }
    if separate_fetch_url {
        // A bare clone copies user branches but not retention refs. Only the
        // push destination has the roots and objects that the hook must fetch.
        let fetch_remote = f.temp.path().join("fetch-only.git");
        f.git(
            f.temp.path(),
            &[
                "clone",
                "--bare",
                f.remote.to_str().unwrap(),
                fetch_remote.to_str().unwrap(),
            ],
        );
        assert!(f.refs(&fetch_remote, RETENTION_PREFIX).is_empty());
        f.git(
            &clone,
            &[
                "remote",
                "set-url",
                "origin",
                fetch_remote.to_str().unwrap(),
            ],
        );
        f.git(
            &clone,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                f.remote.to_str().unwrap(),
            ],
        );
    }
    assert!(f.refs(&clone, RETENTION_PREFIX).is_empty());
    // Make the mirror's only changes ref deletions, including remote pins.
    // Its main branch already matches and therefore supplies no pushed tip.
    for reference in f.refs(&clone, "refs/remotes") {
        f.git(&clone, &["update-ref", "-d", &reference]);
    }
    fs::remove_dir_all(clone.join(".git/cdc/lfs")).unwrap();
    f.git(&clone, &["push", "--mirror", "origin"]);
    assert_eq!(stock_lfs_roots(&f), original_roots);
    let recovered = f.clone_repo("after-ordinary-clone-mirror");
    assert_eq!(
        fs::read(recovered.join("a.bin")).unwrap(),
        b"mirror retained data\n"
    );
}

#[test]
fn retention_mirror_clone_can_publish_pins_with_implicit_mirror_mode() {
    let f = Fixture::new();
    f.publish_file(
        "a.bin",
        b"mirror clone retained data\n",
        "mirror clone source",
    );
    let original_roots = stock_lfs_roots(&f);
    let clone = f.temp.path().join("mirror.git");
    f.git(
        f.temp.path(),
        &[
            "clone",
            "--mirror",
            f.remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(
        f.git(&clone, &["config", "--bool", "remote.origin.mirror"])
            .trim(),
        "true"
    );
    // Deleting the local pins makes the outer mirror delete the original
    // remote refs. Its nested single-ref push must publish a fresh descendant.
    let original_pins = f.refs(&clone, RETENTION_PREFIX);
    assert!(!original_pins.is_empty());
    for reference in &original_pins {
        f.git(&clone, &["update-ref", "-d", reference]);
    }
    f.git(&clone, &["push", "origin"]);
    assert_eq!(
        f.git(&clone, &["config", "--bool", "remote.origin.mirror"])
            .trim(),
        "true"
    );
    assert_eq!(stock_lfs_roots(&f), original_roots);
    for reference in &original_pins {
        assert!(
            !f.run(&f.remote, &["show-ref", "--verify", reference])
                .status
                .success()
        );
    }
    let recovered = f.clone_repo("after-mirror-clone-push");
    assert_eq!(
        fs::read(recovered.join("a.bin")).unwrap(),
        b"mirror clone retained data\n"
    );
}

#[test]
fn retention_mirror_transfers_chunks_from_deleted_unmerged_history() {
    let f = Fixture::new();
    // This needs independent object stores; the HTTP fixture's .lfsconfig
    // intentionally shares one endpoint between all Git remotes.
    if f.endpoint.is_some() {
        return;
    }
    f.publish_file("base.bin", b"main branch data\n", "main history");
    f.git(&f.repo, &["switch", "-c", "unmerged"]);
    let data = test_data(2 * 1024 * 1024, 431);
    f.write("unmerged.bin", &data);
    f.git(&f.repo, &["add", "unmerged.bin"]);
    let unmerged = f.commit("history retained only by pointers");
    let manifest = f.manifest("HEAD:unmerged.bin");
    f.git(&f.repo, &["push", "origin", "unmerged"]);
    f.git(&f.repo, &["switch", "main"]);
    f.git(&f.repo, &["branch", "-D", "unmerged"]);
    f.git(&f.repo, &["push", "origin", ":refs/heads/unmerged"]);
    f.publish_file(
        "base.bin",
        b"updated main branch data\n",
        "advance main independently of the deleted branch",
    );
    assert!(
        !f.git(&f.repo, &["rev-list", "--all"])
            .lines()
            .any(|commit| commit == unmerged)
    );
    let roots = stock_lfs_roots(&f);
    let backup = f.temp.path().join("backup.git");
    f.git(f.temp.path(), &["init", "--bare", backup.to_str().unwrap()]);
    f.git(
        &f.repo,
        &["remote", "add", "backup", backup.to_str().unwrap()],
    );
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    f.git(&f.repo, &["push", "--mirror", "backup"]);

    // A fresh mirror must retrieve every advertised pointer object using
    // only the backup, including chunks with no remaining CDC recipe.
    let recovered = f.temp.path().join("recovered-mirror.git");
    f.git(
        f.temp.path(),
        &[
            "clone",
            "--mirror",
            backup.to_str().unwrap(),
            recovered.to_str().unwrap(),
        ],
    );
    let backup_roots: std::collections::BTreeSet<_> = f
        .git(&backup, &["lfs", "ls-files", "--all", "--long"])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect();
    assert_eq!(backup_roots, roots);
    f.git(
        &recovered,
        &[
            "-c",
            "lfs.storage=cdc/lfs",
            "lfs",
            "fetch",
            "--all",
            "origin",
        ],
    );
    for chunk in &manifest.chunks {
        let oid = &manifest.extra[&format!("lfs.{}", chunk.hash.to_hex())];
        let path = recovered
            .join("cdc/lfs/objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(oid);
        let start = chunk.offset as usize;
        assert_eq!(
            fs::read(path).unwrap(),
            data[start..start + chunk.length as usize]
        );
    }

    // If a retained chunk is unavailable everywhere, the gate must fail
    // before any user or managed ref is published to another empty remote.
    for chunk in &manifest.chunks {
        let oid = &manifest.extra[&format!("lfs.{}", chunk.hash.to_hex())];
        let path = f.content.join(&oid[..2]).join(&oid[2..4]).join(oid);
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    let missing = f.temp.path().join("missing-objects.git");
    f.git(
        f.temp.path(),
        &["init", "--bare", missing.to_str().unwrap()],
    );
    let out = f.run(&f.repo, &["push", "--mirror", missing.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("LFS download failed"));
    assert!(f.refs(&missing, "refs").is_empty());
}

#[test]
fn cold_cache_push_recovers_from_target_despite_origin_tracking() {
    let f = Fixture::new();
    if f.endpoint.is_some() {
        return;
    }
    f.publish_file(
        "a.bin",
        b"available only at the backup\n",
        "initial content",
    );
    let backup = f.temp.path().join("backup.git");
    f.git(
        f.temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            backup.to_str().unwrap(),
        ],
    );
    f.git(
        &f.repo,
        &["remote", "add", "backup", backup.to_str().unwrap()],
    );
    f.git(&f.repo, &["push", "backup", "main"]);
    fs::remove_dir_all(&f.content).unwrap();
    for target in ["backup", backup.to_str().unwrap()] {
        f.write("note.txt", target.as_bytes());
        f.git(&f.repo, &["add", "note.txt"]);
        f.commit("recover from target with an evicted cache");
        fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
        f.git(&f.repo, &["push", target, "main"]);
        assert_eq!(
            f.git(&f.repo, &["config", "branch.main.remote"]).trim(),
            "origin"
        );
    }
    let recovered = f.temp.path().join("backup-recovery");
    f.git(
        f.temp.path(),
        &[
            "clone",
            backup.to_str().unwrap(),
            recovered.to_str().unwrap(),
        ],
    );
    assert_eq!(
        fs::read(recovered.join("a.bin")).unwrap(),
        b"available only at the backup\n"
    );
}

#[test]
fn cold_cache_push_negotiates_existing_objects_without_downloading_them() {
    let f = Fixture::new();
    let data = test_data(6 * 1024 * 1024, 451);
    f.publish_file("a.bin", &data, "published content");
    f.write("note.txt", b"metadata-only update\n");
    f.git(&f.repo, &["add", "note.txt"]);
    f.commit("no new chunk content");
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    let requests = || {
        f.endpoint.as_ref().map(|_| {
            fs::read_to_string(f.temp.path().join("server.log"))
                .unwrap()
                .lines()
                .filter(|line| line.contains("method=GET") || line.contains("method=PUT"))
                .count()
        })
    };
    let before = requests();
    let out = f.run(&f.repo, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        requests(),
        before,
        "existing objects must not be transferred"
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("Downloading"));
    assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
    assert!(
        f.refs(&f.repo, &format!("{RETENTION_PREFIX}/pending-*"))
            .is_empty()
    );
    fs::remove_file(f.repo.join("a.bin")).unwrap();
    f.git(&f.repo, &["restore", "a.bin"]);
    assert_eq!(fs::read(f.repo.join("a.bin")).unwrap(), data);
}

#[test]
fn http_cold_push_recovers_only_objects_missing_at_the_destination() {
    if std::env::var_os("GIT_CDC_LFS_TEST_SERVER").is_none() {
        return;
    }
    let f = Fixture::new();
    let backup = Fixture::new();
    let endpoint = backup.endpoint.as_ref().unwrap();
    let data = test_data(3 * 1024 * 1024, 453);
    f.publish_file("a.bin", &data, "source endpoint content");
    let objects = f.objects();
    assert!(objects.len() > 1);
    let first = object_name(objects.keys().next().unwrap());
    f.git(
        &f.repo,
        &[
            "-c",
            &format!("lfs.pushurl={endpoint}"),
            "-c",
            "lfs.storage=cdc/lfs",
            "lfs",
            "push",
            "--object-id",
            "origin",
            &first,
        ],
    );
    f.git(
        &f.repo,
        &["remote", "add", "backup", backup.remote.to_str().unwrap()],
    );
    f.git(&f.repo, &["config", "lfs.pushurl", endpoint]);
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    let downloads = || {
        fs::read_to_string(f.temp.path().join("server.log"))
            .unwrap()
            .lines()
            .filter(|line| line.contains("method=GET"))
            .count()
    };
    let before = downloads();
    f.git(&f.repo, &["push", "backup", "main"]);
    assert_eq!(downloads() - before, objects.len() - 1);
    assert_eq!(backup.objects().len(), objects.len());
    assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
    let recovered = f.temp.path().join("http-backup-recovery");
    f.git(
        f.temp.path(),
        &[
            "-c",
            &format!("lfs.url={endpoint}"),
            "clone",
            backup.remote.to_str().unwrap(),
            recovered.to_str().unwrap(),
        ],
    );
    assert_eq!(fs::read(recovered.join("a.bin")).unwrap(), data);
}

#[test]
fn retention_reference_hook_can_clean_content_after_transfer_leases_release() {
    let f = Fixture::new();
    let hook = f.repo.join(".git/hooks/reference-transaction");
    let record = f.temp.path().join("reference-clean-output");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\n[ \"$1\" = prepared ] || exit 0\nwhile read old new ref; do\ncase \"$ref\" in refs/notes/git-cdc-lfs/*) git-cdc lfs-clean <a.bin >> '{}' || exit 1;; esac\ndone\n",
            record.display()
        ),
    ).unwrap();
    make_executable(&hook);
    f.publish_file(
        "a.bin",
        b"reference hooks may read and clean the cache\n",
        "cache-using reference hook",
    );
    assert!(
        fs::read(record)
            .unwrap()
            .starts_with(b"version git-cdc/spec/v1\n")
    );
}

#[test]
fn rejected_cold_transfer_keeps_fetched_objects_until_retention_succeeds() {
    let f = Fixture::new();
    if f.endpoint.is_some() {
        return;
    }
    f.publish_file(
        "a.bin",
        b"keep fetched data when pin publication fails\n",
        "source content",
    );
    let backup = f.temp.path().join("reject-cold.git");
    f.git(f.temp.path(), &["init", "--bare", backup.to_str().unwrap()]);
    f.git(
        &f.repo,
        &["remote", "add", "backup", backup.to_str().unwrap()],
    );
    let hook = backup.join("hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\ncat >/dev/null\nexit 1\n").unwrap();
    make_executable(&hook);
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    let out = f.run(&f.repo, &["push", "backup", "main"]);
    assert!(!out.status.success());
    assert!(f.refs(&backup, "refs").is_empty());
    assert!(!objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
    assert!(
        f.refs(&f.repo, &format!("{RETENTION_PREFIX}/pending-*"))
            .is_empty()
    );
    fs::remove_file(hook).unwrap();
    f.git(&f.repo, &["push", "backup", "main"]);
    assert!(!f.refs(&backup, RETENTION_PREFIX).is_empty());
}

#[test]
fn cold_mirror_uses_bounded_batches_and_evicts_only_fetched_chunks() {
    let f = Fixture::new();
    if f.endpoint.is_some() {
        return; // Separate file remotes provide independent source/destination stores.
    }
    let data = test_data(80 * 1024 * 1024, 457);
    f.publish_file("a.bin", &data, "more content than the transfer budget");
    let backup = f.temp.path().join("bounded-backup.git");
    f.git(f.temp.path(), &["init", "--bare", backup.to_str().unwrap()]);
    f.git(
        &f.repo,
        &["remote", "add", "backup", backup.to_str().unwrap()],
    );
    let cache = f.repo.join(".git/cdc/lfs/objects");
    let peaks = f.temp.path().join("cache-peaks");
    let hook = backup.join("hooks/pre-receive");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\npython3 - '{}' '{}' <<'PY'\nimport os,sys\nsize=sum(os.path.getsize(os.path.join(d,n)) for d,_,files in os.walk(sys.argv[1]) for n in files)\nwith open(sys.argv[2],'a') as out: out.write(str(size)+'\\n')\nPY\ncat >/dev/null\n",
            cache.display(), peaks.display()
        ),
    ).unwrap();
    make_executable(&hook);
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs")).unwrap();
    // Retain one pre-existing local object to prove eviction is conservative.
    let manifest = f.manifest("HEAD:a.bin");
    let chunk = &manifest.chunks[0];
    let oid = &manifest.extra[&format!("lfs.{}", chunk.hash.to_hex())];
    let path = cache.join(&oid[..2]).join(&oid[2..4]).join(oid);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let start = chunk.offset as usize;
    fs::write(&path, &data[start..start + chunk.length as usize]).unwrap();
    f.git(&f.repo, &["push", "--mirror", "backup"]);
    let sizes: Vec<u64> = fs::read_to_string(peaks)
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect();
    assert!(sizes.len() > 1, "the transfer must span several batches");
    assert!(sizes.iter().all(|size| *size <= 64 * 1024 * 1024));
    assert_eq!(objects_at(&cache).len(), 1);
    assert!(path.is_file(), "pre-existing objects must remain cached");
    let recovered = f.temp.path().join("bounded-recovery");
    f.git(
        f.temp.path(),
        &[
            "clone",
            "-b",
            "main",
            backup.to_str().unwrap(),
            recovered.to_str().unwrap(),
        ],
    );
    assert_eq!(fs::read(recovered.join("a.bin")).unwrap(), data);
}

#[test]
fn retention_dry_run_prepares_storage_without_publishing_branches_or_tags() {
    let f = Fixture::new();
    f.write("a.bin", b"dry run storage preparation\n");
    f.git(&f.repo, &["add", "a.bin"]);
    f.commit("dry run");
    f.git(&f.repo, &["push", "--dry-run", "origin", "main"]);
    assert!(
        f.git(
            &f.remote,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads",
                "refs/tags"
            ]
        )
        .is_empty()
    );
    assert!(!stock_lfs_roots(&f).is_empty());
    let pins = f.refs(&f.remote, RETENTION_PREFIX);
    f.push();
    assert!(f.refs(&f.remote, RETENTION_PREFIX).len() >= pins.len());
}

#[test]
fn retention_identical_content_from_independent_clones_keeps_fresh_pins() {
    let f = Fixture::new();
    f.publish_file("base.bin", b"base\n", "base");
    let a = f.clone_repo("same-content-a");
    let b = f.clone_repo("same-content-b");
    let data = test_data(1024 * 1024, 403);
    for repo in [&a, &b] {
        fs::write(repo.join("shared.bin"), &data).unwrap();
        f.git(repo, &["add", "shared.bin"]);
        f.git(repo, &["commit", "-qm", "same content"]);
    }
    let mut a_cmd = f.command(&a, "git");
    a_cmd.args(["push", "origin", "HEAD:refs/heads/a"]);
    let mut b_cmd = f.command(&b, "git");
    b_cmd.args(["push", "origin", "HEAD:refs/heads/b"]);
    let (a_result, b_result) = concurrent_commands(a_cmd, b_cmd);
    assert!(
        a_result.status.success(),
        "{}",
        String::from_utf8_lossy(&a_result.stderr)
    );
    assert!(
        b_result.status.success(),
        "{}",
        String::from_utf8_lossy(&b_result.stderr)
    );
    let roots = f.refs(&f.remote, RETENTION_PREFIX);
    // Each publisher creates a fresh pin; identical chunk bytes still dedup.
    assert_eq!(roots.len(), 3);
    assert_eq!(
        f.objects().values().sum::<u64>(),
        (b"base\n".len() + data.len()) as u64
    );
}

// Lifecycle checks use the unmodified reference HTTP server so verification
// exercises the stock client's real remote checks.
#[test]
fn lifecycle_lfs_drop_and_pull_reclaim_disk_and_preserve_the_index() {
    let f = Fixture::http();
    let data = test_data(3 * 1024 * 1024, 701);
    f.publish_file("asset.bin", &data, "published lifecycle content");
    let recipe = f.git(&f.repo, &["show", ":asset.bin"]);
    let index = f.git(&f.repo, &["ls-files", "--stage"]);
    let remote = f.objects();

    f.cdc(&f.repo, &["drop", "--chunks", "asset.bin"]);
    assert_eq!(
        fs::read(f.repo.join("asset.bin")).unwrap(),
        recipe.as_bytes()
    );
    assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
    assert_eq!(f.git(&f.repo, &["ls-files", "--stage"]), index);
    assert!(f.git(&f.repo, &["status", "--porcelain"]).is_empty());
    assert_eq!(f.objects(), remote, "drop only evicts local objects");

    f.cdc(&f.repo, &["pull", "--", "asset.bin"]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
    assert_eq!(f.git(&f.repo, &["ls-files", "--stage"]), index);
    assert!(f.git(&f.repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn lifecycle_lfs_pull_restores_only_the_selected_indexed_file() {
    let f = Fixture::http();
    let a = test_data(1024 * 1024, 703);
    let b = test_data(1024 * 1024, 705);
    f.write("a.bin", &a);
    f.write("b.bin", &b);
    f.git(&f.repo, &["add", "a.bin", "b.bin"]);
    f.commit("two indexed recipes");
    f.push();
    let first = f.git(&f.repo, &["show", ":a.bin"]);
    let second = f.git(&f.repo, &["show", ":b.bin"]);
    let index = f.git(&f.repo, &["ls-files", "--stage"]);
    f.cdc(&f.repo, &["drop", "--all"]);
    assert_eq!(fs::read(f.repo.join("a.bin")).unwrap(), first.as_bytes());
    assert_eq!(fs::read(f.repo.join("b.bin")).unwrap(), second.as_bytes());
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs/objects")).unwrap();

    f.cdc(&f.repo, &["pull", "--", "a.bin"]);
    assert_eq!(fs::read(f.repo.join("a.bin")).unwrap(), a);
    assert_eq!(fs::read(f.repo.join("b.bin")).unwrap(), second.as_bytes());
    assert_eq!(f.git(&f.repo, &["ls-files", "--stage"]), index);
    assert!(f.git(&f.repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn lifecycle_lfs_drop_dry_run_preserves_content_cache_and_refs() {
    let f = Fixture::http();
    let data = test_data(1024 * 1024, 707);
    f.publish_file("asset.bin", &data, "dry run lifecycle");
    let cache = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    let index = f.git(&f.repo, &["ls-files", "--stage"]);
    let refs = f.git(
        &f.remote,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );

    f.cdc(&f.repo, &["drop", "--chunks", "--dry-run", "asset.bin"]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
    assert_eq!(objects_at(&f.repo.join(".git/cdc/lfs/objects")), cache);
    assert_eq!(f.git(&f.repo, &["ls-files", "--stage"]), index);
    assert_eq!(
        f.git(
            &f.remote,
            &["for-each-ref", "--format=%(objectname) %(refname)"]
        ),
        refs
    );
}

#[test]
fn lifecycle_lfs_prune_reclaims_published_history_and_restores_it_on_checkout() {
    let f = Fixture::http();
    let old = test_data(1024 * 1024, 709);
    let current = test_data(1024 * 1024, 711);
    f.write("asset.bin", &old);
    f.git(&f.repo, &["add", "asset.bin"]);
    let first = f.commit("old published content");
    f.push();
    let old_objects = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    f.publish_file("asset.bin", &current, "current published content");
    let cache = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    let index = f.git(&f.repo, &["ls-files", "--stage"]);
    let remote = f.objects();

    f.cdc(&f.repo, &["prune", "--dry-run"]);
    assert_eq!(objects_at(&f.repo.join(".git/cdc/lfs/objects")), cache);
    f.cdc(&f.repo, &["prune"]);
    let remaining = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    assert!(old_objects.keys().all(|path| !remaining.contains_key(path)));
    assert!(!remaining.is_empty(), "the current index remains cached");
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), current);
    assert_eq!(f.git(&f.repo, &["ls-files", "--stage"]), index);
    assert_eq!(f.objects(), remote);
    assert!(f.git(&f.repo, &["status", "--porcelain"]).is_empty());
    f.git(&f.repo, &["checkout", "--detach", &first]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), old);
}

#[test]
fn lifecycle_mixed_backends_share_drop_and_path_selective_pull() {
    let f = Fixture::http();
    f.cdc(f.temp.path(), &["install", "--global"]);
    let legacy_remote = f.temp.path().join("legacy-objects");
    f.git(&f.repo, &["config", "cdc.store.scheme", "fs"]);
    f.git(
        &f.repo,
        &[
            "config",
            "cdc.store.option",
            &format!("root={}", legacy_remote.display()),
        ],
    );
    let mut attributes = fs::read_to_string(f.repo.join(".gitattributes")).unwrap();
    attributes.push_str("legacy.dat filter=cdc -text\n");
    fs::write(f.repo.join(".gitattributes"), attributes).unwrap();
    let legacy = test_data(1024 * 1024, 713);
    let lfs = test_data(1024 * 1024, 715);
    f.write("legacy.dat", &legacy);
    f.write("asset.bin", &lfs);
    f.git(
        &f.repo,
        &["add", ".gitattributes", "legacy.dat", "asset.bin"],
    );
    f.commit("mixed CDC backends");
    f.cdc(&f.repo, &["push"]);
    f.push();
    let index = f.git(&f.repo, &["ls-files", "--stage"]);
    let legacy_recipe = f.git(&f.repo, &["show", ":legacy.dat"]);
    let lfs_recipe = f.git(&f.repo, &["show", ":asset.bin"]);

    f.cdc(&f.repo, &["drop", "--chunks", "--all"]);
    assert_eq!(
        fs::read(f.repo.join("legacy.dat")).unwrap(),
        legacy_recipe.as_bytes()
    );
    assert_eq!(
        fs::read(f.repo.join("asset.bin")).unwrap(),
        lfs_recipe.as_bytes()
    );
    assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
    f.cdc(&f.repo, &["pull", "--", "legacy.dat"]);
    assert_eq!(fs::read(f.repo.join("legacy.dat")).unwrap(), legacy);
    assert_eq!(
        fs::read(f.repo.join("asset.bin")).unwrap(),
        lfs_recipe.as_bytes()
    );
    f.cdc(&f.repo, &["pull", "--", "asset.bin"]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), lfs);
    assert_eq!(f.git(&f.repo, &["ls-files", "--stage"]), index);
    assert!(f.git(&f.repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn lifecycle_pointer_only_checkout_keeps_recipes_without_fetching_chunks() {
    let f = Fixture::http();
    f.publish_file(
        "asset.bin",
        b"explicit hydration only\n",
        "pointer-only clone source",
    );
    let recipe = f.git(&f.repo, &["show", "HEAD:asset.bin"]);
    let clone = f.temp.path().join("pointer-only");
    f.git(
        f.temp.path(),
        &[
            "clone",
            "--no-checkout",
            f.remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    f.git(&clone, &["config", "cdc.hydrate", "false"]);
    f.git(&clone, &["checkout", "main"]);
    assert_eq!(
        fs::read(clone.join("asset.bin")).unwrap(),
        recipe.as_bytes()
    );
    assert!(objects_at(&clone.join(".git/cdc/lfs/objects")).is_empty());
    assert!(f.git(&clone, &["status", "--porcelain"]).is_empty());
}

#[test]
fn lifecycle_http_eviction_needs_both_remote_content_and_retention() {
    for remove_content in [true, false] {
        let f = Fixture::http();
        f.publish_file(
            "asset.bin",
            b"remote backing must actually exist\n",
            "remote backing",
        );
        if remove_content {
            for path in f.objects().keys() {
                fs::remove_file(path).unwrap();
            }
        } else {
            for reference in f.refs(&f.remote, RETENTION_PREFIX) {
                f.git(&f.remote, &["update-ref", "-d", &reference]);
            }
        }
        let cache = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
        let refs = f.refs(&f.remote, "refs");
        f.cdc(&f.repo, &["drop", "--chunks", "asset.bin"]);
        assert_eq!(objects_at(&f.repo.join(".git/cdc/lfs/objects")), cache);
        assert_eq!(f.refs(&f.remote, "refs"), refs);
    }
}

#[test]
fn lifecycle_http_verification_batches_keep_the_missing_objects_cached() {
    let f = Fixture::http();
    for key in ["cdc.chunk.min", "cdc.chunk.avg", "cdc.chunk.max"] {
        f.git(&f.repo, &["config", key, "64k"]);
    }
    f.publish_file(
        "asset.bin",
        &test_data(130 * 64 * 1024, 727),
        "more than one verification batch",
    );
    let remote = f.objects();
    assert!(remote.len() > 128);
    let missing_path = remote.keys().next().unwrap();
    let missing = object_name(missing_path);
    fs::remove_file(missing_path).unwrap();
    f.cdc(&f.repo, &["drop", "--chunks", "asset.bin"]);
    let remaining = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    assert!(remaining.keys().any(|path| object_name(path) == missing));
    assert!(
        remaining.len() < remote.len(),
        "successful batches are evicted"
    );
}

#[cfg(unix)]
#[test]
fn lifecycle_http_prune_preserves_foreign_files_and_symlinked_objects() {
    use std::os::unix::fs::symlink;
    let f = Fixture::http();
    f.publish_file(
        "asset.bin",
        &test_data(2 * 1024 * 1024, 729),
        "old cached objects",
    );
    let old = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    f.publish_file(
        "asset.bin",
        &test_data(2 * 1024 * 1024, 731),
        "current cached objects",
    );
    let path = old.keys().next().unwrap();
    let outside = f.temp.path().join("outside-cache");
    fs::write(&outside, b"outside contents\n").unwrap();
    fs::remove_file(path).unwrap();
    symlink(&outside, path).unwrap();
    let foreign = f.repo.join(".git/cdc/lfs/objects/foreign.tmp");
    fs::write(&foreign, b"foreign contents\n").unwrap();
    f.cdc(&f.repo, &["prune"]);
    assert!(fs::symlink_metadata(path).unwrap().is_symlink());
    assert_eq!(fs::read(outside).unwrap(), b"outside contents\n");
    assert_eq!(fs::read(foreign).unwrap(), b"foreign contents\n");
}

#[test]
fn lifecycle_http_verification_checks_candidates_despite_unpushed_stock_pointers() {
    let f = Fixture::http();
    let data = b"one shared LFS object\n";
    f.publish_file("asset.bin", data, "published CDC object");
    f.git(&f.repo, &["lfs", "install", "--skip-repo"]);
    f.git(&f.repo, &["lfs", "track", "*.raw"]);
    f.write("unpushed.raw", data);
    f.git(&f.repo, &["add", ".gitattributes", "unpushed.raw"]);
    f.commit("unpushed stock pointer protects the same object in real prune");
    for path in f.objects().keys() {
        fs::remove_file(path).unwrap();
    }
    let cache = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    let native = objects_at(&f.repo.join(".git/lfs/objects"));
    f.cdc(&f.repo, &["drop", "--chunks", "asset.bin"]);
    assert_eq!(objects_at(&f.repo.join(".git/cdc/lfs/objects")), cache);
    assert_eq!(objects_at(&f.repo.join(".git/lfs/objects")), native);
}

#[test]
fn lifecycle_http_reads_do_not_download_during_drop_planning() {
    let f = Fixture::http();
    let data = test_data(1024 * 1024, 721);
    f.publish_file("asset.bin", &data, "cached reads only");
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs/objects")).unwrap();
    let requests = fs::read_to_string(f.temp.path().join("server.log")).unwrap();
    let out = f
        .command(&f.repo, BIN)
        .args(["drop", "--dry-run", "asset.bin"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not cached"));
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
    assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
    assert_eq!(
        fs::read_to_string(f.temp.path().join("server.log")).unwrap(),
        requests
    );
}

#[test]
fn lifecycle_http_eviction_and_restore_use_the_tracking_remote() {
    let f = Fixture::http();
    let backup = Fixture::http();
    let data = test_data(1024 * 1024, 723);
    f.publish_file("asset.bin", &data, "initial backing");
    f.git(&f.repo, &["rm", ".lfsconfig"]);
    f.git(
        &f.repo,
        &[
            "config",
            "remote.origin.lfsurl",
            f.endpoint.as_ref().unwrap(),
        ],
    );
    f.git(
        &f.repo,
        &["remote", "add", "backup", backup.remote.to_str().unwrap()],
    );
    f.git(
        &f.repo,
        &[
            "config",
            "remote.backup.lfsurl",
            backup.endpoint.as_ref().unwrap(),
        ],
    );
    f.commit("per-remote read endpoints");
    f.git(&f.repo, &["push", "backup", "main"]);
    f.git(&f.repo, &["config", "branch.main.remote", "backup"]);
    for reference in f.refs(&f.remote, RETENTION_PREFIX) {
        f.git(&f.remote, &["update-ref", "-d", &reference]);
    }
    f.cdc(&f.repo, &["drop", "--chunks", "asset.bin"]);
    assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
    f.cdc(&f.repo, &["pull", "--", "asset.bin"]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
}

#[test]
fn lifecycle_file_eviction_requires_confirmation_and_cold_pull_still_works() {
    let f = Fixture::file();
    let data = b"file endpoint needs an explicit eviction decision\n";
    f.publish_file("asset.bin", data, "file endpoint");
    let cache = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    f.cdc(&f.repo, &["drop", "--chunks", "--dry-run", "asset.bin"]);
    let out = f
        .command(&f.repo, BIN)
        .args(["drop", "--chunks", "asset.bin"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("file:// cache eviction requires interactive confirmation")
    );
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
    assert_eq!(objects_at(&f.repo.join(".git/cdc/lfs/objects")), cache);
    f.cdc(&f.repo, &["drop", "asset.bin"]);
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs/objects")).unwrap();
    f.cdc(&f.repo, &["pull", "--", "asset.bin"]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
}

#[cfg(target_os = "linux")]
#[test]
fn lifecycle_file_confirmation_accepts_yes_and_preserves_files_on_no() {
    let f = Fixture::file();
    let data = b"file eviction confirmation\n";
    f.publish_file("asset.bin", data, "interactive confirmation");
    for (answer, accepted) in [("no\n", false), ("yes\n", true)] {
        let out = f.cdc_interactive(&["drop", "--chunks", "asset.bin"], answer);
        assert_eq!(
            out.status.success(),
            accepted,
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        if accepted {
            assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
        } else {
            assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
            assert!(!objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
        }
    }
}

#[test]
fn lifecycle_edge_sha256_repositories_support_drop_prune_and_pull() {
    let f = Fixture::http_with_format("sha256");
    f.publish_file(
        "asset.bin",
        &test_data(1024 * 1024, 741),
        "old SHA-256 Git history",
    );
    let data = test_data(1024 * 1024, 743);
    f.publish_file("asset.bin", &data, "current SHA-256 Git history");
    assert_eq!(
        f.git(&f.repo, &["rev-parse", "--show-object-format"])
            .trim(),
        "sha256"
    );
    f.cdc(&f.repo, &["prune"]);
    f.cdc(&f.repo, &["drop", "--chunks", "asset.bin"]);
    assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
    f.cdc(&f.repo, &["pull", "asset.bin"]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
}

#[test]
fn lifecycle_edge_url_rewrites_are_applied_once() {
    for (http, endpoint_key) in [
        (true, None),
        (false, None),
        (false, Some("lfs.url")),
        (false, Some("remote.origin.lfsurl")),
        (false, Some(".lfsconfig")),
    ] {
        let mut f = if http {
            Fixture::http()
        } else {
            Fixture::file()
        };
        let data = b"one URL rewrite\n";
        f.publish_file("asset.bin", data, "published before configuring a rewrite");
        let base = f.temp.path().join("base");
        let prefix = base.join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        let destination = prefix.join("remote.git");
        fs::rename(&f.remote, &destination).unwrap();
        f.remote = destination;
        f.git(
            &f.repo,
            &[
                "config",
                &format!("url.{}/.insteadOf", prefix.display()),
                &format!("{}/", base.display()),
            ],
        );
        f.git(
            &f.repo,
            &[
                "remote",
                "set-url",
                "origin",
                base.join("remote.git").to_str().unwrap(),
            ],
        );
        assert_eq!(
            f.git(&f.repo, &["remote", "get-url", "origin"]).trim(),
            f.remote.to_str().unwrap()
        );
        if let Some(key) = endpoint_key {
            let raw_url = base.join("remote.git");
            if key == ".lfsconfig" {
                f.git(
                    &f.repo,
                    &[
                        "config",
                        "--file",
                        ".lfsconfig",
                        "lfs.url",
                        raw_url.to_str().unwrap(),
                    ],
                );
            } else {
                f.git(&f.repo, &["config", key, raw_url.to_str().unwrap()]);
            }
        }
        if http {
            f.cdc(&f.repo, &["drop", "--chunks", "asset.bin"]);
            assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
        } else {
            f.cdc(&f.repo, &["drop", "asset.bin"]);
            fs::remove_dir_all(f.repo.join(".git/cdc/lfs/objects")).unwrap();
        }
        f.cdc(&f.repo, &["pull", "asset.bin"]);
        assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
    }
}

#[test]
fn lifecycle_edge_relative_lfs_urls_keep_the_source_directory() {
    for location in [
        "lfs.url",
        "remote.origin.lfsurl",
        "worktree",
        "index",
        "HEAD",
    ] {
        let f = Fixture::file();
        let data = b"relative LFS endpoint\n";
        f.publish_file("asset.bin", data, "published file endpoint");
        if location.ends_with("url") {
            f.git(&f.repo, &["config", location, "../remote.git"]);
        } else {
            fs::write(f.repo.join(".lfsconfig"), "[lfs]\nurl = ../remote.git\n").unwrap();
            f.git(&f.repo, &["add", ".lfsconfig"]);
            f.commit("relative endpoint in .lfsconfig");
            if location != "worktree" {
                fs::remove_file(f.repo.join(".lfsconfig")).unwrap();
            }
            if location == "HEAD" {
                f.git(&f.repo, &["rm", "--cached", ".lfsconfig"]);
            }
        }
        f.assert_cold_restoration("asset.bin", data);
    }
}

#[test]
fn lifecycle_edge_ambiguous_branch_names_keep_the_tracking_remote() {
    let f = Fixture::http();
    let backup = Fixture::http();
    let data = b"read from the tracking remote\n";
    f.publish_file("asset.bin", data, "initial content");
    f.git(&f.repo, &["rm", ".lfsconfig"]);
    f.git(
        &f.repo,
        &[
            "config",
            "remote.origin.lfsurl",
            f.endpoint.as_ref().unwrap(),
        ],
    );
    f.git(
        &f.repo,
        &["remote", "add", "backup", backup.remote.to_str().unwrap()],
    );
    f.git(
        &f.repo,
        &[
            "config",
            "remote.backup.lfsurl",
            backup.endpoint.as_ref().unwrap(),
        ],
    );
    f.commit("per-remote endpoints");
    f.git(&f.repo, &["push", "backup", "main"]);
    f.git(&f.repo, &["config", "branch.main.remote", "backup"]);
    f.git(&f.repo, &["tag", "main"]);
    assert_eq!(
        f.git(&f.repo, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "heads/main"
    );
    for path in f.objects().keys() {
        fs::remove_file(path).unwrap();
    }
    f.cdc(&f.repo, &["drop", "asset.bin"]);
    fs::remove_dir_all(f.repo.join(".git/cdc/lfs/objects")).unwrap();
    f.cdc(&f.repo, &["pull", "asset.bin"]);
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
}

#[cfg(unix)]
#[test]
fn lifecycle_edge_standalone_agents_require_explicit_confirmation() {
    for scope in ["global", "url", "git-suffix"] {
        let f = Fixture::http();
        let data = b"only locally cached content remains\n";
        f.publish_file(
            "asset.bin",
            data,
            "publish before enabling standalone transfers",
        );
        for path in f.objects().keys() {
            fs::remove_file(path).unwrap();
        }
        let agent = f.temp.path().join("missing-agent.py");
        fs::write(&agent, r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    event = json.loads(line)
    if event['event'] == 'init':
        print('{}', flush=True)
    elif event['event'] == 'terminate':
        break
    else:
        print(json.dumps({'event': 'complete', 'oid': event['oid'], 'error': {'code': 404, 'message': 'missing object'}}), flush=True)
"#).unwrap();
        make_executable(&agent);
        f.git(
            &f.repo,
            &[
                "config",
                "lfs.customtransfer.missing.path",
                agent.to_str().unwrap(),
            ],
        );
        let key = match scope {
            "global" => "lfs.standalonetransferagent".into(),
            "url" => format!(
                "lfs.{}.standalonetransferagent",
                f.endpoint.as_ref().unwrap()
            ),
            _ => {
                let endpoint = f.endpoint.as_ref().unwrap();
                f.git(
                    &f.repo,
                    &["config", "lfs.url", &format!("{endpoint}repo.git/info/lfs")],
                );
                // Git LFS matches this scope; Git's --get-urlmatch does not.
                format!("lfs.{endpoint}repo.standalonetransferagent")
            }
        };
        f.git(&f.repo, &["config", &key, "missing"]);
        let before = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
        let out = f
            .command(&f.repo, BIN)
            .args(["drop", "--chunks", "asset.bin"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("standalone cache eviction requires interactive confirmation")
        );
        assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
        assert_eq!(objects_at(&f.repo.join(".git/cdc/lfs/objects")), before);
        #[cfg(target_os = "linux")]
        for (answer, accepted) in [("no\n", false), ("yes\n", true)] {
            let out = f.cdc_interactive(&["drop", "--chunks", "asset.bin"], answer);
            assert_eq!(
                out.status.success(),
                accepted,
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
            if accepted {
                assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
            } else {
                assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
                assert_eq!(objects_at(&f.repo.join(".git/cdc/lfs/objects")), before);
            }
        }
    }
}

#[test]
fn lifecycle_config_lfsconfig_selects_the_download_remote() {
    for (location, http) in [
        ("worktree", false),
        ("index", false),
        ("HEAD", false),
        ("git-override", false),
        ("worktree", true),
    ] {
        let f = if http {
            Fixture::http()
        } else {
            Fixture::file()
        };
        let data = b"only the LFS default remote has this content\n";
        f.publish_file("asset.bin", data, "published backing");
        f.git(&f.repo, &["remote", "rename", "origin", "backup"]);
        f.git(&f.repo, &["config", "--unset", "branch.main.remote"]);
        let empty = f.temp.path().join("empty.git");
        f.git(&f.repo, &["init", "--bare", empty.to_str().unwrap()]);
        f.git(
            &f.repo,
            &["remote", "add", "origin", empty.to_str().unwrap()],
        );
        f.git(
            &f.repo,
            &[
                "config",
                "--file",
                ".lfsconfig",
                "remote.lfsdefault",
                if location == "git-override" {
                    "origin"
                } else {
                    "backup"
                },
            ],
        );
        f.git(&f.repo, &["add", ".lfsconfig"]);
        f.commit("LFS default remote");
        if location == "git-override" {
            f.git(&f.repo, &["config", "remote.lfsdefault", "backup"]);
        } else if location != "worktree" {
            fs::remove_file(f.repo.join(".lfsconfig")).unwrap();
            if location == "HEAD" {
                f.git(&f.repo, &["rm", "--cached", ".lfsconfig"]);
            }
        }
        // Newer Git LFS rejects this .lfsconfig key. Give content only to
        // whichever remote the installed stock client actually selects.
        let native = f.git(&f.repo, &["lfs", "env"]);
        if native.lines().any(|line| line.starts_with("Endpoint=")) {
            f.git(
                &f.repo,
                &["remote", "set-url", "origin", f.remote.to_str().unwrap()],
            );
            f.git(
                &f.repo,
                &["remote", "set-url", "backup", empty.to_str().unwrap()],
            );
        }
        f.assert_cold_restoration("asset.bin", data);
        if http {
            f.cdc(&f.repo, &["drop", "--chunks", "asset.bin"]);
            assert!(objects_at(&f.repo.join(".git/cdc/lfs/objects")).is_empty());
            f.cdc(&f.repo, &["pull", "asset.bin"]);
            assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
        }
    }
}

#[cfg(unix)]
#[test]
fn lifecycle_config_downloads_without_a_git_remote() {
    let f = Fixture::file();
    let data = b"standalone downloads need only an LFS endpoint\n";
    f.publish_file("asset.bin", data, "published backing");
    f.standalone_download_agent(false);
    f.git(&f.repo, &["remote", "remove", "origin"]);
    assert!(f.git(&f.repo, &["remote"]).is_empty());
    f.assert_cold_restoration("asset.bin", data);

    let cache = objects_at(&f.repo.join(".git/cdc/lfs/objects"));
    let out = f
        .command(&f.repo, BIN)
        .args(["drop", "--chunks", "asset.bin"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("retention verification"));
    assert_eq!(fs::read(f.repo.join("asset.bin")).unwrap(), data);
    assert_eq!(objects_at(&f.repo.join(".git/cdc/lfs/objects")), cache);
}

#[cfg(unix)]
#[test]
fn lifecycle_config_relative_transfer_agent_paths_keep_the_source_directory() {
    let f = Fixture::file();
    let data = b"repository-local transfer executable\n";
    f.publish_file("asset.bin", data, "published backing");
    f.standalone_download_agent(true);
    f.assert_cold_restoration("asset.bin", data);
}

#[test]
fn lifecycle_config_expands_lfsconfig_includes_in_the_source_repository() {
    for mode in ["nested", "conditional", "git-override"] {
        let f = Fixture::file();
        let data = b"included LFS endpoint\n";
        f.publish_file("asset.bin", data, "published backing");
        let empty = f.temp.path().join("empty.git");
        f.git(&f.repo, &["init", "--bare", empty.to_str().unwrap()]);
        f.git(
            &f.repo,
            &["remote", "set-url", "origin", empty.to_str().unwrap()],
        );
        fs::create_dir_all(f.repo.join("settings/nested")).unwrap();
        fs::write(
            f.repo.join(".lfsconfig"),
            if mode == "conditional" {
                "[includeIf \"onbranch:main\"]\npath = settings/endpoint.inc\n"
            } else {
                "[include]\npath = settings/endpoint.inc\n"
            },
        )
        .unwrap();
        fs::write(
            f.repo.join("settings/endpoint.inc"),
            "[include]\npath = nested/endpoint.inc\n",
        )
        .unwrap();
        fs::write(
            f.repo.join("settings/nested/endpoint.inc"),
            if mode == "git-override" {
                "[lfs]\nurl = ../empty.git\n"
            } else {
                "[lfs]\nurl = ../remote.git\n"
            },
        )
        .unwrap();
        if mode == "git-override" {
            f.git(&f.repo, &["config", "lfs.url", "../remote.git"]);
        }
        f.assert_cold_restoration("asset.bin", data);
    }
}
