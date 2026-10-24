//! Real protocol/FD traffic over a socket pair, never the desktop compositor.
use crate::capture::worker::{Job, Session};
use std::{
    fs::File,
    io::{Read, Write},
    sync::{Arc, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1 as device, zwlr_data_control_manager_v1 as manager,
    zwlr_data_control_offer_v1 as offer,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
    protocol::wl_seat,
};

type Representation = (&'static str, &'static [u8]);
type Payload = Vec<Representation>;
const BOOTSTRAP: &[Representation] = &[("text/plain", b"boot")];

#[derive(Default)]
struct Server {
    device: Option<device::ZwlrDataControlDeviceV1>,
    requested: Vec<String>,
    destroyed: usize,
    stall: bool,
    stalled: Option<File>,
}
impl Server {
    fn offer(&self, handle: &DisplayHandle, payload: &[Representation], primary: bool) {
        let device = self.device.as_ref().unwrap();
        let resource = device
            .client()
            .unwrap()
            .create_resource::<offer::ZwlrDataControlOfferV1, _, Self>(handle, 1, payload.to_vec())
            .unwrap();
        device.data_offer(&resource);
        for (mime, _) in payload {
            resource.offer((*mime).into());
        }
        if primary {
            device.primary_selection(Some(&resource));
        } else {
            device.selection(Some(&resource));
        }
    }
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
        state: &mut Self,
        _: &Client,
        _: &manager::ZwlrDataControlManagerV1,
        request: manager::Request,
        _: &(),
        handle: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let manager::Request::GetDataDevice { id, .. } = request {
            state.device = Some(data.init(id, ()));
            state.offer(handle, BOOTSTRAP, false);
        }
    }
}
impl Dispatch<device::ZwlrDataControlDeviceV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &device::ZwlrDataControlDeviceV1,
        _: device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl Dispatch<offer::ZwlrDataControlOfferV1, Payload> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &offer::ZwlrDataControlOfferV1,
        request: offer::Request,
        payload: &Payload,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            offer::Request::Receive { mime_type, fd } => {
                let bytes = payload
                    .iter()
                    .find(|(mime, _)| *mime == mime_type)
                    .unwrap()
                    .1;
                state.requested.push(mime_type);
                let mut file = File::from(fd);
                if state.stall {
                    state.stalled = Some(file);
                } else {
                    file.write_all(bytes).unwrap();
                }
            }
            offer::Request::Destroy => state.destroyed += 1,
            _ => {}
        }
    }
}

