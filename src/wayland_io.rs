//! Shared bounded polling; the short timeout keeps cancellation/expiry responsive.
use std::os::fd::BorrowedFd;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::{Connection, EventQueue};

pub(crate) fn read_events<'fd, State, const N: usize>(
    connection: &'fd Connection,
    queue: &mut EventQueue<State>,
    transfers: impl IntoIterator<Item = BorrowedFd<'fd>>,
) -> Result<(), String> {
    queue.flush().map_err(|_| "Wayland flush failed")?;
    let Some(guard) = queue.prepare_read() else {
        return Ok(());
    };
    // N includes the connection. No per-tick allocation, even with transfers.
    let mut fds: [_; N] = std::array::from_fn(|_| PollFd::new(connection, PollFlags::IN));
    let mut count = 1;
    for fd in transfers {
        let slot = fds
            .get_mut(count)
            .ok_or("Too many Wayland transfers to poll")?;
        *slot = PollFd::from_borrowed_fd(fd, PollFlags::IN);
        count += 1;
    }
    let active = fds.get_mut(..count).ok_or("Missing Wayland poll slot")?;
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: 25_000_000,
    };
    match poll(active, Some(&timeout)) {
        Ok(_) if !active[0].revents().is_empty() => {
            guard.read().map_err(|_| "Wayland disconnected")?;
        }
        Ok(_) | Err(rustix::io::Errno::INTR) => {}
        Err(_) => return Err("Wayland poll failed".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Connection, read_events};
    use std::{
        io::Write,
        os::{fd::AsFd, unix::net::UnixStream},
    };

    #[test]
    fn polling_bounds_transfer_wakeups_and_disconnects() {
        let (socket, peer) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut queue = connection.new_event_queue::<()>();
        let (transfer, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"ready").unwrap();
        assert!(read_events::<_, 0>(&connection, &mut queue, []).is_err());
        assert!(read_events::<_, 1>(&connection, &mut queue, [transfer.as_fd()]).is_err());
        assert!(read_events::<_, 2>(&connection, &mut queue, [transfer.as_fd()]).is_ok());
        assert!(read_events::<_, 1>(&connection, &mut queue, []).is_ok());
        drop(peer);
        assert!(read_events::<_, 1>(&connection, &mut queue, []).is_err());
    }
}
