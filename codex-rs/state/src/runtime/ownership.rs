//! Exclusive initialization ownership for the goals database.
//!
//! The runtime remains a single, globally closable resource bundle. This
//! module only arbitrates initialization: one live runtime owns the canonical
//! goals database identity in a process, and the adjacent OS lock extends the
//! same ownership decision across processes. A competing initialization gets a
//! typed busy error instead of a second runtime.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;

const RUNTIME_LOCK_EXTENSION: &str = "runtime.lock";

type Registry = HashMap<PathBuf, Weak<RuntimeProcessLock>>;

static REGISTRY: OnceLock<tokio::sync::Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static tokio::sync::Mutex<Registry> {
    REGISTRY.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// The failure returned when a goals database is already owned.
#[derive(Debug)]
pub enum RuntimeOwnershipError {
    /// The canonical goals database identity is already owned in this
    /// process, or its adjacent lock is held by another process.
    Busy {
        goals_path: PathBuf,
        lock_path: PathBuf,
    },
    /// The lock could not be inspected or acquired for an I/O reason.
    Io {
        goals_path: PathBuf,
        lock_path: PathBuf,
        source: io::Error,
    },
    /// The goals path could not be reduced to a stable identity.
    Canonicalize {
        goals_path: PathBuf,
        source: io::Error,
    },
}

impl RuntimeOwnershipError {
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Busy { .. })
    }

    pub fn goals_path(&self) -> &Path {
        match self {
            Self::Busy { goals_path, .. }
            | Self::Io { goals_path, .. }
            | Self::Canonicalize { goals_path, .. } => goals_path,
        }
    }

    pub fn lock_path(&self) -> Option<&Path> {
        match self {
            Self::Busy { lock_path, .. } | Self::Io { lock_path, .. } => Some(lock_path),
            Self::Canonicalize { .. } => None,
        }
    }
}

impl fmt::Display for RuntimeOwnershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy {
                goals_path,
                lock_path,
            } => write!(
                formatter,
                "goals database is already owned ({}; lock {})",
                goals_path.display(),
                lock_path.display()
            ),
            Self::Io {
                goals_path,
                lock_path,
                source,
            } => write!(
                formatter,
                "failed to acquire goals database ownership for {} (lock {}): {source}",
                goals_path.display(),
                lock_path.display()
            ),
            Self::Canonicalize { goals_path, source } => write!(
                formatter,
                "failed to canonicalize goals database path {}: {source}",
                goals_path.display()
            ),
        }
    }
}

impl Error for RuntimeOwnershipError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Busy { .. } => None,
            Self::Io { source, .. } | Self::Canonicalize { source, .. } => Some(source),
        }
    }
}

/// Ownership acquired for one complete `StateRuntime` initialization.
pub(crate) struct RuntimeOwnerAdmission {
    process_lock: Arc<RuntimeProcessLock>,
}

impl RuntimeOwnerAdmission {
    pub(crate) fn owner_capability(&self) -> OwnerCapability {
        OwnerCapability {
            _process_lock: Arc::clone(&self.process_lock),
        }
    }
}

/// Private authority accepted by direct goals-store mutation paths.
///
/// The capability carries the process lock with direct goals-store authority,
/// so that authority cannot outlive the ownership decision silently. No public
/// constructor is provided.
#[derive(Clone)]
pub(crate) struct OwnerCapability {
    _process_lock: Arc<RuntimeProcessLock>,
}

/// Admit a runtime only when it owns the canonical goals database identity.
///
/// The in-process registry closes the gap between separate file descriptors in
/// one process and makes same-process contention deterministic. The registry
/// stores only a weak lock witness, so dead entries are removed on the next
/// admission. The OS lock remains the cross-process boundary.
pub(crate) async fn admit(
    goals_path: &Path,
) -> Result<RuntimeOwnerAdmission, RuntimeOwnershipError> {
    let identity = canonical_goals_db_identity(goals_path).map_err(|source| {
        RuntimeOwnershipError::Canonicalize {
            goals_path: goals_path.to_path_buf(),
            source,
        }
    })?;
    let lock_path = runtime_lock_path(&identity);

    let mut registry_guard = registry().lock().await;
    registry_guard.retain(|_, process_lock| process_lock.upgrade().is_some());
    if registry_guard
        .get(&identity)
        .and_then(Weak::upgrade)
        .is_some()
    {
        return Err(RuntimeOwnershipError::Busy {
            goals_path: identity,
            lock_path,
        });
    }

    let process_lock = RuntimeProcessLock::try_acquire(&lock_path).map_err(|source| {
        RuntimeOwnershipError::Io {
            goals_path: identity.clone(),
            lock_path: lock_path.clone(),
            source,
        }
    })?;
    let Some(process_lock) = process_lock else {
        return Err(RuntimeOwnershipError::Busy {
            goals_path: identity,
            lock_path,
        });
    };
    let process_lock = Arc::new(process_lock);
    registry_guard.insert(identity.clone(), Arc::downgrade(&process_lock));
    drop(registry_guard);

    Ok(RuntimeOwnerAdmission { process_lock })
}

pub(crate) fn runtime_lock_path(goals_path: &Path) -> PathBuf {
    goals_path.with_extension(RUNTIME_LOCK_EXTENSION)
}

/// Return a path identity that converges for existing symlink aliases and for
/// aliases through symlinked parent directories. The database itself may not
/// exist on first startup, so canonicalize its parent in that case.
pub(crate) fn canonical_goals_db_identity(path: &Path) -> io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let canonical_parent = std::fs::canonicalize(parent)?;
            let file_name = path.file_name().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("goals database path has no filename: {}", path.display()),
                )
            })?;
            Ok(canonical_parent.join(file_name))
        }
        Err(error) => Err(error),
    }
}

/// A process-lifetime advisory lock. Dropping the final lock witness releases
/// the OS lock.
pub(crate) struct RuntimeProcessLock {
    _file: File,
}

impl RuntimeProcessLock {
    pub(crate) fn try_acquire(lock_path: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}
