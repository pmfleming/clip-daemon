//! Negotiated Ringboard policy extension. Legacy servers fail the handshake
//! before any mutation is sent. See packaging/ringboard-policy/README.md.
use std::{
    fs::File,
    io::{IoSlice, Seek, Write},
    os::fd::AsFd,
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

fn request(
    op: u8,
    id: u64,
    proof: &[u8; 32],
    mime: &str,
    file: Option<&File>,
) -> BackendResult<()> {
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
    send(&socket, &[0xc1], SendFlags::NOSIGNAL).map_err(ipc_error)?;
    let mut version = [0];
    let (_, length) = recv(&socket, &mut version, RecvFlags::empty()).map_err(ipc_error)?;
    if length != 1 || version != [0xc1] {
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

fn ipc_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::unavailable(format!("Ringboard policy IPC failed: {error}"))
}
