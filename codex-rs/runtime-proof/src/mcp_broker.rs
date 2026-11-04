//! Per-invocation local transport for protected Ops MCP requests.
//!
//! The child sends bounded HTTP request bytes over an inherited Unix
//! sequenced-packet channel. The launcher authenticates every packet with
//! kernel-supplied credentials and performs the remote request itself, keeping
//! the Ops bearer and runtime proof key out of the child.
//!
//! This boundary controls only the signed Ops Work-item authority. The
//! separately provisioned Responses credential and task-readable data remain
//! child capabilities; this broker does not claim general egress confinement.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_exec_server_protocol::HttpRequestParams;
use codex_exec_server_protocol::HttpRequestResponse;
use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::sync::Mutex;
use std::sync::OnceLock;
use uuid::Uuid;

const MAX_PACKET_BYTES: usize = 32 * 1024;
const CHUNK_BYTES: usize = 8 * 1024;
pub(crate) const MAX_MESSAGE_BYTES: usize = 6 * 1024 * 1024;
const BROKER_FD_ENV: &str = "OPS_RUNTIME_BROKER_FD";

#[derive(Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Packet {
    Start {
        request_id: Uuid,
        total_len: usize,
    },
    Data {
        request_id: Uuid,
        sequence: u32,
        bytes: String,
    },
    End {
        request_id: Uuid,
    },
    Ack {
        request_id: Uuid,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct PeerIdentity {
    pub(crate) pid: libc::pid_t,
    pub(crate) uid: libc::uid_t,
}

static CHILD_SOCKET: OnceLock<Mutex<Option<File>>> = OnceLock::new();

pub(crate) fn seqpacket_pair() -> Result<(File, File)> {
    let mut descriptors = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("create MCP broker channel");
    }
    let parent = unsafe { File::from_raw_fd(descriptors[0]) };
    let child = unsafe { File::from_raw_fd(descriptors[1]) };
    enable_passcred(&parent)?;
    Ok((parent, child))
}

pub(crate) fn shutdown(socket: &File) -> Result<()> {
    if unsafe { libc::shutdown(socket.as_raw_fd(), libc::SHUT_RDWR) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::ENOTCONN | libc::EINVAL)) {
        return Ok(());
    }
    Err(error).context("close protected MCP broker channel")
}

pub(crate) fn install_child_socket_from_environment() -> Result<()> {
    let descriptor = std::env::var_os(BROKER_FD_ENV)
        .context("protected MCP broker descriptor is unavailable")?;
    unsafe { std::env::remove_var(BROKER_FD_ENV) };
    let descriptor = descriptor
        .to_str()
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .context("protected MCP broker descriptor is invalid")?
        .parse::<RawFd>()
        .context("protected MCP broker descriptor is out of range")?;
    if descriptor < 3 {
        bail!("protected MCP broker descriptor is a standard descriptor");
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    verify_socket_type(file.as_raw_fd())?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut peer_size = std::mem::size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            file.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut peer_size,
        )
    } != 0
        || peer_size as usize != std::mem::size_of_val(&peer)
        || peer.pid != unsafe { libc::getppid() }
        || peer.uid != 0
    {
        bail!("protected MCP broker peer is not the root launcher");
    }
    let store = CHILD_SOCKET.get_or_init(|| Mutex::new(None));
    let mut slot = store
        .lock()
        .map_err(|_| anyhow::anyhow!("protected MCP broker state is unavailable"))?;
    if slot.is_some() {
        bail!("protected MCP broker was initialized more than once");
    }
    *slot = Some(file);
    Ok(())
}

pub(crate) async fn request(params: HttpRequestParams) -> Result<HttpRequestResponse> {
    let payload = serde_json::to_vec(&params)?;
    if payload.is_empty() || payload.len() > MAX_MESSAGE_BYTES {
        bail!("protected MCP broker request exceeds its size limit");
    }
    let response = tokio::task::spawn_blocking(move || {
        let store = CHILD_SOCKET
            .get()
            .context("protected MCP broker is unavailable")?;
        let mut socket = store
            .lock()
            .map_err(|_| anyhow::anyhow!("protected MCP broker state is unavailable"))?;
        let socket = socket.as_mut().context("protected MCP broker is closed")?;
        let request_id = Uuid::new_v4();
        send_message(socket, request_id, &payload)?;
        receive_message(socket, /*peer*/ None, request_id)
    })
    .await
    .context("protected MCP broker worker failed")??;
    serde_json::from_slice(&response).context("decode protected MCP broker response")
}

