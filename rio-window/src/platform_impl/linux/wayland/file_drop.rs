//! File-only DND on the window connection. Clipboard/PRIMARY ownership is deliberately
//! left to copypasta (which uses a different connection).
//!
//! Use SCTK's manager, per-seat devices and receive pipe, but dispatch offers ourselves:
//! this lets us destroy ignored selection/undetermined offers and keep dropped offers
//! alive independently of subsequent enter/leave events until asynchronous I/O ends.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use ahash::AHashMap;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, PostAction, RegistrationToken};
use sctk::data_device_manager::data_device::{DataDevice, DataDeviceData};
use sctk::data_device_manager::{data_offer, DataDeviceManagerState};
use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};

use super::state::WinitState;
use super::window::{WindowRequests, WindowState};
use super::WindowId;
use crate::event::WindowEvent;

const URI_LIST: &str = "text/uri-list";
const MAX_PAYLOAD: usize = 1024 * 1024;
const MAX_PATHS: usize = 4096;
const READ_BUDGET: usize = 64 * 1024;
const MAX_READ_CALLS: usize = 16;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
// Also bound offers that a broken source never assigns to selection or DND.
const MAX_OFFERS_PER_SEAT: usize = 32;

pub(super) struct FileDropState {
    manager: Option<DataDeviceManagerState>,
    devices: AHashMap<ObjectId, SeatDevice>,
    offers: AHashMap<ObjectId, Offer>,
    loop_handle: LoopHandle<'static, WinitState>,
}

struct SeatDevice {
    _device: DataDevice,
    drag: Option<ObjectId>,
    transfer: Option<ObjectId>,
}

struct Offer {
    proxy: WlDataOffer,
    seat: ObjectId,
    uri_list: bool,
    source_actions: DndAction,
    selected_action: DndAction,
    target: Option<Target>,
    accepted: bool,
    transfer: Option<Transfer>,
}

struct Target {
    id: WindowId,
    window: Weak<Mutex<WindowState>>,
    requests: Weak<WindowRequests>,
}

impl Target {
    fn is_live(&self, state: &WinitState) -> bool {
        let Some(requests) = self.requests.upgrade() else {
            return false;
        };
        if requests.closed.load(Ordering::Relaxed) {
            return false;
        }
        let Some(window) = self.window.upgrade() else {
            return false;
        };
        state
            .windows
            .borrow()
            .get(&self.id)
            .is_some_and(|current| Arc::ptr_eq(current, &window))
    }
}

struct Transfer {
    payload: Vec<u8>,
    pipe: Option<RegistrationToken>,
    timer: Option<RegistrationToken>,
}

impl Transfer {
    fn cancel(&mut self, handle: &LoopHandle<'_, WinitState>) {
        for token in [self.pipe.take(), self.timer.take()].into_iter().flatten() {
            handle.remove(token);
        }
    }
}

impl Offer {
    fn copy_allowed(&self) -> bool {
        copy_allowed(
            self.proxy.version(),
            self.accepted,
            self.source_actions,
            self.selected_action,
        )
    }
}

fn copy_allowed(
    version: u32,
    accepted: bool,
    source: DndAction,
    selected: DndAction,
) -> bool {
    // v1/v2 have no action negotiation or finish request: receiving is implicitly
    // COPY. For v3 require an explicitly negotiated COPY, never MOVE or ASK.
    accepted
        && (version < 3
            || (source.contains(DndAction::Copy) && selected == DndAction::Copy))
}

impl FileDropState {
    pub(super) fn new(
        globals: &GlobalList,
        qh: &QueueHandle<WinitState>,
        loop_handle: LoopHandle<'static, WinitState>,
    ) -> Self {
        Self {
            manager: DataDeviceManagerState::bind(globals, qh).ok(),
            devices: Default::default(),
            offers: Default::default(),
            loop_handle,
        }
    }

