//! Handle-bound identity. Unsupported identity queries fail closed, never by metadata fallback.

#[cfg(not(windows))]
pub(crate) use same_file::Handle as FileIdentity;

#[cfg(windows)]
use std::fs::File;
#[cfg(windows)]
use std::io;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::path::Path;

#[cfg(windows)]
#[derive(Debug, Eq, PartialEq)]
struct WindowsFileId {
    volume: u64,
    id: [u8; 16],
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct FileIdentity {
    // Retain the handle, so an unlinked file's identifier cannot be recycled
    // while it is still used as an append/replacement checkpoint.
    _file: File,
    key: WindowsFileId,
}

#[cfg(windows)]
impl FileIdentity {
    pub(crate) fn from_file(file: File) -> io::Result<Self> {
        use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
        use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
        use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

        let mut info = FILE_ID_INFO::default();
        // SAFETY: `file` owns a valid handle and stays alive throughout the call.
        // `info` is an initialized, aligned FILE_ID_INFO with exclusive writable
        // access; FileIdInfo selects that exact layout and its full buffer size.
        // The API does not retain either pointer. On failure no output is used.
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if result == 0 {
            // In particular, do not fall back to a 64-bit file index on ReFS or
            // when a driver does not implement this information class.
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _file: file,
            key: WindowsFileId {
                volume: info.VolumeSerialNumber,
                id: info.FileId.Identifier,
            },
        })
    }

    pub(crate) fn from_path(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_file(File::open(path)?)
    }
}

#[cfg(windows)]
impl PartialEq for FileIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

#[cfg(windows)]
impl Eq for FileIdentity {}

#[cfg(test)]
#[path = "file_identity_tests.rs"]
mod tests;
