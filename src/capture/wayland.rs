//! Protocol glue; offers are collected fully before any receive request.
use std::{
    collections::HashMap,
    fs::File,
    os::fd::AsFd,
    sync::Arc,
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{OFlags, fcntl_setfl},
    pipe::{PipeFlags, pipe_with},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
    protocol::{wl_callback, wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ext_device, ext_data_control_manager_v1 as ext_manager,
    ext_data_control_offer_v1 as ext_offer,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wlr_device, zwlr_data_control_manager_v1 as wlr_manager,
    zwlr_data_control_offer_v1 as wlr_offer,
};

use super::{
    Generation,
    policy::{MAX_OFFERS, MAX_SEATS, OfferedMimes},
    transfer::{MAX_TRANSFERS, Received, Transfer},
    worker::{Job, Session},
};

enum Offer {
    Ext(ext_offer::ExtDataControlOfferV1),
    Wlr(wlr_offer::ZwlrDataControlOfferV1),
}
impl Offer {
    fn id(&self) -> u32 {
        match self {
            Self::Ext(v) => v.id().protocol_id(),
            Self::Wlr(v) => v.id().protocol_id(),
        }
    }
    fn receive(&self, mime: String, fd: &impl AsFd) {
        match self {
            Self::Ext(v) => v.receive(mime, fd.as_fd()),
            Self::Wlr(v) => v.receive(mime, fd.as_fd()),
        }
    }
}
impl Drop for Offer {
    fn drop(&mut self) {
        match self {
            Self::Ext(v) => v.destroy(),
            Self::Wlr(v) => v.destroy(),
        }
    }
}
enum Manager {
    Ext(ext_manager::ExtDataControlManagerV1),
    Wlr(wlr_manager::ZwlrDataControlManagerV1),
}
enum Device {
    Ext(ext_device::ExtDataControlDeviceV1),
    Wlr(wlr_device::ZwlrDataControlDeviceV1),
}
impl Drop for Device {
    fn drop(&mut self) {
        match self {
            Self::Ext(v) => v.destroy(),
            Self::Wlr(v) => v.destroy(),
        }
    }
}
struct Seat {
    proxy: wl_seat::WlSeat,
    device: Option<Device>,
}
impl Seat {
    fn attach(&mut self, manager: &Option<Manager>, name: u32, qh: &QueueHandle<State>) {
        self.device = match manager {
            Some(Manager::Ext(manager)) => {
                Some(Device::Ext(manager.get_data_device(&self.proxy, qh, name)))
            }
            Some(Manager::Wlr(manager)) => {
                Some(Device::Wlr(manager.get_data_device(&self.proxy, qh, name)))
            }
            None => None,
        };
    }
}
impl Drop for Seat {
    fn drop(&mut self) {
        if self.proxy.version() >= 5 {
            self.proxy.release();
        }
    }
}
struct Pending {
    offer: Offer,
    mimes: OfferedMimes,
    seat: u32,
}
struct Receiving {
    pending: Pending,
    mime: String,
    transfer: Transfer,
    generation: Generation,
}

#[derive(Clone, Copy)]
enum Barrier {
    Registry,
    Ready,
}

struct State {
    session: Arc<Session>,
    registry: wl_registry::WlRegistry,
    ext: Option<u32>,
    wlr: Option<u32>,
    manager: Option<Manager>,
    seats: HashMap<u32, Seat>,
    offers: HashMap<u32, Pending>,
    transfers: Vec<Receiving>,
    ready: bool,
    skip_initial: bool,
    error: Option<&'static str>,
}

impl State {
    fn remove_seat(&mut self, name: u32) {
        self.offers.retain(|_, pending| pending.seat != name);
        self.transfers
            .retain(|receiving| receiving.pending.seat != name);
        self.seats.remove(&name);
    }

    fn offer(&mut self, offer: Offer, seat: u32) {
        if self.offers.len() + self.transfers.len() >= MAX_OFFERS {
            return;
        }
        self.offers.insert(
            offer.id(),
            Pending {
                offer,
                mimes: OfferedMimes::default(),
                seat,
            },
        );
    }

    fn select(&mut self, id: u32) {
        let Some(pending) = self.offers.remove(&id) else {
            return;
        };
        if self.skip_initial && !self.ready {
            return;
        }
        self.start(pending);
    }

    fn start(&mut self, mut pending: Pending) {
        if self.transfers.len() >= MAX_TRANSFERS {
            return;
        }
        let Some(generation) = self.session.control.gate.admit() else {
            return;
        };
        let Some(mime) = pending.mimes.next() else {
            tracing::debug!(reason = "excluded-or-unusable", "capture offer dropped");
            return;
        };
        let limit = self.session.limit;
        let Some(reservation) = self.session.budget.reserve(limit + 1) else {
            return;
        };
        let result = (|| {
            let (read, write) = pipe_with(PipeFlags::CLOEXEC)?;
            fcntl_setfl(&read, OFlags::NONBLOCK)?;
            pending.offer.receive(mime.clone(), &write);
            drop(write);
            Transfer::new(File::from(read), limit, reservation)
        })();
        match result {
            Ok(transfer) => self.transfers.push(Receiving {
                pending,
                mime,
                transfer,
                generation,
            }),
            Err(_) => tracing::warn!(reason = "transfer-start", "capture transfer dropped"),
        }
    }

    fn receive(&mut self) {
        // Move out only the small bounded transfer list; blank fallback may append.
        for mut receiving in std::mem::take(&mut self.transfers) {
            if !self.session.control.gate.accepts(receiving.generation) {
                continue;
            }
            match receiving.transfer.receive(Instant::now()) {
                Ok(false) => self.transfers.push(receiving),
                Ok(true) => self.finish(receiving),
                Err(_) => tracing::debug!(
                    reason = "transfer-limit-timeout-or-io",
                    "capture transfer dropped"
                ),
            }
        }
    }

    fn finish(&mut self, receiving: Receiving) {
        match receiving.transfer.finish() {
            Ok(Received::Blank) => self.start(receiving.pending),
            Ok(Received::Complete(file, reservation)) => {
                let job = Job {
                    file,
                    mime: receiving.mime,
                    generation: receiving.generation,
                    _reservation: reservation,
                };
                // Full means drop, not wait: pause/shutdown always stays responsive.
                if self.session.jobs.try_send(job).is_err() {
                    tracing::debug!(reason = "ingest-backpressure", "capture transfer dropped");
                }
            }
            Err(_) => tracing::warn!(reason = "transfer-finish", "capture transfer dropped"),
        }
    }

    fn initialize(&mut self, connection: &Connection, qh: &QueueHandle<Self>) {
        self.manager = self
            .ext
            .map(|name| Manager::Ext(self.registry.bind(name, 1, qh, ())))
            .or_else(|| {
                self.wlr
                    .map(|name| Manager::Wlr(self.registry.bind(name, 2, qh, ())))
            });
        if self.manager.is_none() || self.seats.is_empty() {
            self.error = Some("Wayland capture requires data-control and a seat");
            return;
        }
        for (&name, seat) in &mut self.seats {
            seat.attach(&self.manager, name, qh);
        }
        connection.display().sync(qh, Barrier::Ready);
    }
}

pub(super) fn run(session: Arc<Session>, skip_initial: bool) -> Result<(), String> {
    let connection =
        Connection::connect_to_env().map_err(|_| "Wayland connection is unavailable")?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut state = State {
        session,
        registry: connection.display().get_registry(&qh, ()),
        ext: None,
        wlr: None,
        manager: None,
        seats: HashMap::new(),
        offers: HashMap::new(),
        transfers: Vec::new(),
        ready: false,
        skip_initial,
        error: None,
    };
    connection.display().sync(&qh, Barrier::Registry);
    let started = Instant::now();
    while !state.session.stopped() {
        queue
            .dispatch_pending(&mut state)
            .map_err(|_| "Wayland event dispatch failed")?;
        if let Some(error) = state.error {
            return Err(error.into());
        }
        if !state.ready && started.elapsed() > Duration::from_secs(5) {
            return Err("Wayland capture initialization timed out".into());
        }
        state.receive();
        queue.flush().map_err(|_| "Wayland capture flush failed")?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        if wait_readable(&connection, &state.transfers)? {
            guard.read().map_err(|_| "Wayland capture disconnected")?;
        }
    }
    Ok(())
}

fn wait_readable(connection: &Connection, transfers: &[Receiving]) -> Result<bool, String> {
    let mut fds = vec![PollFd::new(connection, PollFlags::IN)];
    fds.extend(
        transfers
            .iter()
            .map(|receiving| PollFd::new(&receiving.transfer, PollFlags::IN)),
    );
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: 25_000_000,
    };
    match poll(&mut fds, Some(&timeout)) {
        Ok(_) => Ok(!fds[0].revents().is_empty()),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(_) => Err("Wayland capture poll failed".into()),
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "ext_data_control_manager_v1" => state.ext = Some(name),
                "zwlr_data_control_manager_v1" if version >= 2 => state.wlr = Some(name),
                "wl_seat" if state.seats.len() < MAX_SEATS => {
                    let proxy = registry.bind(name, version.min(7), qh, ());
                    let mut seat = Seat {
                        proxy,
                        device: None,
                    };
                    seat.attach(&state.manager, name, qh);
                    state.seats.insert(name, seat);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.remove_seat(name);
                if state.ready && state.seats.is_empty() {
                    state.error = Some("Wayland capture has no seats");
                }
                if state.ext == Some(name) || state.wlr == Some(name) {
                    state.error = Some("Wayland data-control manager disappeared");
                }
            }
            _ => {}
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
            Barrier::Ready => {
                // A seat/device may disappear between the two sync barriers.
                if state.error.is_some() || state.seats.is_empty() {
                    state.error.get_or_insert("Wayland capture has no seats");
                    return;
                }
                state.ready = true;
                state.session.report(Ok(()));
            }
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ext_manager::ExtDataControlManagerV1);
delegate_noop!(State: ignore wlr_manager::ZwlrDataControlManagerV1);

// Only the generated protocol type/event names differ. Policy stays in State.
macro_rules! dispatch_data_control {
    ($device:ident, $device_ty:ident, $offer:ident, $offer_ty:ident, $variant:ident) => {
        impl Dispatch<$device::$device_ty, u32> for State {
            fn event(state: &mut Self, _: &$device::$device_ty, event: $device::Event,
                seat: &u32, _: &Connection, _: &QueueHandle<Self>) {
                match event {
                    $device::Event::DataOffer { id } => state.offer(Offer::$variant(id), *seat),
                    $device::Event::Selection { id: Some(id) } => state.select(id.id().protocol_id()),
                    $device::Event::PrimarySelection { id: Some(id) } => { state.offers.remove(&id.id().protocol_id()); }
                    $device::Event::Finished => { state.remove_seat(*seat); state.error = Some("Wayland capture device finished"); }
                    _ => {}
                }
            }
            event_created_child!(State, $device::$device_ty, [0 => ($offer::$offer_ty, ())]);
        }
        impl Dispatch<$offer::$offer_ty, ()> for State {
            fn event(state: &mut Self, offer: &$offer::$offer_ty, event: $offer::Event,
                _: &(), _: &Connection, _: &QueueHandle<Self>) {
                if let $offer::Event::Offer { mime_type } = event {
                    if let Some(pending) = state.offers.get_mut(&offer.id().protocol_id()) {
                        pending.mimes.add(mime_type);
                    }
                }
            }
        }
    };
}
dispatch_data_control!(
    ext_device,
    ExtDataControlDeviceV1,
    ext_offer,
    ExtDataControlOfferV1,
    Ext
);
dispatch_data_control!(
    wlr_device,
    ZwlrDataControlDeviceV1,
    wlr_offer,
    ZwlrDataControlOfferV1,
    Wlr
);
