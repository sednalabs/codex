use std::io;
use std::path::Path;

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
fn is_disk_file(file: &tokio::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
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
