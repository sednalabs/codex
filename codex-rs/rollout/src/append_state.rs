//! In-process recovery for one owned append, not a crash-recovery journal.

use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;

use crate::file_identity::FileIdentity;

/// The small synchronous boundary also permits real partial-progress I/O tests.
pub(crate) trait AppendIo: Read + Seek + Write {
    fn file(&self) -> &File;
}

impl AppendIo for File {
    fn file(&self) -> &File {
        self
    }
}

pub(crate) struct PendingAppend {
    bytes: Vec<u8>,
    checkpoint: Option<Checkpoint>,
    conflict: Option<String>,
}

struct Checkpoint {
    identity: FileIdentity,
    start: u64,
    witness: Vec<u8>,
}

impl PendingAppend {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            checkpoint: None,
            conflict: None,
        }
    }

    pub(crate) fn conflict(&mut self, message: &str) -> io::Error {
        let message = self.conflict.get_or_insert_with(|| message.to_owned());
        io::Error::new(io::ErrorKind::InvalidData, message.clone())
    }

    pub(crate) fn check_conflict(&self) -> io::Result<()> {
        match &self.conflict {
            Some(message) => Err(io::Error::new(io::ErrorKind::InvalidData, message.clone())),
            None => Ok(()),
        }
    }

    /// Called only with the physical path fence held. The caller joins this entire
    /// operation before it closes/reopens the file: no Tokio file work can outlive it.
    pub(crate) fn finish(
        &mut self,
        file: &mut (impl AppendIo + ?Sized),
        path: &Path,
    ) -> io::Result<()> {
        self.check_conflict()?;
        if self.checkpoint.is_none() {
            let start = file.file().metadata()?.len();
            let mut witness = vec![0; start.min(1024) as usize];
            file.seek(SeekFrom::Start(start - witness.len() as u64))?;
            file.read_exact(&mut witness)?;
            self.checkpoint = Some(Checkpoint {
                identity: FileIdentity::from_file(file.file().try_clone()?)?,
                start,
                witness,
            });
        }
        let written = self.verified_prefix(file, path)?;
        file.write_all(&self.bytes[written..])?;
        file.flush()?;
        if self.verified_prefix(file, path)? != self.bytes.len() {
            return Err(self.conflict("rollout append did not persist its complete line"));
        }
        Ok(())
    }

    fn verified_prefix(
        &mut self,
        file: &mut (impl AppendIo + ?Sized),
        path: &Path,
    ) -> io::Result<usize> {
        let checkpoint = self
            .checkpoint
            .as_ref()
            .expect("append checkpoint established");
        let identity = FileIdentity::from_file(file.file().try_clone()?)?;
        let path_identity = match FileIdentity::from_path(path) {
            Ok(identity) => identity,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(self.conflict("rollout path disappeared during an owned append"));
            }
            Err(err) => return Err(err),
        };
        if identity != checkpoint.identity || path_identity != checkpoint.identity {
            return Err(self.conflict("rollout file was replaced during an owned append"));
        }
        let len = file.file().metadata()?.len();
        let Some(written) = len
            .checked_sub(checkpoint.start)
            .and_then(|len| usize::try_from(len).ok())
            .filter(|len| *len <= self.bytes.len())
        else {
            return Err(self.conflict("rollout length conflicts with its owned append"));
        };
        let mut witness = vec![0; checkpoint.witness.len()];
        file.seek(SeekFrom::Start(checkpoint.start - witness.len() as u64))?;
        if let Err(err) = file.read_exact(&mut witness) {
            return Err(if err.kind() == io::ErrorKind::UnexpectedEof {
                self.conflict("rollout shrank while verifying its owned append")
            } else {
                err
            });
        }
        if witness != checkpoint.witness {
            return Err(self.conflict("rollout prefix changed during an owned append"));
        }
        let mut tail = vec![0; written];
        if let Err(err) = file.read_exact(&mut tail) {
            return Err(if err.kind() == io::ErrorKind::UnexpectedEof {
                self.conflict("rollout shrank while verifying its owned append")
            } else {
                err
            });
        }
        if tail != self.bytes[..written] {
            return Err(self.conflict("rollout tail conflicts with its owned append"));
        }
        Ok(written)
    }
}

#[cfg(test)]
#[path = "append_state_tests.rs"]
pub(crate) mod tests;
