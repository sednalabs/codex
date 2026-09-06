use codex_protocol::ThreadId;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

const LOCK_SUFFIX: &str = ".goal-execution.lock";

/// An owned, non-blocking execution lease for one thread goal.
///
/// The file handle is retained behind an `Arc` so clones keep the native OS
/// lock alive until the final clone is dropped. The lock file is intentionally
/// never removed: its path is the stable identity for this thread and goals
/// database.
#[derive(Clone)]
pub struct GoalExecutionLease {
    _file: Arc<File>,
}

impl fmt::Debug for GoalExecutionLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GoalExecutionLease")
    }
}

/// Failure to acquire an explicit goal execution lease.
#[derive(Debug)]
pub enum GoalExecutionLeaseError {
    /// Another process or runtime currently owns this thread's lease.
    Busy,
    /// Native advisory file locking is unavailable on this target.
    Unsupported,
    /// The lock path could not be opened or acquired.
    Io(std::io::Error),
}

impl fmt::Display for GoalExecutionLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("goal execution lease is busy"),
            Self::Unsupported => formatter.write_str("goal execution leases are unsupported"),
            Self::Io(error) => write!(formatter, "goal execution lease I/O error: {error}"),
        }
    }
}

impl std::error::Error for GoalExecutionLeaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Busy | Self::Unsupported => None,
        }
    }
}

impl GoalExecutionLease {
    pub(crate) fn acquire(
        goals_db_path: &Path,
        thread_id: ThreadId,
    ) -> Result<Self, GoalExecutionLeaseError> {
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (goals_db_path, thread_id);
            return Err(GoalExecutionLeaseError::Unsupported);
        }

        #[cfg(any(unix, windows))]
        {
            let lock_path = lock_path(goals_db_path, thread_id);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(lock_path)
                .map_err(GoalExecutionLeaseError::Io)?;
            match file.try_lock() {
                Ok(()) => Ok(Self {
                    _file: Arc::new(file),
                }),
                Err(std::fs::TryLockError::WouldBlock) => Err(GoalExecutionLeaseError::Busy),
                Err(std::fs::TryLockError::Error(error)) => {
                    Err(GoalExecutionLeaseError::Io(error))
                }
            }
        }
    }
}

fn lock_path(goals_db_path: &Path, thread_id: ThreadId) -> PathBuf {
    let file_name = goals_db_path
        .file_name()
        .expect("goals database path must have a file name")
        .to_string_lossy();
    goals_db_path.with_file_name(format!(".{file_name}.{thread_id}{LOCK_SUFFIX}"))
}

#[cfg(test)]
#[path = "goal_execution_lease_tests.rs"]
mod tests;
