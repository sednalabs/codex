use super::*;
use std::collections::VecDeque;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::TempDir;

/// All successful writes reach a real file; failures happen after real progress.
pub(crate) struct FaultFile {
    file: File,
    faults: VecDeque<usize>,
    written: usize,
    flush_error: bool,
    pub(crate) panic_on_write_error: bool,
    pub(crate) first_write: Arc<Mutex<Vec<u8>>>,
}

impl FaultFile {
    pub(crate) fn new(file: File, faults: Vec<usize>, flush_error: bool) -> Self {
        Self {
            file,
            faults: faults.into(),
            written: 0,
            flush_error,
            panic_on_write_error: false,
            first_write: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl AppendIo for FaultFile {
    fn file(&self) -> &File {
        &self.file
    }
}

impl Read for FaultFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.read(bytes)
    }
}

impl Seek for FaultFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

impl Write for FaultFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut first = self.first_write.lock().unwrap();
        if first.is_empty() {
            first.extend_from_slice(bytes);
        }
        drop(first);
        let length = if let Some(limit) = self.faults.front() {
            if self.written == *limit {
                self.faults.pop_front();
                assert!(
                    !self.panic_on_write_error,
                    "injected append worker panic after real progress"
                );
                return Err(io::Error::other(
                    "injected write failure after real progress",
                ));
            }
            bytes.len().min(*limit - self.written)
        } else {
            bytes.len()
        };
        let count = self.file.write(&bytes[..length])?;
        self.written += count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.flush_error) {
            return Err(io::Error::other("injected flush failure after full write"));
        }
        self.file.flush()
    }
}

fn append_file(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
}

#[test]
fn owned_append_missing_checkpoint_fails_closed() -> io::Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("rollout.jsonl");
    let original = b"existing bytes\n";
    fs::write(&path, original)?;
    let mut file = append_file(&path)?;
    let mut pending = PendingAppend::new(b"owned append\n".to_vec());

    let err = pending
        .verified_prefix(&mut file, &path)
        .expect_err("a missing checkpoint must fail closed");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("checkpoint is missing"));
    assert!(pending.checkpoint.is_none());
    let retry = pending
        .finish(&mut file, &path)
        .expect_err("the conflict must remain sticky on retry");
    assert_eq!(retry.to_string(), err.to_string());
    assert!(pending.checkpoint.is_none());
    assert_eq!(fs::read(&path)?, original);
    Ok(())
}

#[tokio::test]
async fn owned_append_retries_only_suffix_after_repeated_partial_progress() -> io::Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("rollout.jsonl");
    let meta = codex_protocol::protocol::SessionMeta {
        originator: "x".repeat(4096),
        ..Default::default()
    };
    let id = meta.id;
    let mut bytes = serde_json::to_vec(&crate::RolloutLine {
        timestamp: "2026-10-19T00:00:00.001Z".to_owned(),
        ordinal: Some(0),
        item: crate::RolloutItem::SessionMeta(codex_protocol::protocol::SessionMetaLine {
            meta,
            git: None,
        }),
    })?;
    bytes.push(b'\n');
    let mut pending = PendingAppend::new(bytes.clone());
    let mut file = FaultFile::new(append_file(&path)?, vec![1025, 2049], /*flush_error*/ false);
    for length in [1025, 2049] {
        pending
            .finish(&mut file, &path)
            .expect_err("real partial write fails");
        assert_eq!(fs::read(&path)?, bytes[..length]);
    }
    pending.finish(&mut file, &path)?;
    assert_eq!(fs::read(&path)?, bytes);
    let (items, loaded_id, parse_errors) =
        crate::RolloutRecorder::load_rollout_items(&path).await?;
    assert_eq!(items.len(), 1);
    assert_eq!(loaded_id, Some(id));
    assert_eq!(parse_errors, 0);
    Ok(())
}

#[test]
fn owned_append_acknowledges_full_line_after_flush_error_without_duplication() -> io::Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("rollout.jsonl");
    let bytes = b"{\"message\":\"complete\"}\n".to_vec();
    let mut pending = PendingAppend::new(bytes.clone());
    let mut file = FaultFile::new(append_file(&path)?, vec![], /*flush_error*/ true);
    pending
        .finish(&mut file, &path)
        .expect_err("flush fails after full write");
    drop(file);
    pending.finish(&mut append_file(&path)?, &path)?;
    assert_eq!(fs::read(&path)?, bytes);
    Ok(())
}

#[test]
fn owned_append_conflicts_are_sticky_and_never_modify_foreign_bytes() -> io::Result<()> {
    for mutation in ["tail", "extra", "shrink", "replace", "witness"] {
        let home = TempDir::new()?;
        let path = home.path().join("rollout.jsonl");
        fs::write(&path, b"prefix\n")?;
        let bytes = vec![b'x'; 4096];
        let mut pending = PendingAppend::new(bytes.clone());
        let mut file = FaultFile::new(append_file(&path)?, vec![1025], /*flush_error*/ false);
        pending
            .finish(&mut file, &path)
            .expect_err("partial progress");
        drop(file);
        let original = fs::read(&path)?;
        match mutation {
            "tail" => {
                let mut changed = original.clone();
                changed[8] = b'y';
                fs::write(&path, changed)?;
            }
            "extra" => {
                append_file(&path)?.write_all(&vec![b'x'; 4096])?;
            }
            "shrink" => {
                File::options().write(true).open(&path)?.set_len(3)?;
            }
            "replace" => {
                let replacement = home.path().join("replacement");
                fs::write(&replacement, &original)?;
                fs::rename(replacement, &path)?;
            }
            "witness" => {
                let mut changed = original.clone();
                changed[0] = b'P';
                fs::write(&path, changed)?;
            }
            _ => unreachable!(),
        }
        let before = fs::read(&path)?;
        let err = pending
            .finish(&mut append_file(&path)?, &path)
            .expect_err(mutation);
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{mutation}");
        assert_eq!(fs::read(&path)?, before, "{mutation}");
        // Even restoring the expected bytes must not silently clear a conflict.
        fs::write(&path, &original)?;
        assert_eq!(
            pending
                .finish(&mut append_file(&path)?, &path)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&path)?, original);
    }
    Ok(())
}
