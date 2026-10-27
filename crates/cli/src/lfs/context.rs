//! A private Git repository carries one frozen download configuration. Git LFS
//! still resolves endpoints, credentials and transfer adapters itself.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, ensure};

use super::{Store, git_out, repo_root, retention};

pub(super) enum VerificationRisk {
    FileEndpoint,
    StandaloneAgent,
}

impl VerificationRisk {
    pub fn label(&self) -> &'static str {
        match self {
            Self::FileEndpoint => "file://",
            Self::StandaloneAgent => "standalone",
        }
    }
}

impl std::fmt::Display for VerificationRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileEndpoint => f.write_str("file:// endpoints"),
            Self::StandaloneAgent => f.write_str("custom standalone transfer agents"),
        }
    }
}

pub(super) struct DownloadContext {
    directory: tempfile::TempDir,
    remote: String,
    destination: Option<String>,
    pub verification_risk: Option<VerificationRisk>,
}

/// Drop repository selectors inherited from hooks and alternate indexes. Keep
/// credential/SSH/proxy environment variables available to the stock client.
fn private_command(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(directory);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
        "GIT_REPLACE_REF_BASE",
    ] {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    command
}

fn successful(output: Output, operation: &str) -> Result<Vec<u8>> {
    ensure!(output.status.success(), "{operation} failed");
    Ok(output.stdout)
}

fn relevant(key: &str) -> bool {
    [
        "lfs.",
        "remote.",
        "branch.",
        "http.",
        "credential.",
        "ssh.",
        "protocol.",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
        || matches!(key, "core.askpass" | "core.sshcommand")
}

fn absolute_url(root: &Path, url: &str) -> Result<String> {
    if !url.is_empty()
        && !url.contains("://")
        && !url.contains(':')
        && !Path::new(url).is_absolute()
    {
        Ok(root
            .join(url)
            .to_str()
            .context("non-UTF-8 remote path")?
            .to_owned())
    } else {
        Ok(url.to_owned())
    }
}

fn is_lfs_download_url(key: &str) -> bool {
    key == "lfs.url" || (key.starts_with("remote.") && key.ends_with(".lfsurl"))
}

type Config = Vec<(String, String)>;

fn read_config(args: &[&str]) -> Result<Config> {
    let records = successful(
        Command::new("git")
            .args(["config", "--null", "--includes"])
            .args(args)
            .output()?,
        "reading LFS configuration",
    )?;
    records
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let record = std::str::from_utf8(record)?;
            let (key, value) = record.split_once('\n').unwrap_or((record, "true"));
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn config_value<'a>(config: &'a Config, key: &str) -> Option<&'a str> {
    config
        .iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Expand includes where Git LFS reads the source configuration, before any
/// files or commands move into the private repository.
fn source_lfs_config(root: &Path) -> Result<Config> {
    let path = root.join(".lfsconfig");
    if path.is_file() {
        return read_config(&[
            "--file",
            path.to_str().context("non-UTF-8 .lfsconfig path")?,
            "--list",
        ]);
    }
    for revision in [":.lfsconfig", "HEAD:.lfsconfig"] {
        if let Ok(config) = read_config(&["--blob", revision, "--list"]) {
            return Ok(config);
        }
    }
    Ok(Vec::new())
}

fn default_remote(config: &Config, lfs_config: &Config) -> Result<Option<String>> {
    if let Some(remote) = config_value(config, "remote.lfsdefault") {
        return Ok(Some(remote.to_owned()));
    }
    let Some(remote) = config_value(lfs_config, "remote.lfsdefault") else {
        return Ok(None);
    };
    // Git LFS versions differ on whether this key is safe in .lfsconfig.
    // Ask the installed client; diagnostic key names are not localized.
    let report = Command::new("git").args(["lfs", "env"]).output()?;
    ensure!(
        report.status.success(),
        "reading effective LFS configuration failed"
    );
    let rejected = String::from_utf8_lossy(&report.stderr)
        .lines()
        .any(|line| line.trim() == "remote.lfsdefault");
    Ok((!rejected).then(|| remote.to_owned()))
}

fn download_remote(config: &Config, lfs_config: &Config) -> Result<String> {
    let branch = git_out(&["symbolic-ref", "--quiet", "HEAD"])
        .ok()
        .and_then(|reference| reference.strip_prefix("refs/heads/").map(str::to_owned));
    if let Some(branch) = branch
        && let Some(remote) = config_value(config, &format!("branch.{branch}.remote"))
    {
        return Ok(remote.to_owned());
    }
    if let Some(remote) = default_remote(config, lfs_config)? {
        return Ok(remote);
    }
    let remotes = git_out(&["remote"]).unwrap_or_default();
    let names: Vec<_> = remotes.lines().collect();
    Ok(if names.len() == 1 {
        names[0].to_owned()
    } else {
        "origin".into()
    })
}

/// Resolve aliases once using the source configuration, then anchor local
/// paths before moving into the private repository. Preserve SSH URLs intact.
fn resolve_lfs_url(root: &Path, value: &str) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    let url = git_out(&["ls-remote", "--get-url", "--", value])?;
    absolute_url(root, &url)
}

