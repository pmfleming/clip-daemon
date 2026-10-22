//! Memory-only data-control source. Expiry drops *this connection*, never clears
//! the seat selection, so a newer owner cannot be erased by an old timer.
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{OFlags, fcntl_setfl},
};
use std::{
    fs::File,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop, event_created_child,
    protocol::{wl_callback, wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ed, ext_data_control_manager_v1 as em,
    ext_data_control_offer_v1 as eo, ext_data_control_source_v1 as es,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wd, zwlr_data_control_manager_v1 as wm,
    zwlr_data_control_offer_v1 as wo, zwlr_data_control_source_v1 as ws,
};

const MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "x-kde-passwordManagerHint",
    "application/x-shelllist-sensitive",
];
static OWNERS: AtomicUsize = AtomicUsize::new(0);
struct Admission;
impl Drop for Admission {
    fn drop(&mut self) {
        OWNERS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(super) fn publish(text: Vec<u8>, seconds: u16) -> Result<(), String> {
    if text.is_empty()
        || text.len() > 256
        || std::str::from_utf8(&text).is_err()
        || (seconds != 0 && !(10..=300).contains(&seconds))
    {
        return Err("Invalid sensitive clipboard payload or expiry".into());
    }
    OWNERS
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < 16).then_some(n + 1)
        })
        .map_err(|_| "Too many sensitive clipboard owners")?;
    let admission = Admission;
    let (send, recv) = mpsc::sync_channel(1);
    // If the caller times out, the worker must not publish later.
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel = cancelled.clone();
    std::thread::Builder::new()
        .name("secret-selection".into())
        .spawn(move || {
            let _admission = admission;
            if let Err(error) = run(text, seconds, &send, &cancel) {
                let _ = send.try_send(Err(error));
            }
        })
        .map_err(|_| "Cannot start sensitive clipboard owner")?;
    let result = recv
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "Sensitive clipboard ownership was not acknowledged".to_string())
        .and_then(|v| v);
    if result.is_err() {
        cancelled.store(true, Ordering::SeqCst);
    }
    result
}

