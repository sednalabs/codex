//! Shared length-prefixed JSON framing for independent Windows sandbox IPC protocols.

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Read;
use std::io::Write;

/// Bound the memory used by an individual untrusted IPC frame.
const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

pub(crate) fn write_frame<W: Write, T: Serialize>(mut writer: W, message: &T) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_LEN {
        anyhow::bail!("frame too large: {}", payload.len());
    }
    let len = payload.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn read_frame<R: Read, T: DeserializeOwned>(mut reader: R) -> Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        anyhow::bail!("frame too large: {len}");
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    let message = serde_json::from_slice(&payload)?;
    Ok(Some(message))
}
