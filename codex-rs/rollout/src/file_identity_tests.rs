use super::*;
use std::fs;
use std::fs::File;
use std::io;
use tempfile::TempDir;

#[test]
fn file_identity_matches_cloned_and_reopened_handles() -> io::Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("file");
    fs::write(&path, b"original")?;
    let file = File::open(&path)?;
    let cloned = file.try_clone()?;
    let original = FileIdentity::from_file(file)?;
    assert_eq!(original, FileIdentity::from_file(cloned)?);
    assert_eq!(original, FileIdentity::from_path(&path)?);
    // Identity is not a size/mtime heuristic.
    fs::write(&path, b"changed length and contents")?;
    assert_eq!(original, FileIdentity::from_path(&path)?);
    Ok(())
}

#[test]
fn file_identity_distinguishes_same_content_files() -> io::Result<()> {
    let home = TempDir::new()?;
    let first = home.path().join("first");
    let second = home.path().join("second");
    fs::write(&first, b"same contents")?;
    fs::write(&second, b"same contents")?;
    assert_ne!(
        FileIdentity::from_path(&first)?,
        FileIdentity::from_path(&second)?
    );
    Ok(())
}

#[test]
fn file_identity_detects_same_content_replacement_with_original_handle_retained() -> io::Result<()>
{
    let home = TempDir::new()?;
    let path = home.path().join("file");
    let replacement = home.path().join("replacement");
    fs::write(&path, b"same contents")?;
    fs::write(&replacement, b"same contents")?;
    let file = File::open(&path)?;
    let cloned = file.try_clone()?;
    let original = FileIdentity::from_file(file)?;
    fs::rename(&replacement, &path)?;
    assert_ne!(original, FileIdentity::from_path(&path)?);
    assert_eq!(original, FileIdentity::from_file(cloned)?);
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_file_identity_compares_all_128_bits() {
    let first = WindowsFileId {
        volume: 7,
        id: [0; 16],
    };
    let mut second = WindowsFileId {
        volume: 7,
        id: [0; 16],
    };
    second.id[15] = 1;
    assert_ne!(first, second);
    second.id[15] = 0;
    second.volume = 8;
    assert_ne!(first, second);
}