fn freeze_value(root: &Path, key: &str, value: &str) -> Result<String> {
    if is_lfs_download_url(key) {
        return resolve_lfs_url(root, value);
    }
    let path = Path::new(value);
    if key.starts_with("lfs.customtransfer.")
        && key.ends_with(".path")
        && path.is_relative()
        && path.components().count() > 1
    {
        return Ok(root
            .join(path)
            .to_str()
            .context("non-UTF-8 transfer-agent path")?
            .to_owned());
    }
    Ok(value.to_owned())
}

impl DownloadContext {
    pub fn new() -> Result<Self> {
        let root = repo_root()?;
        let config = read_config(&["--list"])?;
        let lfs_config = source_lfs_config(&root)?;
        let remote = download_remote(&config, &lfs_config)?;
        // Git LFS uses the last configured fetch URL. Ask Git to expand its
        // URL rewrites once; the private configuration omits rewrite rules.
        // An explicit LFS endpoint can download without a Git destination.
        let destination = if remote == "." {
            None
        } else {
            git_out(&["remote", "get-url", "--all", &remote])
                .ok()
                .and_then(|urls| urls.lines().last().map(str::to_owned))
                .filter(|url| !url.is_empty())
                .map(|url| absolute_url(&root, &url))
                .transpose()?
        };
        let object_format = git_out(&["rev-parse", "--show-object-format"])?;
        let directory = tempfile::tempdir()?;
        let hooks = directory.path().join("empty-hooks");
        fs::create_dir(&hooks)?;
        successful(
            private_command(directory.path())
                .arg("-c")
                .arg(format!("core.hooksPath={}", hooks.display()))
                .args([
                    "init",
                    "--quiet",
                    "--initial-branch=cdc-cache-probe",
                    "--template=",
                ])
                .arg(format!("--object-format={object_format}"))
                .output()?,
            "initializing LFS verification repository",
        )?;
        let mut context = Self {
            directory,
            remote,
            destination,
            verification_risk: None,
        };
        // Be conservative about URL-scoped agents. Git LFS also matches
        // scopes with an omitted `.git` suffix, unlike Git's URL matcher.
        let standalone_agent = config.iter().any(|(key, value)| {
            key.starts_with("lfs.")
                && key.ends_with(".standalonetransferagent")
                && !value.trim().is_empty()
        });
        for (key, value) in &config {
            if relevant(key) && key != "lfs.storage" {
                let value = freeze_value(&root, key, value)?;
                context.configure(key, &value, true)?;
            }
        }
        context.freeze_lfs_config(&root, &lfs_config)?;
        if let Some(destination) = &context.destination {
            context.configure(
                &format!("remote.{}.url", context.remote),
                destination,
                false,
            )?;
        } else if context.remote != "." {
            // `lfs fetch` requires a named Git remote even when an explicit
            // LFS URL supplies the endpoint. This stays private and is never
            // treated as a destination for retention or eviction verification.
            let repository = context
                .path()
                .to_str()
                .context("non-UTF-8 private repository")?;
            context.configure(&format!("remote.{}.url", context.remote), repository, false)?;
        }
        for (key, value) in [
            ("branch.cdc-cache-probe.remote", context.remote.as_str()),
            ("remote.lfsdefault", context.remote.as_str()),
            ("lfs.pruneremotetocheck", context.remote.as_str()),
            ("lfs.storage", "cdc/lfs"),
            ("lfs.remote.autodetect", "false"),
            ("lfs.remote.searchall", "false"),
            ("lfs.skipdownloaderrors", "false"),
            ("lfs.fetchinclude", ""),
            ("lfs.fetchexclude", ""),
        ] {
            context.configure(key, value, false)?;
        }
        successful(
            context
                .command()
                .args([
                    "-c",
                    "user.name=git-cdc",
                    "-c",
                    "user.email=git-cdc@localhost",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "LFS cache verification",
                ])
                .output()?,
            "preparing LFS verification history",
        )?;
        context.verification_risk = context.detect_verification_risk(standalone_agent)?;
        Ok(context)
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn store(&self) -> Store {
        Store {
            root: self.path().join(".git/cdc/lfs"),
        }
    }

    fn detect_verification_risk(&self, standalone_agent: bool) -> Result<Option<VerificationRisk>> {
        // These are nonlocalized key/value fields emitted by `git lfs env`,
        // used only to identify verification risks, not object availability.
        let report = String::from_utf8(successful(
            self.command().args(["lfs", "env"]).output()?,
            "resolving LFS endpoint",
        )?)?;
        let prefix = if self.remote == "origin" {
            "Endpoint=".to_owned()
        } else {
            format!("Endpoint ({})=", self.remote)
        };
        let endpoint = report
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .and_then(|line| line.rsplit_once(" (auth=").map(|(url, _)| url));
        ensure!(
            endpoint.is_some() || self.destination.is_none(),
            "Git LFS did not report the selected download endpoint"
        );
        Ok(if endpoint.is_some_and(|url| url.starts_with("file://")) {
            Some(VerificationRisk::FileEndpoint)
        } else if standalone_agent {
            Some(VerificationRisk::StandaloneAgent)
        } else {
            None
        })
    }

    pub fn destination(&self) -> Result<&str> {
        ensure!(
            self.remote != ".",
            "local repository cannot be its own LFS eviction backing"
        );
        self.destination.as_deref().context(
            "LFS retention verification requires a Git destination for the download remote",
        )
    }

    pub fn share_source_objects(&self) -> Result<()> {
        // Git's retention checks can reuse the clone's metadata, including a
        // linked worktree's shared store. Keep download recipes self-contained:
        // stock LFS versions differ in their handling of quoted alternates.
        let objects = git_out(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])?;
        let quoted = objects
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        fs::write(
            self.path().join(".git/objects/info/alternates"),
            format!("\"{quoted}\"\n"),
        )?;
        Ok(())
    }