    pub(super) fn add_seat(&mut self, seat: &WlSeat, qh: &QueueHandle<WinitState>) {
        if self.devices.contains_key(&seat.id()) {
            return;
        }
        if let Some(manager) = &self.manager {
            self.devices.insert(
                seat.id(),
                SeatDevice {
                    _device: manager.get_data_device(qh, seat),
                    drag: None,
                    transfer: None,
                },
            );
        }
    }

    pub(super) fn remove_seat(&mut self, seat: &ObjectId) {
        let offers: Vec<_> = self
            .offers
            .iter()
            .filter(|(_, offer)| &offer.seat == seat)
            .map(|(id, _)| id.clone())
            .collect();
        for id in offers {
            self.remove_offer(&id);
        }
        // SCTK releases wl_data_device for v2+, and gracefully drops v1 proxies.
        self.devices.remove(seat);
    }

    pub(super) fn remove_window(&mut self, window: WindowId) {
        let offers: Vec<_> = self
            .offers
            .iter()
            .filter(|(_, offer)| {
                offer
                    .target
                    .as_ref()
                    .is_some_and(|target| target.id == window)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in offers {
            self.remove_offer(&id);
        }
    }

    fn remove_offer(&mut self, id: &ObjectId) {
        if let Some(mut offer) = self.offers.remove(id) {
            if let Some(mut transfer) = offer.transfer.take() {
                transfer.cancel(&self.loop_handle);
            }
            if let Some(device) = self.devices.get_mut(&offer.seat) {
                if device.drag.as_ref() == Some(id) {
                    device.drag = None;
                }
                if device.transfer.as_ref() == Some(id) {
                    device.transfer = None;
                }
            }
            offer.proxy.destroy();
        }
    }
}

impl Drop for FileDropState {
    fn drop(&mut self) {
        let ids: Vec<_> = self.offers.keys().cloned().collect();
        for id in ids {
            self.remove_offer(&id);
        }
    }
}

impl WinitState {
    fn enter_file_drag(
        &mut self,
        seat: &ObjectId,
        serial: u32,
        surface: &wayland_client::protocol::wl_surface::WlSurface,
        id: Option<WlDataOffer>,
    ) {
        if let Some(old) = self
            .file_drop
            .devices
            .get_mut(seat)
            .and_then(|device| device.drag.take())
        {
            self.file_drop.remove_offer(&old);
        }
        let Some(proxy) = id else { return };
        let Some(offer) = self.file_drop.offers.get_mut(&proxy.id()) else {
            return;
        };
        // Never associate an offer from another seat or a previous dropped offer.
        if &offer.seat != seat || offer.target.is_some() || offer.transfer.is_some() {
            return;
        }
        let window_id = super::make_wid(surface);
        offer.target = self.windows.borrow().get(&window_id).and_then(|window| {
            self.window_requests
                .borrow()
                .get(&window_id)
                .map(|requests| Target {
                    id: window_id,
                    window: Arc::downgrade(window),
                    requests: Arc::downgrade(requests),
                })
        });
        offer.accepted = offer.uri_list && offer.target.is_some();
        proxy.accept(serial, offer.accepted.then(|| URI_LIST.to_owned()));
        if proxy.version() >= 3 {
            // COPY only, including when the source only supports MOVE/ASK. In that
            // case negotiation yields no action and the drop is rejected.
            proxy.set_actions(DndAction::Copy, DndAction::Copy);
        }
        self.file_drop.devices.get_mut(seat).unwrap().drag = Some(proxy.id());
    }

    fn drop_files(&mut self, seat: &ObjectId) {
        let Some(id) = self
            .file_drop
            .devices
            .get_mut(seat)
            .and_then(|device| device.drag.take())
        else {
            return;
        };
        let valid = self.file_drop.offers.get(&id).is_some_and(|offer| {
            offer.copy_allowed()
                && offer
                    .target
                    .as_ref()
                    .is_some_and(|target| target.is_live(self))
        });
        if !valid {
            self.file_drop.remove_offer(&id);
            return;
        }
        // At most one bounded transfer per seat. A new drop cancels a stalled old one.
        if let Some(old) = self
            .file_drop
            .devices
            .get_mut(seat)
            .and_then(|device| device.transfer.take())
        {
            self.file_drop.remove_offer(&old);
        }
        let proxy = &self.file_drop.offers[&id].proxy;
        let pipe =
            match data_offer::receive(proxy, URI_LIST.to_owned()).and_then(|pipe| {
                set_nonblocking(&pipe)?;
                Ok(pipe)
            }) {
                Ok(pipe) => pipe,
                Err(error) => {
                    tracing::warn!("Failed to receive Wayland file drop: {error}");
                    self.file_drop.remove_offer(&id);
                    return;
                }
            };
        let callback_id = id.clone();
        let pipe_token =
            match self.loop_handle.insert_source(pipe, move |_, file, state| {
                // Reading through &File leaves calloop's registered FD in place.
                let mut file = file.as_ref();
                state.read_file_drop(&callback_id, &mut file)
            }) {
                Ok(token) => token,
                Err(error) => {
                    tracing::warn!(
                        "Failed to register Wayland file drop pipe: {}",
                        error.error
                    );
                    self.file_drop.remove_offer(&id);
                    return;
                }
            };
        self.file_drop.offers.get_mut(&id).unwrap().transfer = Some(Transfer {
            payload: Vec::new(),
            pipe: Some(pipe_token),
            timer: None,
        });
        self.file_drop.devices.get_mut(seat).unwrap().transfer = Some(id.clone());
        let callback_id = id.clone();
        match self.loop_handle.insert_source(
            Timer::from_duration(TRANSFER_TIMEOUT),
            move |_, _, state| {
                if let Some(transfer) = state
                    .file_drop
                    .offers
                    .get_mut(&callback_id)
                    .and_then(|offer| offer.transfer.as_mut())
                {
                    // This timer removes itself via TimeoutAction; cancel only the pipe.
                    transfer.timer = None;
                }
                state.file_drop.remove_offer(&callback_id);
                TimeoutAction::Drop
            },
        ) {
            Ok(token) => {
                self.file_drop
                    .offers
                    .get_mut(&id)
                    .unwrap()
                    .transfer
                    .as_mut()
                    .unwrap()
                    .timer = Some(token)
            }
            Err(error) => {
                tracing::warn!(
                    "Failed to register Wayland file drop timeout: {}",
                    error.error
                );
                self.file_drop.remove_offer(&id);
            }
        }
    }

    fn read_file_drop(&mut self, id: &ObjectId, reader: &mut impl Read) -> PostAction {
        let live = self.file_drop.offers.get(id).is_some_and(|offer| {
            offer.copy_allowed()
                && offer
                    .target
                    .as_ref()
                    .is_some_and(|target| target.is_live(self))
        });
        let Some(offer) = self.file_drop.offers.get_mut(id) else {
            return PostAction::Remove;
        };
        let Some(transfer) = offer.transfer.as_mut() else {
            return PostAction::Remove;
        };
        let result = if live {
            read_bounded(reader, &mut transfer.payload, MAX_PAYLOAD, READ_BUDGET)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "drop target is gone or action is no longer COPY",
            ))
        };
        if matches!(result, Ok(ReadProgress::Pending)) {
            return PostAction::Continue;
        }
        // Do not remove the currently executing source through LoopHandle.
        transfer.pipe = None;
        match result {
            Ok(ReadProgress::Eof) => match parse_uri_list(&transfer.payload) {
                Ok(paths) if !paths.is_empty() => {
                    let window = offer.target.as_ref().unwrap().id;
                    for path in paths {
                        self.events_sink
                            .push_window_event(WindowEvent::DroppedFile(path), window);
                    }
                    // Standalone FD callbacks don't pass through the Wayland dispatcher.
                    self.dispatched_events = true;
                    if offer.proxy.version() >= 3 {
                        offer.proxy.finish();
                    }
                }
                Ok(_) => (),
                Err(error) => tracing::debug!("Cancelled Wayland file drop: {error}"),
            },
            Err(error) => tracing::debug!("Cancelled Wayland file drop: {error}"),
            Ok(ReadProgress::Pending) => unreachable!(),
        }
        self.file_drop.remove_offer(id);
        PostAction::Remove
    }
}

