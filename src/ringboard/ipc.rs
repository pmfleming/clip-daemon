//! Negotiated Ringboard policy extension. Legacy servers fail the handshake
//! before any mutation is sent. See packaging/ringboard-policy/README.md.
use std::{fs::File, io::IoSlice, os::fd::AsFd, time::Duration};

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
            "Safe replacement requires the clip-daemon Ringboard policy package; no history was changed",
        ));
    }
    let mut request = Vec::from(&b"CDP1"[..]);
    request.push(1); // compare-and-replace
    request.extend_from_slice(&id.to_le_bytes());
    request.extend_from_slice(proof);
    request.push(mime_len);
    request.extend_from_slice(mime.as_bytes());
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    let fds = [file.as_fd()];
    if !ancillary.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(ipc_error("Could not attach replacement file"));
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
            "Clipboard content changed before the replacement",
        )),
        _ => Err(ipc_error(
            "Ringboard rejected the replacement; refresh history before retrying",
        )),
    }
}

fn ipc_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::unavailable(format!("Ringboard policy IPC failed: {error}"))
}