struct Compositor {
    socket: std::os::unix::net::UnixStream,
    display: Display<Server>,
    state: Server,
    session: Arc<Session>,
    incoming: mpsc::Receiver<Job>,
    worker: Option<JoinHandle<Result<(), String>>>,
}
impl Drop for Compositor {
    fn drop(&mut self) {
        self.session.control.gate.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Compositor {
    fn new(skip_initial: bool) -> Self {
        let display = Display::<Server>::new().unwrap();
        let mut handle = display.handle();
        handle.create_global::<Server, manager::ZwlrDataControlManagerV1, _>(2, ());
        handle.create_global::<Server, wl_seat::WlSeat, _>(7, ());
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let socket = server.try_clone().unwrap();
        handle.insert_client(server, Arc::new(())).unwrap();
        let connection = wayland_client::Connection::from_socket(client).unwrap();
        let (jobs, incoming) = mpsc::sync_channel(1);
        let (ready, mut acknowledged) = tokio::sync::oneshot::channel();
        let session = Arc::new(Session::new(8, jobs, ready));
        session.control.gate.resume().unwrap();
        let worker = {
            let session = session.clone();
            std::thread::spawn(move || super::run_on(connection, session, skip_initial))
        };
        let mut result = Self {
            socket,
            display,
            state: Server::default(),
            session,
            incoming,
            worker: Some(worker),
        };
        result.until(|_| match acknowledged.try_recv() {
            Ok(ready) => {
                ready.unwrap();
                true
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
            Err(error) => panic!("capture never became ready: {error}"),
        });
        result.until(|fixture| fixture.state.destroyed == 1);
        result
    }

    fn until(&mut self, mut complete: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            self.display.dispatch_clients(&mut self.state).unwrap();
            self.display.flush_clients().unwrap();
            if complete(self) {
                return;
            }
            assert!(Instant::now() < deadline, "protocol progress timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn send(&mut self, payload: &[Representation], primary: bool) -> Vec<String> {
        self.state.requested.clear();
        let destroyed = self.state.destroyed;
        self.state.offer(&self.display.handle(), payload, primary);
        self.until(|fixture| fixture.state.destroyed > destroyed);
        std::mem::take(&mut self.state.requested)
    }

    fn received(&mut self) -> Option<(String, Vec<u8>)> {
        match self.incoming.try_recv() {
            Ok(mut job) => {
                assert!(self.session.control.gate.accepts(job.generation));
                let mut bytes = Vec::new();
                job.file.read_to_end(&mut bytes).unwrap();
                Some((job.mime, bytes))
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(error) => panic!("capture job channel closed: {error}"),
        }
    }
}

#[test]
fn native_capture_enforces_offer_policy_bootstrap_and_backpressure() {
    for skip_initial in [false, true] {
        let mut server = Compositor::new(skip_initial);
        assert_eq!(
            server.received(),
            (!skip_initial).then(|| ("text/plain".into(), b"boot".to_vec()))
        );
        assert_eq!(server.state.requested.len(), usize::from(!skip_initial));
        for (payload, primary, requested, expected) in [
            (
                &[
                    ("text/plain", &b"secret"[..]),
                    ("x-kde-passwordManagerHint", b"secret"),
                ][..],
                false,
                &[][..],
                None,
            ),
            (&[("text/plain", &b"primary"[..])][..], true, &[][..], None),
            (
                &[("text/html", &b"<b>x</b>"[..]), ("text/plain", b" \n")][..],
                false,
                &["text/plain", "text/html"][..],
                Some(("text/html", &b"<b>x</b>"[..])),
            ),
            (
                &[
                    ("text/plain", &b"012345678"[..]),
                    ("text/html", b"fallback"),
                ][..],
                false,
                &["text/plain"][..],
                None,
            ),
            (
                &[("UTF8_STRING", &b"hello"[..])][..],
                false,
                &["UTF8_STRING"][..],
                Some(("UTF8_STRING", &b"hello"[..])),
            ),
        ] {
            assert_eq!(server.send(payload, primary), requested);
            assert_eq!(
                server.received(),
                expected.map(|(mime, bytes)| (mime.into(), bytes.to_vec()))
            );
        }
        // A full ingest queue drops new transfers instead of blocking the event loop.
        server.send(&[("text/plain", b"first")], false);
        server.send(&[("text/plain", b"second")], false);
        assert_eq!(
            server.received(),
            Some(("text/plain".into(), b"first".to_vec()))
        );
        assert!(server.received().is_none());
        if skip_initial {
            server.state.device.as_ref().unwrap().finished();
        } else {
            server.socket.shutdown(std::net::Shutdown::Both).unwrap();
        }
        server.until(|fixture| fixture.worker.as_ref().unwrap().is_finished());
        let error = server.worker.take().unwrap().join().unwrap().unwrap_err();
        assert!(error.contains("Wayland"), "{error}");
    }
    let mut server = Compositor::new(true);
    server.state.stall = true;
    server
        .state
        .offer(&server.display.handle(), BOOTSTRAP, false);
    server.until(|fixture| fixture.state.stalled.is_some());
    server.session.control.gate.close();
    server.until(|fixture| fixture.worker.as_ref().unwrap().is_finished());
    server.worker.take().unwrap().join().unwrap().unwrap();
    assert!(server.received().is_none());
    assert_eq!(
        server
            .state
            .stalled
            .as_mut()
            .unwrap()
            .write_all(b"late")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::BrokenPipe
    );
}
