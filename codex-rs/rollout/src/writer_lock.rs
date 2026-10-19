//! Shared cross-process ownership for local thread writers and rollout publication.
//! Thread lock-file creation and removal uses the existing home coordination lock.

use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_protocol::ThreadId;
use tracing::warn;

const WRITER_LOCK_DIR: &str = "thread-writer-locks";
const COORDINATION_LOCK_FILE: &str = ".coordination.lock";
const PHYSICAL_LOCK_DIR: &str = ".rollout-writer-locks";

/// Private path ownership, shared even when different profiles reference the same history.
/// Unlike an inode lock, this survives plain/compressed representation replacement.
/// Only participating writers are coordinated. Identity checks do not make a
/// later publication or unlink atomic against nonparticipating filesystem users.
pub(crate) struct PhysicalRolloutGuard {
    _lock: WriterLockGuard,
    path: PathBuf,
}

impl PhysicalRolloutGuard {
    pub(crate) fn acquire(path: &Path, create_parent: bool) -> io::Result<Self> {
        let plain = crate::compression::plain_rollout_path(path);
        let compressed = plain.with_extension("jsonl.zst");
        for candidate in [path, plain.as_path(), compressed.as_path()] {
            if fs::symlink_metadata(candidate)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                // Resolve symlinks even if try_exists would conceal a dangling link.
                fs::canonicalize(candidate)?;
            }
        }
        let existing = if path.try_exists()? {
            Some(path)
        } else if plain.try_exists()? {
            Some(plain.as_path())
        } else if compressed.try_exists()? {
            Some(compressed.as_path())
        } else {
            None
        };
        let path = if let Some(existing) = existing {
            crate::compression::plain_rollout_path(&fs::canonicalize(existing)?)
        } else {
            let parent = plain
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            if create_parent {
                fs::create_dir_all(parent)?;
            }
            fs::canonicalize(parent)?.join(
                plain
                    .file_name()
                    .ok_or_else(|| io::Error::other("rollout has no file name"))?,
            )
        };
        // Dangling file symlinks must not create a second lock namespace.
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unresolved rollout symlink",
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("rollout has no parent"))?;
        let coordinator = Arc::new(WriterLockCoordinator {
            directory: parent.join(PHYSICAL_LOCK_DIR),
            cleanup_attempted: AtomicBool::new(true),
        });
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("rollout has no file name"))?;
        let lock = coordinator.acquire_named(name)?;
        Ok(Self { _lock: lock, path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Coordinates writer ownership within one Codex home.
pub struct WriterLockCoordinator {
    directory: PathBuf,
    cleanup_attempted: AtomicBool,
}

/// Keeps a thread owned until all file work has finished.
pub struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    path: PathBuf,
    file: Option<File>,
}

impl WriterLockCoordinator {
    /// Uses the same lock namespace as existing local thread-store writers.
    pub fn new(codex_home: &Path) -> Self {
        Self {
            directory: codex_home.join(WRITER_LOCK_DIR),
            cleanup_attempted: AtomicBool::new(false),
        }
    }

    /// Acquires exclusive writer ownership, returning `WouldBlock` for an active writer.
    pub fn acquire(self: &Arc<Self>, thread_id: ThreadId) -> io::Result<WriterLockGuard> {
        self.acquire_named(OsStr::new(&thread_id.to_string()))
    }

    fn acquire_named(self: &Arc<Self>, name: &OsStr) -> io::Result<WriterLockGuard> {
        let _coordination_lock = self.lock_coordination()?;
        if !self.cleanup_attempted.swap(true, Ordering::Relaxed)
            && let Err(err) = self.remove_stale_thread_locks()
        {
            warn!("failed to clean up stale thread writer locks: {err}");
        }

        let mut file_name = name.to_os_string();
        file_name.push(".lock");
        let path = self.directory.join(file_name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to open thread writer lock {}: {err}",
                    path.display()
                ))
            })?;

        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "rollout already has an active writer",
                ));
            }
            Err(std::fs::TryLockError::Error(err)) => {
                return Err(io::Error::other(format!(
                    "failed to acquire thread writer lock {}: {err}",
                    path.display()
                )));
            }
        }

        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            path,
            file: Some(file),
        })
    }

    /// Holds coordination through publication after probing that the thread is idle.
    /// Every writer takes coordination before opening its thread lock, so the probe
    /// itself can be released. Encoding and verification must finish before this call.
    pub(crate) fn try_acquire_for_publication(
        &self,
        thread_id: ThreadId,
    ) -> io::Result<Option<File>> {
        let coordination_lock = self.lock_coordination()?;
        let path = self.directory.join(format!("{thread_id}.lock"));
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => match file.try_lock() {
                Ok(()) => Ok(Some(coordination_lock)),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(std::fs::TryLockError::Error(err)) => Err(err),
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Some(coordination_lock)),
            Err(err) => Err(err),
        }
    }

    fn lock_coordination(&self) -> io::Result<File> {
        fs::create_dir_all(&self.directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join(COORDINATION_LOCK_FILE))?;
        file.lock().map_err(|err| {
            io::Error::other(format!(
                "failed to acquire thread writer coordination lock: {err}"
            ))
        })?;
        Ok(file)
    }

    fn remove_stale_thread_locks(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(thread_id) = file_name.strip_suffix(".lock") else {
                continue;
            };
            if ThreadId::from_string(thread_id).is_err() {
                continue;
            }

            let path = entry.path();
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warn!(
                        "failed to inspect thread writer lock {}: {err}",
                        path.display()
                    );
                    continue;
                }
            };
            match file.try_lock() {
                Ok(()) => {
                    drop(file);
                    if let Err(err) = fs::remove_file(&path)
                        && err.kind() != io::ErrorKind::NotFound
                    {
                        warn!(
                            "failed to remove stale thread writer lock {}: {err}",
                            path.display()
                        );
                    }
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(err)) => {
                    warn!(
                        "failed to inspect thread writer lock {}: {err}",
                        path.display()
                    );
                }
            }
        }
        Ok(())
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        let coordination_lock = match self.coordinator.lock_coordination() {
            Ok(lock) => lock,
            Err(err) => {
                warn!("failed to coordinate thread writer lock cleanup: {err}");
                return;
            }
        };

        // Close the writer lock before deleting it so cleanup works on Windows too.
        drop(self.file.take());
        if let Err(err) = fs::remove_file(&self.path)
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!(
                "failed to remove thread writer lock {}: {err}",
                self.path.display()
            );
        }
        drop(coordination_lock);
    }
}

#[cfg(test)]
#[path = "writer_lock_tests.rs"]
mod tests;