// We deliberately don't delegate wl_data_device/wl_data_offer to SCTK: its
// retained drag/selection slots would otherwise also destroy the offers we own.
wayland_client::delegate_dispatch!(WinitState: [WlDataDeviceManager: sctk::globals::GlobalData] => DataDeviceManagerState);

impl Dispatch<WlDataDevice, DataDeviceData> for WinitState {
    wayland_client::event_created_child!(WinitState, WlDataDevice, [0 => (WlDataOffer, ())]);

    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        data: &DataDeviceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let seat = data.seat().id();
        if !state.file_drop.devices.contains_key(&seat) {
            if let wl_data_device::Event::DataOffer { id } = event {
                id.destroy();
            }
            return;
        }
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if state
                    .file_drop
                    .offers
                    .values()
                    .filter(|offer| offer.seat == seat)
                    .count()
                    >= MAX_OFFERS_PER_SEAT
                {
                    id.destroy();
                    return;
                }
                state.file_drop.offers.insert(
                    id.id(),
                    Offer {
                        proxy: id,
                        seat,
                        uri_list: false,
                        source_actions: DndAction::empty(),
                        selected_action: DndAction::empty(),
                        target: None,
                        accepted: false,
                        transfer: None,
                    },
                );
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                id,
                ..
            } => state.enter_file_drag(&seat, serial, &surface, id),
            wl_data_device::Event::Leave => {
                if let Some(id) = state
                    .file_drop
                    .devices
                    .get_mut(&seat)
                    .and_then(|device| device.drag.take())
                {
                    state.file_drop.remove_offer(&id);
                }
                // A dropped offer belongs to its transfer, not the pointer focus.
            }
            wl_data_device::Event::Drop => state.drop_files(&seat),
            wl_data_device::Event::Selection { id: Some(id) } => {
                state.file_drop.remove_offer(&id.id())
            }
            _ => (),
        }
    }
}

