//! Negotiated Ringboard policy extension. Legacy servers fail the handshake
//! before any mutation is sent. See packaging/ringboard-policy/README.md.
use std::{
    fs::File,
    io::{IoSlice, Seek, Write},
    os::fd::{AsFd, OwnedFd},
    time::Duration,
};

use clipboard_history_core::dirs::socket_file;
use rustix::net::{
    AddressFamily, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix,
    SocketFlags, SocketType, connect, recv, send, sendmsg, socket_with,
    sockopt::{Timeout, set_socket_timeout},
};
use sha2::{Digest, Sha256};

use crate::backend::{BackendError, BackendResult};

pub(super) fn content_proof(digest: &[u8; 32], stored_mime: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"clip-daemon:proof:v1:");
    hash.update(digest);
    hash.update(stored_mime.as_bytes());
    hash.finalize().into()
}

pub(super) fn replace(id: u64, proof: &[u8; 32], mime: &str, file: &File) -> BackendResult<()> {
    request(1, id, proof, mime, Some(file))
}

pub(super) fn remove(id: u64, proof: &[u8; 32]) -> BackendResult<()> {
    request(2, id, proof, "", None)
}

pub(super) fn favorite(id: u64, proof: &[u8; 32], value: bool) -> BackendResult<()> {
    request(if value { 3 } else { 4 }, id, proof, "", None)
}

pub(super) fn wipe() -> BackendResult<()> {
    request(5, 0, &[0; 32], "", None)
}

pub(super) fn remove_many(targets: &[(u64, [u8; 32])]) -> BackendResult<()> {
    if targets.is_empty() || targets.len() > 5000 {
        return Err(ipc_error("Invalid deletion count"));
    }
    let mut file = File::from(
        rustix::fs::memfd_create(c"clip-delete-targets", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(ipc_error)?,
    );
    file.write_all(&(targets.len() as u32).to_le_bytes())
        .map_err(ipc_error)?;
    for (id, proof) in targets {
        file.write_all(&id.to_le_bytes())
            .and_then(|()| file.write_all(proof))
            .map_err(ipc_error)?;
    }
    file.rewind().map_err(ipc_error)?;
    request(6, 0, &[0; 32], "", Some(&file))
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct EngineLimits {
    pub max_entries: u32,
    pub max_favorites: u32,
    pub max_entry_bytes: Option<u64>,
}

pub(crate) fn limits() -> BackendResult<EngineLimits> {
    read_limits(send_request(7, 0, &[0; 32], "", None)?)
}

pub(super) fn capture_ready() -> BackendResult<EngineLimits> {
    read_limits(send_request_version(0xc2, 7, 0, &[0; 32], "", None)?)
}

fn read_limits(socket: OwnedFd) -> BackendResult<EngineLimits> {
    let mut response = [0; 20];
    let (_, length) = recv(&socket, &mut response, RecvFlags::TRUNC).map_err(ipc_error)?;
    if length != response.len() || &response[..4] != b"CDS1" {
        return Err(ipc_error(
            "Engine does not expose effective retention limits",
        ));
    }
    let max_bytes = u64::from_le_bytes(response[12..20].try_into().map_err(ipc_error)?);
    Ok(EngineLimits {
        max_entries: u32::from_le_bytes(response[4..8].try_into().map_err(ipc_error)?),
        max_favorites: u32::from_le_bytes(response[8..12].try_into().map_err(ipc_error)?),
        max_entry_bytes: (max_bytes != 0).then_some(max_bytes),
    })
}

/// v2 capture atomically promotes a proven candidate or admits a new entry.
/// None is a confirmed safe rejection; transport errors have uncertain outcome.
pub(super) fn capture(
    id: Option<u64>,
    proof: &[u8; 32],
    mime: &str,
    file: &File,
) -> BackendResult<Option<u64>> {
    let socket = send_request_version(0xc2, 8, id.unwrap_or(u64::MAX), proof, mime, Some(file))?;
    let mut response = [0; 13];
    let (_, length) = recv(&socket, &mut response, RecvFlags::TRUNC).map_err(ipc_error)?;
    if length != response.len() || &response[..4] != b"CDR1" {
        return Err(ipc_error(
            "Capture outcome is unknown; do not retry automatically",
        ));
    }
    match response[4] {
        0 => Ok(Some(u64::from_le_bytes(
            response[5..].try_into().map_err(ipc_error)?,
        ))),
        2 => Ok(None),
        _ => Err(ipc_error("Capture outcome is unknown")),
    }
}

fn request(
    op: u8,
    id: u64,
    proof: &[u8; 32],
    mime: &str,
    file: Option<&File>,
) -> BackendResult<()> {
    let socket = send_request(op, id, proof, mime, file)?;
    let mut response = [0; 5];
    let (_, length) = recv(&socket, &mut response, RecvFlags::TRUNC).map_err(ipc_error)?;
    if length != response.len() || &response[..4] != b"CDR1" {
        return Err(ipc_error(
            "Invalid policy response; refresh history before retrying",
        ));
    }
    match response[4] {
        0 => Ok(()),
        1 => Err(BackendError::stale(
            "Clipboard content changed before the mutation",
        )),
        _ => Err(ipc_error(
            "Ringboard rejected the mutation; refresh history before retrying",
        )),
    }
}

fn send_request(
    op: u8,
    id: u64,
    proof: &[u8; 32],
    mime: &str,
    file: Option<&File>,
) -> BackendResult<OwnedFd> {
    send_request_version(0xc1, op, id, proof, mime, file)
}

fn send_request_version(
    version: u8,
    op: u8,
    id: u64,
    proof: &[u8; 32],
    mime: &str,
    file: Option<&File>,
) -> BackendResult<OwnedFd> {
    let mime_len = u8::try_from(mime.len())
        .ok()
        .filter(|length| *length <= 96)
        .ok_or_else(|| BackendError::unavailable("Replacement MIME exceeds Ringboard's limit"))?;
    let socket = socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(ipc_error)?;
    for timeout in [Timeout::Recv, Timeout::Send] {
        set_socket_timeout(&socket, timeout, Some(Duration::from_secs(10))).map_err(ipc_error)?;
    }
    connect(
        &socket,
        &SocketAddrUnix::new(socket_file()).map_err(ipc_error)?,
    )
    .map_err(ipc_error)?;
    send(&socket, &[version], SendFlags::NOSIGNAL).map_err(ipc_error)?;
    let mut negotiated = [0];
    let (_, length) = recv(&socket, &mut negotiated, RecvFlags::empty()).map_err(ipc_error)?;
    if length != 1 || negotiated != [version] {
        return Err(BackendError::unavailable(
            "Safe mutations require the clip-daemon Ringboard policy package; no history was changed",
        ));
    }
    let mut request = Vec::from(&b"CDP1"[..]);
    request.push(op);
    request.extend_from_slice(&id.to_le_bytes());
    request.extend_from_slice(proof);
    request.push(mime_len);
    request.extend_from_slice(mime.as_bytes());
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    let fds: Vec<_> = file.into_iter().map(AsFd::as_fd).collect();
    if !fds.is_empty() && !ancillary.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(ipc_error("Could not attach policy file"));
    }
    sendmsg(
        &socket,
        &[IoSlice::new(&request)],
        &mut ancillary,
        SendFlags::NOSIGNAL,
    )
    .map_err(ipc_error)?;
    Ok(socket)
}

fn ipc_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::unavailable(format!("Ringboard policy IPC failed: {error}"))
}
