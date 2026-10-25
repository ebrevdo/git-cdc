//! LFS filter installation and preservation of existing Git hook behavior.

use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};

use crate::git::git_out;

const HOOK_MARKER: &str = "# git-cdc standard-lfs pre-push v1";
const FORWARD_MARKER: &str = "# git-cdc repository hook forwarding v1";
// githooks(5), including hooks used by git-p4 and git-send-email. pre-push
// has the storage gate; every other stub only delegates the existing hook.
const FORWARDED_HOOKS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "reference-transaction",
    "push-to-checkout",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "fsmonitor-watchman",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "post-index-change",
];

pub fn install(global: bool) -> Result<()> {
    git_out(&["lfs", "version"]).context("install Git LFS 3.x first")?;
    let scope = if global { "--global" } else { "--local" };
    let (hook, new_global_path) = if !global {
        (pre_push_path()?, None)
    } else if let Ok(path) = git_out(&["config", "--global", "--path", "--get", "core.hooksPath"]) {
        let path = PathBuf::from(path);
        ensure!(
            path.is_absolute(),
            "global core.hooksPath must be absolute for LFS installation"
        );
        (path.join("pre-push"), None)
    } else {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
            .context("HOME or XDG_CONFIG_HOME is required for global hooks")?;
        let path = config_home.join("git-cdc").join("hooks");
        ensure!(
            path.is_absolute(),
            "global hooks directory must be absolute"
        );
        (path.join("pre-push"), Some(path))
    };
    install_hook(&hook)?;
    let is_managed = new_global_path.is_some()
        || git_out(&["config", "--path", "--get", "cdc.lfs.hooksPath"])
            .is_ok_and(|path| hook.parent() == Some(Path::new(&path)));
    if is_managed {
        install_forwarders(hook.parent().unwrap())?;
    }
    for (key, value) in [
        ("filter.cdc-lfs.process", "git-cdc lfs-filter-process"),
        ("filter.cdc-lfs.clean", "git-cdc lfs-clean -- %f"),
        ("filter.cdc-lfs.smudge", "git-cdc lfs-smudge"),
        ("filter.cdc-lfs.required", "true"),
        ("diff.cdc-lfs.textconv", "git-cdc lfs-textconv --"),
        ("diff.cdc-lfs.binary", "true"),
        ("cdc.storage", "lfs"),
    ] {
        git_out(&["config", scope, key, value])?;
    }
    if let Some(path) = new_global_path {
        let path = path.to_str().context("non-UTF-8 global hooks directory")?;
        git_out(&["config", "--global", "core.hooksPath", path])?;
        // Only our managed global directory enables local-hook dispatch.
        git_out(&["config", "--global", "cdc.lfs.hooksPath", path])?;
    }
    eprintln!("git-cdc standard LFS filters and pre-push hook installed ({scope})");
    Ok(())
}

fn install_hook(path: &Path) -> Result<()> {
    let directory = path.parent().context("hook directory")?;
    fs::create_dir_all(directory)?;
    if path.exists() {
        ensure!(
            !path.is_symlink(),
            "refusing to replace symlink hook {}",
            path.display()
        );
        if !has_marker(path, HOOK_MARKER)? {
            let backup = path.with_file_name("pre-push.before-cdc-lfs");
            ensure!(
                !backup.exists(),
                "hook backup already exists: {}",
                backup.display()
            );
            fs::copy(path, &backup)?;
        }
    }
    // The handler resolves backups beside the active hook at invocation time.
    let script = format!("#!/bin/sh\n{HOOK_MARKER}\nexec git-cdc lfs-pre-push \"$@\"\n");
    prepare_hook(directory, &script)?.persist(path)?;
    Ok(())
}

fn prepare_hook(directory: &Path, script: &str) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(script.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    file.as_file().sync_all()?;
    Ok(file)
}

fn has_marker(path: &Path, marker: &str) -> Result<bool> {
    let mut prefix = Vec::new();
    File::open(path)?.take(256).read_to_end(&mut prefix)?;
    Ok(String::from_utf8_lossy(&prefix).contains(marker))
}

fn install_forwarders(directory: &Path) -> Result<()> {
    for name in FORWARDED_HOOKS {
        let path = directory.join(name);
        // Never replace a hook the user added to the active global directory.
        if path.symlink_metadata().is_ok() {
            continue;
        }
        let script =
            format!("#!/bin/sh\n{FORWARD_MARKER}\nexec git-cdc lfs-dispatch-hook {name} \"$@\"\n");
        let file = prepare_hook(directory, &script)?;
        // A concurrent installation or user-created hook wins this race.
        if let Err(error) = file.persist_noclobber(&path)
            && error.error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(error.into());
        }
    }
    Ok(())
}

