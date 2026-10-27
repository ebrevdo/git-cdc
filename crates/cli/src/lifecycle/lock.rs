//! Index lock ownership shared by normal cleanup and signal termination.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(unix)]
use std::thread;

use anyhow::{Context, Result, anyhow};

pub(super) struct IndexLock {
    path: PathBuf,
    owned: bool,
    cleanup: &'static SignalCleanup,
}

impl IndexLock {
    pub(super) fn acquire(path: PathBuf, temporary: &Path) -> Result<Self> {
        let cleanup = SignalCleanup::shared()?;
        let mut locks = cleanup.locks.lock().unwrap();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .context("locking the Git index for CDC worktree updates")?;
        locks.insert(path.clone(), temporary.to_owned());
        Ok(Self {
            path,
            owned: true,
            cleanup,
        })
    }

    pub(super) fn publish(
        &mut self,
        temporary: tempfile::NamedTempFile,
        index: &Path,
    ) -> Result<()> {
        let mut locks = self.cleanup.locks.lock().unwrap();
        temporary
            .persist(index)
            .context("publishing the Git index")?;
        fs::remove_file(&self.path)?;
        locks.remove(&self.path);
        self.owned = false;
        Ok(())
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if self.owned {
            let mut locks = self.cleanup.locks.lock().unwrap();
            let _ = fs::remove_file(&self.path);
            if let Some(temporary) = locks.remove(&self.path) {
                let _ = fs::remove_file(temporary);
            }
        }
        // A fast refresh can finish before the worker observes a signal.
        #[cfg(unix)]
        match self.cleanup.pending.load(Ordering::Relaxed) {
            0 => {}
            signal => self.cleanup.terminate(signal as i32),
        }
    }
}

#[derive(Clone)]
struct SignalCleanup {
    locks: Arc<Mutex<BTreeMap<PathBuf, PathBuf>>>,
    #[cfg(unix)]
    pending: Arc<AtomicUsize>,
}

impl SignalCleanup {
    fn shared() -> Result<&'static Self> {
        // Unregistering signal-hook actions does not restore default handlers.
        // Keep one worker alive after the last lock is released instead.
        static CLEANUP: OnceLock<Result<SignalCleanup>> = OnceLock::new();
        CLEANUP
            .get_or_init(Self::new)
            .as_ref()
            .map_err(|error| anyhow!("initializing index signal cleanup: {error:#}"))
    }

    fn new() -> Result<Self> {
        let cleanup = Self {
            locks: Arc::default(),
            #[cfg(unix)]
            pending: Arc::new(AtomicUsize::new(0)),
        };
        #[cfg(unix)]
        cleanup.listen()?;
        Ok(cleanup)
    }

    #[cfg(unix)]
    fn listen(&self) -> Result<()> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        use signal_hook::iterator::Signals;

        let handled_signals = [SIGHUP, SIGINT, SIGTERM];
        // If initialization fails, termination must still work without a worker.
        let default = Arc::new(AtomicBool::new(true));
        for signal in handled_signals {
            signal_hook::flag::register_conditional_default(signal, Arc::clone(&default))?;
        }
        let mut signals = Signals::new(handled_signals)?;
        for signal in handled_signals {
            signal_hook::flag::register_usize(signal, Arc::clone(&self.pending), signal as usize)?;
        }
        let cleanup = self.clone();
        thread::Builder::new()
            .name("cdc-index-signals".into())
            .spawn(move || {
                if let Some(signal) = signals.forever().next() {
                    cleanup.terminate(signal);
                }
            })?;
        default.store(false, Ordering::SeqCst);
        Ok(())
    }

    #[cfg(unix)]
    fn terminate(&self, signal: i32) -> ! {
        // Hold ownership through termination: creation and publication must
        // not race past signal cleanup.
        let locks = self.locks.lock().unwrap();
        for (path, temporary) in locks.iter() {
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(temporary);
        }
        let _ = signal_hook::low_level::emulate_default_handler(signal);
        std::process::exit(128 + signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_guard_leaves_a_reacquired_lock_owned() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("index");
        let path = directory.path().join("index.lock");
        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let mut first = IndexLock::acquire(path.clone(), temporary.path()).unwrap();
        first.publish(temporary, &index).unwrap();

        let temporary = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let second = IndexLock::acquire(path.clone(), temporary.path()).unwrap();
        drop(first);
        assert!(path.exists());
        drop(second);
        assert!(!path.exists());
        assert!(!temporary.path().exists());
    }
}
