use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::WlOutput,
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::Sandbox;

/// In the order apps prefer them.
pub const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// By path, never through this process's `WAYLAND_DISPLAY`, which may be the live session's.
fn connect(socket: &Path) -> (Connection, GlobalList, EventQueue<Client>) {
    let stream = UnixStream::connect(socket).unwrap_or_else(|e| panic!("connect to {}: {e}", socket.display()));
    let conn = Connection::from_socket(stream).expect("a Wayland connection");
    let (globals, queue) = registry_queue_init::<Client>(&conn).expect("Wayland globals");
    (conn, globals, queue)
}

#[derive(Default)]
struct Client {
    selection: Option<Option<ExtDataControlOfferV1>>,
    cancelled: bool,
    delay: Duration,
    frame: Frame,
}

#[derive(Default)]
struct Frame {
    buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct OfferTypes(std::sync::Mutex<Vec<String>>);

struct Served(BTreeMap<String, Arc<Vec<u8>>>);

pub struct ClipboardOwner {
    stop: Arc<AtomicBool>,
    replaced: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ClipboardOwner {
    pub fn new(sandbox: &Sandbox, items: &[(&str, &[u8])]) -> Self {
        Self::slow(sandbox, items, Duration::ZERO)
    }

    pub fn slow(sandbox: &Sandbox, items: &[(&str, &[u8])], delay: Duration) -> Self {
        let socket = sandbox.wayland_socket().to_path_buf();
        let served: BTreeMap<String, Arc<Vec<u8>>> =
            items.iter().map(|(m, d)| (m.to_string(), Arc::new(d.to_vec()))).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let replaced = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop2, replaced2) = (stop.clone(), replaced.clone());
        let thread = thread::spawn(move || {
            let (conn, globals, mut queue) = connect(&socket);
            let qh = queue.handle();
            let mut st = Client { delay, ..Client::default() };
            let seat: WlSeat = globals.bind(&qh, 1..=8, ()).expect("wl_seat");
            let manager: ExtDataControlManagerV1 = globals.bind(&qh, 1..=1, ()).expect("ext data control");
            let device = manager.get_data_device(&seat, &qh, ());
            let mimes: Vec<String> = served.keys().cloned().collect();
            let source = manager.create_data_source(&qh, Served(served));
            for m in mimes {
                source.offer(m);
            }
            device.set_selection(Some(&source));
            queue.roundtrip(&mut st).expect("roundtrip");
            let _ = ready_tx.send(());
            while !stop2.load(Ordering::Acquire) {
                dispatch_for(&conn, &mut queue, &mut st, Duration::from_millis(20));
                if st.cancelled {
                    replaced2.store(true, Ordering::Release);
                }
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).expect("the clipboard owner set the selection");
        Self { stop, replaced, thread: Some(thread) }
    }

    pub fn replaced(&self) -> bool {
        self.replaced.load(Ordering::Acquire)
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn dispatch_for(conn: &Connection, queue: &mut EventQueue<Client>, st: &mut Client, timeout: Duration) {
    queue.dispatch_pending(st).expect("dispatch");
    conn.flush().expect("flush");
    let Some(guard) = queue.prepare_read() else { return };
    let fd = guard.connection_fd();
    let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
    let ts = rustix::time::Timespec { tv_sec: 0, tv_nsec: timeout.as_nanos() as i64 };
    if rustix::event::poll(&mut fds, Some(&ts)).unwrap_or(0) > 0 {
        let _ = guard.read();
    }
    queue.dispatch_pending(st).expect("dispatch");
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub mimes: Vec<String>,
    pub data: BTreeMap<String, Vec<u8>>,
}

impl Selection {
    pub fn text(&self) -> Option<String> {
        TEXT_MIMES.iter().find_map(|m| self.data.get(*m)).map(|d| String::from_utf8_lossy(d).into_owned())
    }

    pub fn text_under_all_types(&self) -> bool {
        let first = self.data.get(TEXT_MIMES[0]);
        TEXT_MIMES.iter().all(|m| self.data.get(*m).is_some_and(|d| Some(d) == first))
    }
}

pub fn read_clipboard(sandbox: &Sandbox) -> Option<Selection> {
    let (conn, globals, mut queue) = connect(sandbox.wayland_socket());
    let qh = queue.handle();
    let mut st = Client::default();
    let seat: WlSeat = globals.bind(&qh, 1..=8, ()).expect("wl_seat");
    let manager: ExtDataControlManagerV1 = globals.bind(&qh, 1..=1, ()).expect("ext data control");
    let _device = manager.get_data_device(&seat, &qh, ());
    queue.roundtrip(&mut st).expect("roundtrip");
    let offer = st.selection.take().flatten()?;
    let mimes = offer.data::<OfferTypes>().expect("offer types").0.lock().unwrap().clone();
    let mut data = BTreeMap::new();
    for mime in &mimes {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).expect("pipe");
        offer.receive(mime.clone(), write.as_fd());
        drop(write);
        conn.flush().expect("flush");
        let reader = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::fs::File::from(read).read_to_end(&mut buf);
            buf
        });
        data.insert(mime.clone(), reader.join().expect("reader"));
    }
    offer.destroy();
    Some(Selection { mimes, data })
}

pub mod keys {
    pub const KEY_2: u32 = 3;
    pub const KEY_A: u32 = 30;
    pub const KEY_LEFTMETA: u32 = 125;
}

const HOST_KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "psst-host" {
    minimum = 8;
    maximum = 255;
    <AE02> = 11;
    <AC01> = 38;
    <LWIN> = 133;
};
xkb_types "psst-host" { include "complete" };
xkb_compatibility "psst-host" { include "complete" };
xkb_symbols "psst-host" {
    key <AE02> { [ 2, at ] };
    key <AC01> { [ a, A ] };
    key <LWIN> { [ Super_L ] };
    modifier_map Mod4 { <LWIN> };
};
};
"#;

/// On the host sway, so the nested niri's window gets raw evdev codes and applies niri's own
/// layout, as with a real keyboard.
pub struct HostKeyboard {
    _conn: Connection,
    queue: EventQueue<Client>,
    vk: ZwpVirtualKeyboardV1,
    mods: u32,
    start: std::time::Instant,
}

impl HostKeyboard {
    pub fn new(sandbox: &Sandbox) -> Self {
        let (conn, globals, mut queue) = connect(sandbox.host_socket());
        let qh = queue.handle();
        let seat: WlSeat = globals.bind(&qh, 1..=8, ()).expect("wl_seat");
        let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).expect("virtual keyboard");
        let vk = manager.create_virtual_keyboard(&seat, &qh, ());
        let fd = memfd_create("psst-host-keymap", MemfdFlags::CLOEXEC).expect("memfd");
        let mut file = std::fs::File::from(fd);
        file.write_all(HOST_KEYMAP.as_bytes()).and_then(|_| file.write_all(&[0])).expect("write keymap");
        vk.keymap(1, file.as_fd(), HOST_KEYMAP.len() as u32 + 1);
        queue.roundtrip(&mut Client::default()).expect("roundtrip");
        // A new keyboard makes niri's window bind wl_keyboard again; keys sent before that are lost.
        thread::sleep(Duration::from_millis(300));
        Self { _conn: conn, queue, vk, mods: 0, start: std::time::Instant::now() }
    }