    pub fn command(&self) -> Command {
        let mut command = private_command(self.path());
        command
            // The file adapter opens the remote repository separately. This
            // relative override must reach that child too, as in Store::command.
            .args([
                "-c",
                "core.hooksPath=empty-hooks",
                "-c",
                "lfs.storage=cdc/lfs",
            ])
            .env_remove("GIT_LFS_SKIP_SMUDGE")
            .env_remove("GIT_LFS_SKIP_DOWNLOAD_ERRORS");
        command
    }

    fn configure(&self, key: &str, value: &str, append: bool) -> Result<()> {
        successful(
            self.command()
                .args([
                    "config",
                    if append { "--add" } else { "--replace-all" },
                    key,
                    value,
                ])
                .output()?,
            "freezing LFS configuration",
        )?;
        Ok(())
    }

    fn freeze_lfs_config(&self, root: &Path, config: &Config) -> Result<()> {
        let path = self.path().join(".lfsconfig");
        for (key, value) in config {
            // Includes were expanded in the source repository. Replaying them
            // here would resolve relative paths and conditions a second time.
            if key.starts_with("include.") || key.starts_with("includeif.") {
                continue;
            }
            let value = freeze_value(root, key, value)?;
            successful(
                self.command()
                    .args(["config", "--file"])
                    .arg(&path)
                    .args(["--add", key, &value])
                    .output()?,
                "freezing .lfsconfig",
            )?;
        }
        Ok(())
    }

    pub fn download_batch(
        &self,
        objects: &[(String, u64)],
        mut store: impl FnMut(&str, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        let pointers: BTreeMap<_, _> = objects.iter().cloned().collect();
        // The temporary commit has no parents or published refs. HEAD stays
        // empty so these download recipes cannot affect eviction verification.
        let commit = retention::stage_with(&pointers, &[], self.command())?;
        let status = self
            .command()
            .args(["lfs", "fetch", "--all", &self.remote, &commit])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()?;
        ensure!(status.success(), "LFS batch download failed");
        let downloaded = self.store();
        for (oid, size) in objects {
            let bytes = downloaded.read_cached_leased(oid, *size)?;
            store(oid, bytes)?;
            fs::remove_file(downloaded.path(oid))?;
        }
        Ok(())
    }

    /// The private HEAD has no LFS pointers, so every placeholder is prunable.
    /// Verification runs on this disposable cache, never the user's cache.
    pub fn verify_remote(&self, objects: &[(String, u64)]) -> Result<HashSet<String>> {
        // A failure conservatively keeps this entire group. Bound the group
        // while amortizing stock Git LFS startup and verification requests.
        const BATCH_OBJECTS: usize = 1024;
        let mut verified = HashSet::new();
        let store = self.store();
        let objects_path = store.root.join("objects");
        for batch in objects.chunks(BATCH_OBJECTS) {
            match fs::remove_dir_all(&objects_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            for (oid, size) in batch {
                let path = store.path(oid);
                fs::create_dir_all(path.parent().unwrap())?;
                File::create(path)?.set_len(*size)?;
            }
            let output = self
                .command()
                .args([
                    "lfs",
                    "prune",
                    "--dry-run",
                    "--force",
                    "--verify-remote",
                    "--verify-unreachable",
                    "--when-unverified=halt",
                ])
                .output()?;
            if output.status.success() {
                verified.extend(batch.iter().map(|(oid, _)| oid.clone()));
            } else {
                eprintln!("git-cdc: LFS remote verification failed; candidate chunks kept locally");
            }
            fs::remove_dir_all(&objects_path)?;
        }
        Ok(verified)
    }
}