/// Preserve the hook process boundary: stdin/stdout/stderr, cwd, environment,
/// arguments and the original exit status (including signals on Unix).
pub fn dispatch_hook(hook: &str, args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(FORWARDED_HOOKS.contains(&hook), "unsupported hook name");
    let original = hook_common_dir()?.join("hooks").join(hook);
    if !executable(&original)? || has_marker(&original, FORWARD_MARKER)? {
        return Ok(());
    }
    let mut command = Command::new(&original);
    command.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).with_context(|| format!("executing {}", original.display()))
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn hook_common_dir() -> Result<PathBuf> {
    if let Some(common) = std::env::var_os("GIT_COMMON_DIR") {
        return Ok(PathBuf::from(common));
    }
    if let Some(directory) = std::env::var_os("GIT_DIR") {
        let directory = PathBuf::from(directory);
        // During `git init`, reference-transaction can run before rev-parse
        // recognizes the incomplete repository. Git already supplies its path.
        return match fs::read_to_string(directory.join("commondir")) {
            Ok(common) => {
                let common = PathBuf::from(common.trim_end());
                Ok(if common.is_absolute() {
                    common
                } else {
                    directory.join(common)
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(directory),
            Err(error) => Err(error.into()),
        };
    }
    Ok(PathBuf::from(git_out(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ])?))
}

fn pre_push_path() -> Result<PathBuf> {
    Ok(PathBuf::from(git_out(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "hooks/pre-push",
    ])?))
}

pub(super) fn pre_push(remote: &str, url: &str, updates: &mut File) -> Result<()> {
    let installed = pre_push_path()?;
    if installed.is_file() && has_marker(&installed, HOOK_MARKER)? {
        run_pre_push_hook(&installed, remote, url, updates)?;
    }
    let managed = git_out(&["config", "--path", "--get", "cdc.lfs.hooksPath"]);
    let active = git_out(&["config", "--path", "--get", "core.hooksPath"]);
    let (Ok(managed), Ok(active)) = (managed, active) else {
        return Ok(());
    };
    if managed != active {
        return Ok(());
    }
    let local = hook_common_dir()?.join("hooks/pre-push");
    if local != installed {
        run_pre_push_hook(&local, remote, url, updates)?;
    }
    Ok(())
}

fn run_pre_push_hook(hook: &Path, remote: &str, url: &str, updates: &mut File) -> Result<()> {
    let mut hook = hook.to_path_buf();
    if hook.is_file() && has_marker(&hook, HOOK_MARKER)? {
        hook.set_file_name("pre-push.before-cdc-lfs");
    }
    // The CDC handler already invokes stock LFS with managed updates removed.
    // Only recognize complete stock scripts: custom vetoes must still run.
    if !executable(&hook)? || is_stock_lfs_hook(&hook)? {
        return Ok(());
    }
    updates.rewind()?;
    let status = Command::new(&hook)
        .args([remote, url])
        .stdin(Stdio::from(updates.try_clone()?))
        .status()?;
    ensure!(status.success(), "repository pre-push hook refused push");
    Ok(())
}

fn is_stock_lfs_hook(path: &Path) -> Result<bool> {
    let contents = fs::read(path)?;
    let contents = contents.trim_ascii_end();
    if contents == b"#!/bin/sh\ngit lfs pre-push \"$@\"" {
        return Ok(true);
    }
    // Current and older Git LFS templates differ only in their diagnostic.
    for message in [
        "This repository is configured for Git LFS but 'git-lfs' was not found on your path. If you no longer wish to use Git LFS, remove this hook by deleting the 'pre-push' file in the hooks directory (set by 'core.hookspath'; usually '.git/hooks').",
        "This repository is configured for Git LFS but 'git-lfs' was not found on your path. If you no longer wish to use Git LFS, remove this hook by deleting '.git/hooks/pre-push'.",
        "This repository is configured for Git LFS but 'git-lfs' was not found on your path. If you no longer wish to use Git LFS, remove this hook by deleting .git/hooks/pre-push.",
    ] {
        for diagnostic in [
            format!(r#"printf >&2 "\n%s\n\n" "{message}""#),
            format!(r#"echo >&2 "\n{message}\n""#),
        ] {
            let stock = format!(
                "#!/bin/sh\ncommand -v git-lfs >/dev/null 2>&1 || {{ {diagnostic}; exit 2; }}\ngit lfs pre-push \"$@\""
            );
            if contents == stock.as_bytes() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn executable(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(path.metadata()?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    Ok(true)
}