pub(crate) fn serve_one(
    socket: &File,
    expected_peer: PeerIdentity,
    active: &std::sync::atomic::AtomicBool,
    child_is_live: &std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    handle: impl FnOnce(Vec<u8>) -> Result<Vec<u8>>,
) -> Result<bool> {
    verify_socket_type(socket.as_raw_fd())?;
    if !active.load(std::sync::atomic::Ordering::Acquire) || !child_is_live() {
        return Ok(false);
    }
    let request_id = match receive_start(socket, Some(expected_peer))? {
        Some((request_id, total_len)) => {
            let request = receive_body(socket, Some(expected_peer), request_id, total_len)?;
            if !active.load(std::sync::atomic::Ordering::Acquire) || !child_is_live() {
                return Ok(false);
            }
            let response = handle(request);
            if !active.load(std::sync::atomic::Ordering::Acquire) || !child_is_live() {
                return Ok(false);
            }
            match response {
                Ok(response) if !response.is_empty() && response.len() <= MAX_MESSAGE_BYTES => {
                    send_message(socket, request_id, &response)?;
                }
                Ok(_) => send_message(
                    socket,
                    request_id,
                    br#"{"error":"protected MCP response exceeds its size limit"}"#,
                )?,
                Err(_) => send_message(
                    socket,
                    request_id,
                    br#"{"error":"protected MCP request was rejected"}"#,
                )?,
            }
            match receive_packet(socket, Some(expected_peer))? {
                Some(Packet::Ack {
                    request_id: acknowledged,
                }) if acknowledged == request_id => {}
                _ => bail!("protected MCP response was not acknowledged by its child"),
            }
            Some(request_id)
        }
        None => None,
    };
    Ok(request_id.is_some())
}

fn send_message(socket: &File, request_id: Uuid, payload: &[u8]) -> Result<()> {
    if payload.is_empty() || payload.len() > MAX_MESSAGE_BYTES {
        bail!("protected MCP broker message exceeds its size limit");
    }
    send_packet(
        socket,
        &Packet::Start {
            request_id,
            total_len: payload.len(),
        },
    )?;
    for (sequence, chunk) in payload.chunks(CHUNK_BYTES).enumerate() {
        send_packet(
            socket,
            &Packet::Data {
                request_id,
                sequence: u32::try_from(sequence)?,
                bytes: URL_SAFE_NO_PAD.encode(chunk),
            },
        )?;
    }
    send_packet(socket, &Packet::End { request_id })
}

fn receive_message(socket: &File, peer: Option<PeerIdentity>, request_id: Uuid) -> Result<Vec<u8>> {
    let Some((response_id, total_len)) = receive_start(socket, peer)? else {
        bail!("protected MCP broker closed before its response");
    };
    if response_id != request_id {
        bail!("protected MCP broker response identity does not match its request");
    }
    let response = receive_body(socket, peer, request_id, total_len)?;
    send_packet(socket, &Packet::Ack { request_id })?;
    Ok(response)
}

fn receive_start(socket: &File, peer: Option<PeerIdentity>) -> Result<Option<(Uuid, usize)>> {
    let packet = match receive_packet(socket, peer)? {
        None => return Ok(None),
        Some(packet) => packet,
    };
    let Packet::Start {
        request_id,
        total_len,
    } = packet
    else {
        bail!("protected MCP broker message did not start with a length frame");
    };
    if total_len == 0 || total_len > MAX_MESSAGE_BYTES {
        bail!("protected MCP broker message has an invalid size");
    }
    Ok(Some((request_id, total_len)))
}

fn receive_body(
    socket: &File,
    peer: Option<PeerIdentity>,
    request_id: Uuid,
    total_len: usize,
) -> Result<Vec<u8>> {
    let mut payload = Vec::with_capacity(total_len);
    let mut sequence = 0_u32;
    while payload.len() < total_len {
        let packet = receive_packet(socket, peer)?
            .context("protected MCP broker closed within a message")?;
        let Packet::Data {
            request_id: packet_id,
            sequence: packet_sequence,
            bytes,
        } = packet
        else {
            bail!("protected MCP broker message has an unexpected frame");
        };
        if packet_id != request_id || packet_sequence != sequence {
            bail!("protected MCP broker message frames are out of order");
        }
        let bytes = URL_SAFE_NO_PAD.decode(bytes.as_bytes())?;
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES || payload.len() + bytes.len() > total_len
        {
            bail!("protected MCP broker message chunk has an invalid size");
        }
        payload.extend_from_slice(&bytes);
        sequence = sequence
            .checked_add(1)
            .context("protected MCP broker chunk sequence overflowed")?;
    }
    let packet =
        receive_packet(socket, peer)?.context("protected MCP broker closed before its message end");
    if packet? != (Packet::End { request_id }) {
        bail!("protected MCP broker message has an invalid end frame");
    }
    Ok(payload)
}

fn send_packet(socket: &File, packet: &Packet) -> Result<()> {
    let bytes = serde_json::to_vec(packet)?;
    if bytes.is_empty() || bytes.len() > MAX_PACKET_BYTES {
        bail!("protected MCP broker packet exceeds its size limit");
    }
    let sent = unsafe {
        libc::send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error()).context("send protected MCP broker packet");
    }
    if usize::try_from(sent)? != bytes.len() {
        bail!("protected MCP broker packet was not sent completely");
    }
    Ok(())
}

