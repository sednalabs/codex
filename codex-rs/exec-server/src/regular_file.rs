use std::io;
use std::path::Path;
use tokio::io::AsyncReadExt;

pub(crate) async fn open(path: &Path) -> io::Result<tokio::fs::File> {
    #[cfg(windows)]
    if has_named_pipe_prefix(path) {
        return Err(not_file_error(path));
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    configure_open(&mut options);

    let file = options.open(path).await?;
    if !is_disk_file(&file) || !file.metadata().await?.is_file() {
        return Err(not_file_error(path));
    }
    Ok(file)
}

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
fn not_file_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("path `{}` is not a file", path.display()),
    )
}

#[cfg(windows)]
fn has_named_pipe_prefix(path: &Path) -> bool {
    use std::path::Component;
    use std::path::Prefix;

    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(
                prefix.kind(),
                Prefix::DeviceNS(device) if device.to_string_lossy().eq_ignore_ascii_case("pipe")
            )
    )
=======
/// Reads a regular UTF-8 file without following a symlink at its final path component.
pub async fn read_sensitive_file_to_string(path: &Path) -> io::Result<String> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    configure_open(&mut options);

    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);

    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let mut file = options.open(path).await?;
    let metadata = file.metadata().await?;
    if !is_disk_file(&file) || !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path `{}` is not a regular file", path.display()),
        ));
    }

    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    Ok(contents)
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
}

#[cfg(unix)]
fn configure_open(options: &mut tokio::fs::OpenOptions) {
    options.custom_flags(libc::O_NONBLOCK);
}

#[cfg(windows)]
fn configure_open(options: &mut tokio::fs::OpenOptions) {
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

    options.security_qos_flags(SECURITY_IDENTIFICATION);
}

#[cfg(not(any(unix, windows)))]
fn configure_open(_options: &mut tokio::fs::OpenOptions) {}

#[cfg(windows)]
pub(crate) fn is_disk_file(file: &impl std::os::windows::io::AsRawHandle) -> bool {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_TYPE_DISK;
    use windows_sys::Win32::Storage::FileSystem::GetFileType;

    // SAFETY: `file` owns this handle for the duration of the call.
    unsafe { GetFileType(file.as_raw_handle() as HANDLE) == FILE_TYPE_DISK }
}

#[cfg(not(windows))]
fn is_disk_file(_file: &tokio::fs::File) -> bool {
    true
}

#[cfg(test)]
#[path = "regular_file_tests.rs"]
mod tests;