    fn key(&mut self, code: u32, pressed: bool) {
        let t = self.start.elapsed().as_millis() as u32;
        self.vk.key(t, code, pressed as u32);
        if code == keys::KEY_LEFTMETA {
            let mod4 = 1 << 6;
            self.mods = if pressed { self.mods | mod4 } else { self.mods & !mod4 };
            self.vk.modifiers(self.mods, 0, 0, 0);
        }
        self.queue.roundtrip(&mut Client::default()).expect("roundtrip");
    }

    pub fn press(&mut self, code: u32) {
        self.key(code, true);
    }

    pub fn release(&mut self, code: u32) {
        self.key(code, false);
    }

    pub fn tap(&mut self, code: u32) {
        self.press(code);
        self.release(code);
    }
}

impl Drop for HostKeyboard {
    fn drop(&mut self) {
        self.vk.destroy();
        let _ = self.queue.roundtrip(&mut Client::default());
    }
}

#[derive(Debug, Clone)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Screenshot {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        self.rgba[i..i + 4].try_into().expect("four channels")
    }
}

/// niri 26.04's nested (winit) output hands out frames upside down without setting `y_invert`, so
/// they're flipped here. Headless sway captures correctly, so a fix in niri fails the overlay tests.
pub fn screenshot(sandbox: &Sandbox) -> Screenshot {
    let (_conn, globals, mut queue) = connect(sandbox.wayland_socket());
    let qh = queue.handle();
    let mut st = Client::default();
    let output: WlOutput = globals.bind(&qh, 1..=4, ()).expect("wl_output");
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
    let manager: ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ()).expect("wlr screencopy");
    let frame = manager.capture_output(0, &output, &qh, ());
    while st.frame.buffer.is_none() || (frame.version() >= 3 && !st.frame.buffer_done) {
        queue.blocking_dispatch(&mut st).expect("dispatch");
        assert!(!st.frame.failed, "screencopy failed");
    }
    let (format, width, height, stride) = st.frame.buffer.expect("buffer info");
    let size = stride as usize * height as usize;
    let fd: OwnedFd = memfd_create("psst-screenshot", MemfdFlags::CLOEXEC).expect("memfd");
    ftruncate(&fd, size as u64).expect("ftruncate");
    let pool = shm.create_pool(fd.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &qh, ());
    frame.copy(&buffer);
    while !st.frame.ready {
        queue.blocking_dispatch(&mut st).expect("dispatch");
        assert!(!st.frame.failed, "screencopy failed");
    }
    let mut raw = vec![0u8; size];
    let mut done = 0;
    while done < size {
        done += rustix::io::pread(&fd, &mut raw[done..], done as u64).expect("read the screenshot");
    }
    buffer.destroy();
    pool.destroy();
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    let flip = st.frame.y_invert != (sandbox.compositor() == crate::Compositor::NestedNiri);
    for row in 0..height {
        let row = if flip { height - 1 - row } else { row };
        let line = &raw[row as usize * stride as usize..][..width as usize * 4];
        for px in line.as_chunks::<4>().0 {
            // wl_shm formats are little-endian.
            let [r, g, b] = match format {
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => [px[2], px[1], px[0]],
                wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => [px[0], px[1], px[2]],
                other => panic!("unexpected screencopy format {other:?}"),
            };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Screenshot { width, height, rgba }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for Client {
    fn event(
        st: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::Selection { id } => {
                if let Some(Some(old)) = st.selection.replace(id) {
                    old.destroy();
                }
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    event_created_child!(Client, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferTypes::default()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, OfferTypes> for Client {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        types: &OfferTypes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            types.0.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Served> for Client {
    fn event(
        st: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        served: &Served,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                let Some(data) = served.0.get(&mime_type).cloned() else { return };
                let delay = st.delay;
                thread::spawn(move || {
                    thread::sleep(delay);
                    let _ = std::fs::File::from(fd).write_all(&data);
                });
            }
            ext_data_control_source_v1::Event::Cancelled => {
                st.cancelled = true;
                source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Client {
    fn event(
        st: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: wayland_client::WEnum::Value(format),
                width,
                height,
                stride,
            } => st.frame.buffer = Some((format, width, height, stride)),
            zwlr_screencopy_frame_v1::Event::BufferDone => st.frame.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                st.frame.y_invert = matches!(flags, wayland_client::WEnum::Value(f) if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => st.frame.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => st.frame.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Client: ignore WlSeat);
delegate_noop!(Client: ignore WlOutput);
delegate_noop!(Client: ignore WlShm);
delegate_noop!(Client: WlShmPool);
delegate_noop!(Client: ignore WlBuffer);
delegate_noop!(Client: ExtDataControlManagerV1);
delegate_noop!(Client: ZwpVirtualKeyboardManagerV1);
delegate_noop!(Client: ZwpVirtualKeyboardV1);
delegate_noop!(Client: ZwlrScreencopyManagerV1);