fn receive_packet(socket: &File, peer: Option<PeerIdentity>) -> Result<Option<Packet>> {
    let mut bytes = [0_u8; MAX_PACKET_BYTES + 1];
    let mut control = [0_u8; 256];
    let mut vector = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let received =
        unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(std::io::Error::last_os_error()).context("receive protected MCP broker packet");
    }
    if received == 0 {
        return Ok(None);
    }
    if close_received_rights(&message)? {
        bail!("protected MCP broker does not admit descriptor passing");
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || usize::try_from(received)? > MAX_PACKET_BYTES
    {
        bail!("protected MCP broker packet was truncated");
    }
    match peer {
        Some(peer) => verify_message_peer(&message, peer)?,
        None if message.msg_controllen != 0 => {
            bail!("protected MCP broker response carried ancillary data")
        }
        None => {}
    }
    serde_json::from_slice(&bytes[..usize::try_from(received)?])
        .context("decode protected MCP broker packet")
        .map(Some)
}

fn close_received_rights(message: &libc::msghdr) -> Result<bool> {
    let mut found_rights = false;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(message) };
    while !header.is_null() {
        let control_start = message.msg_control as usize;
        let header_start = header as usize;
        let offset = header_start
            .checked_sub(control_start)
            .context("protected MCP ancillary data is outside its buffer")?;
        let header_len = unsafe { libc::CMSG_LEN(0) } as usize;
        let available = message
            .msg_controllen
            .saturating_sub(offset.saturating_add(header_len));
        let cmsg = unsafe { &*header };
        if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
            found_rights = true;
            let declared = cmsg.cmsg_len.saturating_sub(header_len);
            let bytes = declared.min(available);
            let descriptor_count = bytes / std::mem::size_of::<RawFd>();
            let descriptors = unsafe { libc::CMSG_DATA(header).cast::<RawFd>() };
            for index in 0..descriptor_count {
                let descriptor = unsafe { std::ptr::read_unaligned(descriptors.add(index)) };
                if descriptor >= 0 {
                    let _ = unsafe { libc::close(descriptor) };
                }
            }
        }
        header = unsafe { libc::CMSG_NXTHDR(message, header) };
    }
    Ok(found_rights)
}

fn verify_message_peer(message: &libc::msghdr, expected: PeerIdentity) -> Result<()> {
    if message.msg_controllen == 0 {
        bail!("protected MCP broker packet has no kernel credentials");
    }
    let first = unsafe { libc::CMSG_FIRSTHDR(message) };
    if first.is_null() {
        bail!("protected MCP broker packet has invalid ancillary data");
    }
    let header = unsafe { &*first };
    if header.cmsg_level != libc::SOL_SOCKET
        || header.cmsg_type != libc::SCM_CREDENTIALS
        || header.cmsg_len
            != unsafe { libc::CMSG_LEN(std::mem::size_of::<libc::ucred>() as _) } as usize
        || !unsafe { libc::CMSG_NXTHDR(message, first) }.is_null()
    {
        bail!("protected MCP broker packet has unexpected ancillary data");
    }
    let credentials = unsafe { &*libc::CMSG_DATA(first).cast::<libc::ucred>() };
    if credentials.pid != expected.pid || credentials.uid != expected.uid {
        bail!("protected MCP broker packet came from an unexpected process");
    }
    Ok(())
}

fn verify_socket_type(fd: RawFd) -> Result<()> {
    let mut socket_type: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of_val(&socket_type)
        || socket_type != libc::SOCK_SEQPACKET
    {
        bail!("protected MCP broker requires a connected sequenced-packet socket");
    }
    Ok(())
}

fn enable_passcred(socket: &File) -> Result<()> {
    let enabled: libc::c_int = 1;
    if unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&enabled as *const libc::c_int).cast(),
            std::mem::size_of_val(&enabled) as libc::socklen_t,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("enable MCP broker peer credentials");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn broker_frames_are_bound_to_kernel_peer_credentials() {
        let (parent, child) = seqpacket_pair().expect("broker socket pair");
        let request_id = Uuid::new_v4();
        let sender = std::thread::spawn(move || {
            send_message(&child, request_id, b"request").expect("send broker request");
            receive_message(&child, None, request_id).expect("receive broker response")
        });
        let active = AtomicBool::new(true);
        let child_is_live: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| true);
        let served = serve_one(
            &parent,
            PeerIdentity {
                pid: unsafe { libc::getpid() },
                uid: unsafe { libc::getuid() },
            },
            &active,
            &child_is_live,
            |request| {
                if request != b"request" {
                    bail!("unexpected broker test request");
                }
                Ok(b"response".to_vec())
            },
        )
        .expect("serve authenticated request");
        assert!(served);
        assert_eq!(sender.join().expect("join broker sender"), b"response");
    }

    #[test]
    fn broker_rejects_a_different_peer_pid() {
        let (parent, child) = seqpacket_pair().expect("broker socket pair");
        let request_id = Uuid::new_v4();
        let sender = std::thread::spawn(move || {
            send_message(&child, request_id, b"request").expect("send broker request");
        });
        let active = AtomicBool::new(true);
        let child_is_live: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| true);
        let result = serve_one(
            &parent,
            PeerIdentity {
                pid: unsafe { libc::getpid() }.saturating_add(1),
                uid: unsafe { libc::getuid() },
            },
            &active,
            &child_is_live,
            |_| Ok(b"response".to_vec()),
        );
        assert!(result.is_err());
        sender.join().expect("join broker sender");
    }
}