#[derive(Clone, Copy)]
enum Barrier {
    Registry,
    Owned,
}
struct State {
    registry: wl_registry::WlRegistry,
    seats: Vec<wl_seat::WlSeat>,
    ext: Option<u32>,
    wlr: Option<u32>,
    text: Vec<u8>,
    owners: usize,
    owned: bool,
    error: Option<&'static str>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.text.fill(0);
    }
}
impl State {
    fn initialize(&mut self, connection: &Connection, qh: &QueueHandle<Self>) {
        if self.seats.is_empty() {
            self.error = Some("No Wayland seat");
            return;
        }
        if let Some(name) = self.ext {
            let manager: em::ExtDataControlManagerV1 = self.registry.bind(name, 1, qh, ());
            for seat in &self.seats {
                let source = manager.create_data_source(qh, ());
                for mime in MIMES {
                    source.offer((*mime).into());
                }
                manager
                    .get_data_device(seat, qh, ())
                    .set_selection(Some(&source));
                self.owners += 1;
            }
        } else if let Some(name) = self.wlr {
            let manager: wm::ZwlrDataControlManagerV1 = self.registry.bind(name, 2, qh, ());
            for seat in &self.seats {
                let source = manager.create_data_source(qh, ());
                for mime in MIMES {
                    source.offer((*mime).into());
                }
                manager
                    .get_data_device(seat, qh, ())
                    .set_selection(Some(&source));
                self.owners += 1;
            }
        } else {
            self.error = Some("Sensitive clipboard requires Wayland data-control");
            return;
        }
        connection.display().sync(qh, Barrier::Owned);
    }
    fn send(&self, mime: &str, fd: std::os::fd::OwnedFd) {
        let mut file = File::from(fd);
        // Never block the expiry loop on a stalled/malicious paste client.
        if fcntl_setfl(&file, OFlags::NONBLOCK).is_err() {
            return;
        }
        let data: &[u8] = match mime {
            "text/plain;charset=utf-8" | "text/plain" | "UTF8_STRING" => &self.text,
            "x-kde-passwordManagerHint" => b"secret",
            "application/x-shelllist-sensitive" => b"1",
            _ => return,
        };
        let _ = file.write_all(data);
    }
}
fn run(
    text: Vec<u8>,
    seconds: u16,
    ack: &mpsc::SyncSender<Result<(), String>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let connection = Connection::connect_to_env().map_err(|_| "Wayland connection unavailable")?;
    run_on(
        connection,
        text,
        (seconds > 0).then(|| Duration::from_secs(u64::from(seconds))),
        ack,
        cancel,
    )
}
fn run_on(
    connection: Connection,
    text: Vec<u8>,
    expiry: Option<Duration>,
    ack: &mpsc::SyncSender<Result<(), String>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut state = State {
        registry: connection.display().get_registry(&qh, ()),
        seats: vec![],
        ext: None,
        wlr: None,
        text,
        owners: 0,
        owned: false,
        error: None,
    };
    connection.display().sync(&qh, Barrier::Registry);
    let started = Instant::now();
    let mut deadline = None;
    let mut acknowledged = false;
    loop {
        if cancel.load(Ordering::SeqCst) || deadline.is_some_and(|until| Instant::now() >= until) {
            return Ok(());
        }
        if !state.owned && started.elapsed() > Duration::from_secs(2) {
            return Err("Sensitive clipboard initialization timed out".into());
        }
        queue
            .dispatch_pending(&mut state)
            .map_err(|_| "Sensitive clipboard dispatch failed")?;
        if let Some(error) = state.error {
            return Err(error.into());
        }
        if state.owned {
            if state.owners == 0 {
                return Ok(());
            }
            if !acknowledged {
                deadline = expiry.map(|duration| Instant::now() + duration);
                ack.send(Ok(()))
                    .map_err(|_| "Sensitive clipboard caller disappeared")?;
                acknowledged = true;
            }
        }
        queue
            .flush()
            .map_err(|_| "Sensitive clipboard flush failed")?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds = [PollFd::new(&connection, PollFlags::IN)];
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 25_000_000,
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) if !fds[0].revents().is_empty() => {
                guard
                    .read()
                    .map_err(|_| "Sensitive clipboard disconnected")?;
            }
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => return Err("Sensitive clipboard poll failed".into()),
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "ext_data_control_manager_v1" => state.ext = Some(name),
                "zwlr_data_control_manager_v1" if version >= 2 => state.wlr = Some(name),
                "wl_seat" if state.seats.len() < 16 => {
                    state
                        .seats
                        .push(state.registry.bind(name, version.min(7), qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, Barrier> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        barrier: &Barrier,
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match barrier {
            Barrier::Registry => state.initialize(connection, qh),
            Barrier::Owned => state.owned = true,
        }
    }
}
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore em::ExtDataControlManagerV1);
delegate_noop!(State: ignore wm::ZwlrDataControlManagerV1);
macro_rules! protocol {
    ($device:ident, $dt:ident, $source:ident, $st:ident, $offer:ident, $ot:ident) => {
        impl Dispatch<$source::$st, ()> for State {
            fn event(state: &mut Self, source: &$source::$st, event: $source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                match event {
                    $source::Event::Send { mime_type, fd } => state.send(&mime_type, fd),
                    $source::Event::Cancelled => { source.destroy(); state.owners = state.owners.saturating_sub(1); }
                    _ => {}
                }
            }
        }
        impl Dispatch<$device::$dt, ()> for State {
            fn event(state: &mut Self, _: &$device::$dt, event: $device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                match event {
                    $device::Event::DataOffer { id } => id.destroy(),
                    $device::Event::Finished => state.error = Some("Sensitive clipboard device disappeared"),
                    _ => {}
                }
            }
            event_created_child!(State, $device::$dt, [0 => ($offer::$ot, ())]);
        }
        delegate_noop!(State: ignore $offer::$ot);
    };
}
protocol!(
    ed,
    ExtDataControlDeviceV1,
    es,
    ExtDataControlSourceV1,
    eo,
    ExtDataControlOfferV1
);
protocol!(
    wd,
    ZwlrDataControlDeviceV1,
    ws,
    ZwlrDataControlSourceV1,
    wo,
    ZwlrDataControlOfferV1
);

#[cfg(test)]
#[path = "secret_tests.rs"]
mod protocol_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn sensitive_markers_are_always_offered_and_invalid_requests_never_open_wayland() {
        assert!(super::MIMES.contains(&"x-kde-passwordManagerHint"));
        assert!(super::MIMES.contains(&"application/x-shelllist-sensitive"));
        for (text, seconds) in [
            (vec![], 0),
            (vec![b'1'; 257], 0),
            (b"123456".to_vec(), 301),
            (vec![255], 30),
        ] {
            assert!(super::publish(text, seconds).is_err());
        }
        assert_eq!(super::OWNERS.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