impl Dispatch<WlDataOffer, ()> for WinitState {
    fn event(
        state: &mut Self,
        proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(offer) = state.file_drop.offers.get_mut(&proxy.id()) else {
            return;
        };
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                offer.uri_list |= mime_type == URI_LIST
            }
            wl_data_offer::Event::SourceActions { source_actions } => {
                offer.source_actions = match source_actions {
                    WEnum::Value(actions) => actions,
                    _ => DndAction::empty(),
                };
            }
            wl_data_offer::Event::Action { dnd_action } => {
                offer.selected_action = match dnd_action {
                    WEnum::Value(action) => action,
                    _ => DndAction::empty(),
                };
            }
            _ => (),
        }
        // There is no destination-side "cancelled" event. A leave cancels an
        // undropped offer; a revoked COPY action cancels an active transfer.
        if offer.transfer.is_some() && !offer.copy_allowed() {
            state.file_drop.remove_offer(&proxy.id());
        }
    }
}

fn set_nonblocking(fd: &impl AsFd) -> io::Result<()> {
    let fd = fd.as_fd().as_raw_fd();
    // SAFETY: fcntl only inspects/updates flags on this borrowed, live descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum ReadProgress {
    Pending,
    Eof,
}

/// Both bytes and syscalls (including EINTR) are budgeted. Never publish partial
/// data: an error, timeout or over-limit payload discards the entire transfer.
fn read_bounded(
    reader: &mut impl Read,
    payload: &mut Vec<u8>,
    limit: usize,
    budget: usize,
) -> io::Result<ReadProgress> {
    let mut remaining = budget;
    let mut buffer = [0; 8192];
    for _ in 0..MAX_READ_CALLS {
        if remaining == 0 {
            break;
        }
        // One extra byte distinguishes exactly-at-limit EOF from overflow.
        let length = buffer
            .len()
            .min(remaining)
            .min(limit.saturating_sub(payload.len()).saturating_add(1));
        match reader.read(&mut buffer[..length]) {
            Ok(0) => return Ok(ReadProgress::Eof),
            Ok(n) => {
                if n > limit.saturating_sub(payload.len()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "file drop URI list exceeds size limit",
                    ));
                }
                payload.extend_from_slice(&buffer[..n]);
                remaining -= n;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(ReadProgress::Pending)
}

/// Validate the entire list before publishing any events. Exceeding either limit
/// rejects the whole transfer rather than silently dropping paths from its tail.
fn parse_uri_list(bytes: &[u8]) -> io::Result<Vec<PathBuf>> {
    if bytes.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file drop URI list exceeds size limit",
        ));
    }
    let mut paths = Vec::new();
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        // Split BEFORE decoding: encoded newlines, #, %, CR, etc. are path bytes.
        let line = if let Some(line) = line.strip_suffix(b"\n") {
            line.strip_suffix(b"\r").unwrap_or(line)
        } else {
            line
        };
        if let Some(path) = parse_local_uri(line) {
            if paths.len() == MAX_PATHS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file drop URI list exceeds path count limit",
                ));
            }
            paths.push(path);
        }
    }
    Ok(paths)
}

