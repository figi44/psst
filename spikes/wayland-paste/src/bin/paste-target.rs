//! Test client: a window that takes keyboard focus and, on Ctrl+V or
//! Ctrl+Shift+V, reads the regular clipboard through wl_data_device and prints
//! one JSON line per event on stdout:
//!
//! {"ev":"focus"} · {"ev":"keymap","name":..} · {"ev":"mods",..}
//! {"ev":"key","sym":..,"utf8":..,"raw":..,"t":..}
//! {"ev":"paste","chord":"ctrl+v","mods":{..},"mimes":[..],"text":..,"blobs":{mime:{len,fnv}},"t_key":..,"t_done":..}
//!
//! Every offered type is read; text from the first plain-text type, other
//! types as length + FNV-1a hash. Timestamps are CLOCK_MONOTONIC µs.

use std::io::Read;
use std::time::Duration;

use serde_json::{Value, json};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::DataSourceHandler;
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};
use wayland_paste::clipboard::TEXT_MIMES;
use wayland_paste::clock::now_us;

fn emit(v: Value) {
    println!("{v}");
}

fn fnv1a(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

struct Target {
    conn: Connection,
    registry: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    ddm: DataDeviceManagerState,
    data_device: Option<DataDevice>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    window: Window,
    pool: SlotPool,
    configured: bool,
    mods: Modifiers,
    layout: u32,
}

fn main() {
    wayland_paste::guard::assert_sandbox();
    let conn = Connection::connect_to_env().expect("connect");
    let (globals, queue) = registry_queue_init(&conn).expect("registry");
    let qh = queue.handle();
    let mut el: EventLoop<Target> = EventLoop::try_new().expect("event loop");
    WaylandSource::new(conn.clone(), queue)
        .insert(el.handle())
        .expect("wayland source");

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor");
    let xdg = XdgShell::bind(&globals, &qh).expect("xdg_wm_base");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm");
    let surface = compositor.create_surface(&qh);
    let window = xdg.create_window(surface, WindowDecorations::None, &qh);
    window.set_title("psst paste target");
    window.set_app_id("psst-paste-target");
    window.commit();
    let pool = SlotPool::new(64 * 64 * 4, &shm).expect("pool");

    let mut t = Target {
        conn: conn.clone(),
        registry: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        ddm: DataDeviceManagerState::bind(&globals, &qh).expect("wl_data_device_manager"),
        data_device: None,
        keyboard: None,
        window,
        pool,
        configured: false,
        mods: Modifiers::default(),
        layout: 0,
    };
    loop {
        el.dispatch(Duration::from_millis(500), &mut t)
            .expect("dispatch");
    }
}

impl Target {
    fn draw(&mut self, w: u32, h: u32) {
        let stride = w as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            .expect("buffer");
        for px in canvas.as_chunks_mut::<4>().0 {
            *px = 0xff30_6090u32.to_le_bytes();
        }
        let s = self.window.wl_surface();
        s.damage_buffer(0, 0, w as i32, h as i32);
        buffer.attach_to(s).expect("attach");
        s.commit();
    }

    fn on_paste(&mut self, chord: &'static str, t_key: u64) {
        let m = self.mods;
        let mods = json!({"ctrl":m.ctrl,"shift":m.shift,"logo":m.logo,"alt":m.alt});
        let offer = self
            .data_device
            .as_ref()
            .and_then(|d| d.data().selection_offer());
        let Some(offer) = offer else {
            emit(
                json!({"ev":"paste","chord":chord,"mods":mods,"mimes":[],"text":null,"blobs":{},"t_key":t_key,"t_done":now_us()}),
            );
            return;
        };
        let mut mimes: Vec<String> = offer.with_mime_types(|m| m.to_vec());
        mimes.dedup();
        let text_mime = TEXT_MIMES
            .iter()
            .find(|t| mimes.iter().any(|m| m == *t))
            .map(|s| s.to_string());
        let mut pipes = Vec::new();
        for m in &mimes {
            if TEXT_MIMES.contains(&m.as_str()) && Some(m) != text_mime.as_ref() {
                continue;
            }
            match offer.receive(m.clone()) {
                Ok(p) => pipes.push((m.clone(), p)),
                Err(e) => eprintln!("receive {m}: {e}"),
            }
        }
        let _ = self.conn.flush();
        let layout = self.layout;
        std::thread::spawn(move || {
            let handles: Vec<_> = pipes
                .into_iter()
                .map(|(m, mut p)| {
                    std::thread::spawn(move || {
                        let mut buf = Vec::new();
                        let _ = p.read_to_end(&mut buf);
                        (m, buf)
                    })
                })
                .collect();
            let mut text = Value::Null;
            let mut blobs = serde_json::Map::new();
            for h in handles {
                let (m, buf) = h.join().unwrap();
                if Some(&m) == text_mime.as_ref() {
                    text = Value::String(String::from_utf8_lossy(&buf).into_owned());
                } else {
                    blobs.insert(m, json!({"len": buf.len(), "fnv": fnv1a(&buf)}));
                }
            }
            emit(
                json!({"ev":"paste","chord":chord,"mods":mods,"mimes":mimes,"text":text,"blobs":blobs,
                        "layout":layout,"t_key":t_key,"t_done":now_us()}),
            );
        });
    }
}

impl KeyboardHandler for Target {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        emit(json!({"ev":"focus"}));
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        emit(json!({"ev":"leave"}));
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        e: KeyEvent,
    ) {
        let t_key = now_us();
        let m = self.mods;
        emit(
            json!({"ev":"key","sym":format!("{:?}", e.keysym),"utf8":e.utf8,"raw":e.raw_code,
                    "ctrl":m.ctrl,"shift":m.shift,"logo":m.logo,"alt":m.alt,"layout":self.layout,"t":t_key}),
        );
        if m.ctrl && (e.keysym == Keysym::v || e.keysym == Keysym::V) {
            self.on_paste(if m.shift { "ctrl+shift+v" } else { "ctrl+v" }, t_key);
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        m: Modifiers,
        _: RawModifiers,
        layout: u32,
    ) {
        self.mods = m;
        self.layout = layout;
        emit(
            json!({"ev":"mods","ctrl":m.ctrl,"shift":m.shift,"logo":m.logo,"alt":m.alt,"layout":layout}),
        );
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        let s = keymap.as_string();
        let name = s
            .split_once("xkb_keycodes \"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(n, _)| n.to_string())
            .unwrap_or_default();
        emit(json!({"ev":"keymap","name":name,"len":s.len()}));
    }
}

impl SeatHandler for Target {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
        if self.data_device.is_none() {
            self.data_device = Some(self.ddm.get_data_device(qh, &seat));
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(
                self.seat_state
                    .get_keyboard(qh, &seat, None)
                    .expect("keyboard"),
            );
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl WindowHandler for Target {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        std::process::exit(0);
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        c: WindowConfigure,
        _: u32,
    ) {
        let w = c.new_size.0.map_or(320, |v| v.get());
        let h = c.new_size.1.map_or(200, |v| v.get());
        self.draw(w, h);
        if !self.configured {
            self.configured = true;
            emit(json!({"ev":"configured","w":w,"h":h}));
        }
    }
}

impl DataDeviceHandler for Target {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for Target {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for Target {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: String,
        _: WritePipe,
    ) {
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl CompositorHandler for Target {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Target {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for Target {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Target {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(Target);
smithay_client_toolkit::delegate_dispatch2!(Target);
