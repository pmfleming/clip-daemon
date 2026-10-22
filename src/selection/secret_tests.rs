//! Native data-control interaction against an isolated compositor, never the user's clipboard.
use std::{
    sync::{Arc, Mutex, atomic::AtomicBool, mpsc},
    time::Duration,
};
use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1 as device, zwlr_data_control_manager_v1 as manager,
    zwlr_data_control_source_v1 as source,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
    protocol::wl_seat,
};
#[derive(Default)]
struct Server {
    sources: Vec<source::ZwlrDataControlSourceV1>,
    offers: Vec<String>,
    clears: usize,
}
impl GlobalDispatch<manager::ZwlrDataControlManagerV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<manager::ZwlrDataControlManagerV1>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ());
    }
}
impl GlobalDispatch<wl_seat::WlSeat, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_seat::WlSeat>,
        _: &(),
        data: &mut DataInit<'_, Self>,
    ) {
        data.init(resource, ());
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_seat::WlSeat,
        _: wl_seat::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl Dispatch<manager::ZwlrDataControlManagerV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &manager::ZwlrDataControlManagerV1,
        request: manager::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        match request {
            manager::Request::CreateDataSource { id } => {
                data.init(id, ());
            }
            manager::Request::GetDataDevice { id, .. } => {
                data.init(id, ());
            }
            _ => {}
        }
    }
}
impl Dispatch<source::ZwlrDataControlSourceV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &source::ZwlrDataControlSourceV1,
        request: source::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let source::Request::Offer { mime_type } = request {
            state.offers.push(mime_type);
        }
    }
}
impl Dispatch<device::ZwlrDataControlDeviceV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &device::ZwlrDataControlDeviceV1,
        request: device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let device::Request::SetSelection { source } = request {
            if let Some(source) = source {
                state.sources.push(source);
            } else {
                state.clears += 1;
            }
        }
    }
}

#[test]
fn native_expiry_destroys_only_its_source_and_never_clears_a_newer_selection() {
    let mut display = Display::<Server>::new().unwrap();
    let mut handle = display.handle();
    handle.create_global::<Server, manager::ZwlrDataControlManagerV1, _>(2, ());
    handle.create_global::<Server, wl_seat::WlSeat, _>(7, ());
    let mut workers = Vec::new();
    let mut acknowledgements = Vec::new();
    for expiry in [Some(Duration::from_millis(100)), None] {
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        handle.insert_client(server, Arc::new(())).unwrap();
        let connection = wayland_client::Connection::from_socket(client).unwrap();
        let (send, recv) = mpsc::sync_channel(1);
        workers.push(std::thread::spawn(move || {
            super::run_on(
                connection,
                b"001234".to_vec(),
                expiry,
                &send,
                &AtomicBool::new(false),
            )
        }));
        acknowledgements.push(recv);
    }
    let state = Arc::new(Mutex::new(Server::default()));
    let stopped = Arc::new(AtomicBool::new(false));
    let observed = state.clone();
    let stop = stopped.clone();
    let server = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            display
                .dispatch_clients(&mut *observed.lock().unwrap())
                .unwrap();
            display.flush_clients().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    for ack in acknowledgements {
        ack.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    }
    std::thread::sleep(Duration::from_millis(200));
    {
        let state = state.lock().unwrap();
        assert_eq!(state.sources.len(), 2);
        assert_eq!(
            state.sources.iter().filter(|s| s.is_alive()).count(),
            1,
            "only the expiring connection is destroyed"
        );
        assert_eq!(
            state.clears, 0,
            "an old timer must never issue set_selection(None)"
        );
        assert_eq!(
            state
                .offers
                .iter()
                .filter(|m| *m == "x-kde-passwordManagerHint")
                .count(),
            2
        );
        for source in &state.sources {
            if source.is_alive() {
                source.cancelled();
            }
        }
    }
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    stopped.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();
}