fn parse_local_uri(line: &[u8]) -> Option<PathBuf> {
    if line.is_empty() || line.starts_with(b"#") {
        return None;
    }
    if line
        .iter()
        .any(|&b| b < b' ' || b == 0x7f || b == b'?' || b == b'#')
    {
        return None;
    }
    let rest = line
        .get(7..)
        .filter(|_| line[..7].eq_ignore_ascii_case(b"file://"))?;
    let path = if rest.starts_with(b"/") {
        rest
    } else {
        let slash = rest.iter().position(|&b| b == b'/')?;
        if !rest[..slash].eq_ignore_ascii_case(b"localhost") {
            return None;
        }
        &rest[slash..]
    };
    let mut i = 0;
    while i < path.len() {
        if path[i] == b'%' {
            if !path.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                || !path.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
            {
                return None;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    let path = percent_encoding::percent_decode(path).collect::<Vec<u8>>();
    if !path.starts_with(b"/") || path.starts_with(b"//") || path.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn uri_lists_preserve_order_and_path_bytes() {
        let data = "# comment\r\n\r\nfile:///tmp/a%20b\r\n\
                    file://localhost/home/用户/📂\n\
                    file:///tmp/ literal spaces \n\
                    file:///tmp/a%0Ab%0Dc%23d%3Fe%25f%2Fg\n\
                    FILE://LOCALHOST/tmp/folder/\n\
                    file:///tmp/a%20b";
        assert_eq!(
            parse_uri_list(data.as_bytes()).unwrap(),
            vec![
                PathBuf::from("/tmp/a b"),
                PathBuf::from("/home/用户/📂"),
                PathBuf::from("/tmp/ literal spaces "),
                PathBuf::from("/tmp/a\nb\rc#d?e%f/g"),
                PathBuf::from("/tmp/folder/"),
                PathBuf::from("/tmp/a b"),
            ]
        );
    }

    #[test]
    fn encoded_line_delimiters_do_not_create_uris_or_comments() {
        assert_eq!(
            parse_uri_list(b"file:///a%0Afile:///b%0D%0A%23comment\nfile:///c").unwrap(),
            vec![
                PathBuf::from("/a\nfile:///b\r\n#comment"),
                PathBuf::from("/c"),
            ]
        );
        assert_eq!(
            parse_uri_list(b"file:///a%250Ab").unwrap(),
            vec![PathBuf::from("/a%0Ab")]
        );
    }

    #[test]
    fn nonlocal_relative_or_malformed_uris_are_ignored() {
        for uri in [
            "",
            "#file:///comment",
            "https://localhost/a",
            "/absolute",
            "relative",
            "file:relative",
            "file:/absolute",
            "file://",
            "file://localhost",
            "file://server/share/a",
            "file://localhost.example/a",
            "file://user@localhost/a",
            "file://localhost:80/a",
            "file://127.0.0.1/a",
            "file://%6cocalhost/a",
            "file:////server/a",
            "file:///a?query",
            "file:///a#fragment",
            "file:///a%",
            "file:///a%0",
            "file:///a%GG",
            "file:///a%0g",
            "file:///a%00b",
            "file:///%2fserver/a",
            " file:///a",
            "file:///a\rb",
            "file:///a\tb",
            "file:///a\0b",
        ] {
            assert!(
                parse_local_uri(uri.as_bytes()).is_none(),
                "accepted {uri:?}"
            );
        }
        assert_eq!(parse_uri_list(b"file:///good\nfile:///bad%\r\nfile://server/no\nfile://localhost/also-good").unwrap(),
            vec![PathBuf::from("/good"), PathBuf::from("/also-good")]);
    }

    #[test]
    fn unix_names_need_not_exist_or_be_utf8() {
        assert_eq!(
            parse_uri_list(b"file:///no-such-directory/../a%FF").unwrap(),
            vec![PathBuf::from(OsString::from_vec(
                b"/no-such-directory/../a\xff".to_vec()
            ))]
        );
        assert_eq!(
            parse_uri_list(b"file:///").unwrap(),
            vec![PathBuf::from("/")]
        );
    }

    #[test]
    fn path_count_limit_is_all_or_nothing() {
        let mut list = String::new();
        for index in 0..MAX_PATHS {
            list.push_str(&format!("file:///path-{index}\n"));
        }
        let paths = parse_uri_list(list.as_bytes()).unwrap();
        assert_eq!(paths.len(), MAX_PATHS);
        assert_eq!(paths.first().unwrap(), &PathBuf::from("/path-0"));
        assert_eq!(
            paths.last().unwrap(),
            &PathBuf::from(format!("/path-{}", MAX_PATHS - 1))
        );
        // Ignored lines generate no events and do not consume the path budget.
        list.push_str("# comment\r\nhttps://example.com/a\nfile:///bad%\n");
        assert_eq!(parse_uri_list(list.as_bytes()).unwrap().len(), MAX_PATHS);
        list.push_str("file:///one-too-many\n");
        assert!(list.len() < MAX_PAYLOAD);
        assert_eq!(
            parse_uri_list(list.as_bytes()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // Duplicated tiny URIs count individually, just like the events emitted.
        assert!(parse_uri_list(&b"file:///\n".repeat(MAX_PATHS + 1)).is_err());
    }

    #[test]
    fn payload_size_limit_is_all_or_nothing() {
        let mut list = b"file:///valid\n#".to_vec();
        list.resize(MAX_PAYLOAD, b'x');
        assert_eq!(
            parse_uri_list(&list).unwrap(),
            vec![PathBuf::from("/valid")]
        );
        list.push(b'x');
        assert_eq!(
            parse_uri_list(&list).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn only_explicit_copy_is_allowed_for_v3() {
        let all = DndAction::Copy | DndAction::Move | DndAction::Ask;
        assert!(copy_allowed(3, true, all, DndAction::Copy));
        for selected in [DndAction::empty(), DndAction::Move, DndAction::Ask, all] {
            assert!(!copy_allowed(3, true, all, selected));
        }
        assert!(!copy_allowed(3, true, DndAction::Move, DndAction::Copy));
        assert!(!copy_allowed(3, false, all, DndAction::Copy));
        for version in [1, 2] {
            assert!(copy_allowed(
                version,
                true,
                DndAction::empty(),
                DndAction::empty()
            ));
            assert!(!copy_allowed(
                version,
                false,
                DndAction::empty(),
                DndAction::empty()
            ));
        }
    }

    #[test]
    fn reads_are_budgeted_and_eof_is_required() {
        let mut reader = Cursor::new(b"12345678");
        let mut payload = Vec::new();
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 8, 3).unwrap(),
            ReadProgress::Pending
        );
        assert_eq!(payload, b"123");
        assert_eq!(reader.position(), 3);
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 8, 5).unwrap(),
            ReadProgress::Pending
        );
        assert_eq!(payload, b"12345678");
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 8, 5).unwrap(),
            ReadProgress::Eof
        );
    }

    #[test]
    fn oversized_payload_never_grows_past_limit() {
        let mut reader = Cursor::new(b"123456789");
        let mut payload = Vec::new();
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 8, 8).unwrap(),
            ReadProgress::Pending
        );
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 8, 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(payload, b"12345678");
    }

    struct ErrorReader {
        kind: io::ErrorKind,
        calls: usize,
    }

    impl Read for ErrorReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            Err(self.kind.into())
        }
    }

    #[test]
    fn interruptions_and_would_block_cannot_spin() {
        let mut reader = ErrorReader {
            kind: io::ErrorKind::Interrupted,
            calls: 0,
        };
        let mut payload = b"partial".to_vec();
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 1024, 1024).unwrap(),
            ReadProgress::Pending
        );
        assert_eq!(reader.calls, MAX_READ_CALLS);
        reader.kind = io::ErrorKind::WouldBlock;
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 1024, 1024).unwrap(),
            ReadProgress::Pending
        );
        assert_eq!(reader.calls, MAX_READ_CALLS + 1);
        assert_eq!(payload, b"partial");
        reader.kind = io::ErrorKind::BrokenPipe;
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 1024, 1024)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn nonblocking_fd_keeps_partial_data_until_eof() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        set_nonblocking(&reader).unwrap();
        let mut payload = Vec::new();
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 64, 64).unwrap(),
            ReadProgress::Pending
        );
        writer.write_all(b"file:///a").unwrap();
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 64, 64).unwrap(),
            ReadProgress::Pending
        );
        assert_eq!(payload, b"file:///a");
        drop(writer);
        assert_eq!(
            read_bounded(&mut reader, &mut payload, 64, 64).unwrap(),
            ReadProgress::Eof
        );
    }

    #[test]
    fn cancellation_removes_sources_and_closes_fd() {
        let event_loop = calloop::EventLoop::<WinitState>::try_new().unwrap();
        let handle = event_loop.handle();
        let (reader, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let pipe = handle
            .insert_source(
                calloop::generic::Generic::new(
                    reader,
                    calloop::Interest::READ,
                    calloop::Mode::Level,
                ),
                |_, _, _| Ok(PostAction::Continue),
            )
            .unwrap();
        let timer = handle
            .insert_source(Timer::from_duration(TRANSFER_TIMEOUT), |_, _, _| {
                TimeoutAction::Drop
            })
            .unwrap();
        let mut transfer = Transfer {
            payload: Vec::new(),
            pipe: Some(pipe),
            timer: Some(timer),
        };
        transfer.cancel(&handle);
        transfer.cancel(&handle); // idempotent, no stale-token reuse
        assert!(handle.disable(&pipe).is_err());
        assert!(handle.disable(&timer).is_err());
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn callback_cleanup_does_not_remove_current_source() {
        let event_loop = calloop::EventLoop::<WinitState>::try_new().unwrap();
        let handle = event_loop.handle();
        for timer_is_current in [false, true] {
            let pipe = handle
                .insert_source(Timer::immediate(), |_, _, _| TimeoutAction::Drop)
                .unwrap();
            let timer = handle
                .insert_source(Timer::immediate(), |_, _, _| TimeoutAction::Drop)
                .unwrap();
            let mut transfer = Transfer {
                payload: Vec::new(),
                pipe: Some(pipe),
                timer: Some(timer),
            };
            let current = if timer_is_current {
                transfer.timer.take().unwrap()
            } else {
                transfer.pipe.take().unwrap()
            };
            let other = if timer_is_current { pipe } else { timer };
            transfer.cancel(&handle);
            assert!(handle.disable(&other).is_err());
            assert!(handle.disable(&current).is_ok());
            handle.remove(current);
        }
    }
}
