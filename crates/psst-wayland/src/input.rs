//! The menu's keyboard and pointer. Only the menu's surface takes either, so keys are read, and the
//! keymap compiled, only while it's open.

use std::os::fd::OwnedFd;
use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use psst_overlay::Key;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::{
    wl_keyboard::{self, WlKeyboard},
    wl_pointer::{self, WlPointer},
    wl_seat::{self, WlSeat},
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use xkbcommon::xkb;

use crate::thread::State;

const BTN_LEFT: u32 = 0x110;
/// Until the compositor sends its own, in `repeat_info`.
const REPEAT: Repeat = Repeat { rate: 25, delay_ms: 600 };

/// The seat's devices on one connection.
pub(crate) struct Devices {
    cursor_shapes: Option<WpCursorShapeManagerV1>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    cursor: Option<WpCursorShapeDeviceV1>,
}

impl Devices {
    /// The keyboard and pointer come with the seat's capabilities.
    pub(crate) fn bind(globals: &GlobalList, qh: &QueueHandle<State>) -> Self {
        Self { cursor_shapes: globals.bind(qh, 1..=1, ()).ok(), keyboard: None, pointer: None, cursor: None }
    }
}

#[derive(Clone, Copy)]
struct Repeat {
    /// Keys a second; 0 turns repeating off.
    rate: i32,
    delay_ms: i32,
}

pub(crate) struct Keys {
    keymap: Option<(OwnedFd, usize)>,
    xkb: Option<xkb::State>,
    /// As the last `modifiers` event left them, to apply to a keymap compiled later.
    mods: [u32; 4],
    focused: bool,
    pointer: Option<(f64, f64)>,
    repeat: Repeat,
    /// The key held down, and the timer repeating it. A timer stops itself once it's not this one.
    held: Option<(u32, u64)>,
    timers: u64,
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            keymap: None,
            xkb: None,
            mods: [0; 4],
            focused: false,
            pointer: None,
            repeat: REPEAT,
            held: None,
            timers: 0,
        }
    }
}

impl Keys {
    fn xkb(&mut self) -> Option<&mut xkb::State> {
        if self.xkb.is_none()
            && let Some((fd, size)) = self.keymap.take()
        {
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            // SAFETY: the compositor sent the fd as a keymap of `size` bytes, as the protocol requires.
            let keymap = unsafe {
                xkb::Keymap::new_from_fd(&context, fd, size, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)
            };
            match keymap {
                Ok(Some(keymap)) => {
                    let mut state = xkb::State::new(&keymap);
                    let [depressed, latched, locked, group] = self.mods;
                    state.update_mask(depressed, latched, locked, 0, 0, group);
                    self.xkb = Some(state);
                }
                Ok(None) => log::warn!("psst-wayland: the compositor's keymap doesn't compile; the menu ignores keys"),
                Err(err) => log::warn!("psst-wayland: cannot read the compositor's keymap: {err}"),
            }
        }
        self.xkb.as_mut()
    }

    /// `None` for keys the menu has no use for, modifiers among them.
    fn translate(&mut self, key: u32) -> Option<Key> {
        let state = self.xkb()?;
        let code = xkb::Keycode::new(key + 8);
        let sym = state.key_get_one_sym(code);
        let held = |name| state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
        let (ctrl, shift) = (held(xkb::MOD_NAME_CTRL), held(xkb::MOD_NAME_SHIFT));
        Some(match sym {
            xkb::Keysym::Up | xkb::Keysym::KP_Up => Key::Up,
            xkb::Keysym::Down | xkb::Keysym::KP_Down => Key::Down,
            xkb::Keysym::Left | xkb::Keysym::KP_Left => Key::Left,
            xkb::Keysym::Right | xkb::Keysym::KP_Right => Key::Right,
            xkb::Keysym::Home | xkb::Keysym::KP_Home => Key::Home,
            xkb::Keysym::End | xkb::Keysym::KP_End => Key::End,
            xkb::Keysym::Tab if shift => Key::BackTab,
            xkb::Keysym::Tab => Key::Tab,
            xkb::Keysym::ISO_Left_Tab => Key::BackTab,
            xkb::Keysym::Return | xkb::Keysym::KP_Enter => Key::Enter,
            xkb::Keysym::Escape => Key::Escape,
            xkb::Keysym::BackSpace if ctrl => Key::DeleteWord,
            xkb::Keysym::BackSpace => Key::Backspace,
            xkb::Keysym::Delete | xkb::Keysym::KP_Delete => Key::Delete,
            xkb::Keysym::w | xkb::Keysym::W if ctrl => Key::DeleteWord,
            xkb::Keysym::u | xkb::Keysym::U if ctrl => Key::ClearToStart,
            _ if ctrl => return None,
            _ => {
                let text = state.key_get_utf8(code);
                if text.is_empty() || text.chars().any(char::is_control) {
                    return None;
                }
                Key::Text(text)
            }
        })
    }

    fn repeats(&mut self, key: u32) -> bool {
        self.repeat.rate > 0
            && self.xkb().is_some_and(|state| state.get_keymap().key_repeats(xkb::Keycode::new(key + 8)))
    }
}

impl State {
    fn key_down(&mut self, key: u32) {
        let Some(translated) = self.keys.translate(key) else { return };
        if self.keys.repeats(key) {
            self.keys.timers += 1;
            let timer = self.keys.timers;
            self.keys.held = Some((key, timer));
            let Repeat { rate, delay_ms } = self.keys.repeat;
            let every = Duration::from_micros(1_000_000 / rate.max(1) as u64);
            let first = Timer::from_duration(Duration::from_millis(delay_ms.max(0) as u64));
            self.lh
                .insert_source(first, move |_, _, st| {
                    if st.keys.held != Some((key, timer)) {
                        return TimeoutAction::Drop;
                    }
                    if let Some(translated) = st.keys.translate(key) {
                        st.menu_key(translated);
                    }
                    if st.keys.held == Some((key, timer)) {
                        TimeoutAction::ToDuration(every)
                    } else {
                        TimeoutAction::Drop
                    }
                })
                .expect("calloop accepts timers");
        }
        self.menu_key(translated);
    }

    pub(crate) fn release_input(&mut self) {
        self.keys.focused = false;
        self.keys.pointer = None;
        self.keys.held = None;
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(st: &mut Self, seat: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event else { return };
        let Some(wl) = st.wl.as_mut() else { return };
        let devices = &mut wl.devices;
        match (devices.keyboard.take(), caps.contains(wl_seat::Capability::Keyboard)) {
            (None, true) => devices.keyboard = Some(seat.get_keyboard(qh, ())),
            (Some(keyboard), false) => {
                if keyboard.version() >= 3 {
                    keyboard.release();
                }
                st.keys = Keys::default();
            }
            (keyboard, _) => devices.keyboard = keyboard,
        }
        let devices = &mut st.wl.as_mut().expect("checked").devices;
        match (devices.pointer.take(), caps.contains(wl_seat::Capability::Pointer)) {
            (None, true) => {
                let pointer = seat.get_pointer(qh, ());
                devices.cursor = devices.cursor_shapes.as_ref().map(|shapes| shapes.get_pointer(&pointer, qh, ()));
                devices.pointer = Some(pointer);
            }
            (Some(pointer), false) => {
                if let Some(cursor) = devices.cursor.take() {
                    cursor.destroy();
                }
                if pointer.version() >= 3 {
                    pointer.release();
                }
                st.keys.pointer = None;
            }
            (pointer, _) => devices.pointer = pointer,
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(st: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let keys = &mut st.keys;
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                keys.xkb = None;
                keys.keymap = (format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)).then_some((fd, size as usize));
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                // The keys already down (the menu's own hotkey, say) aren't pressed for the menu.
                keys.focused = st.overlay.is_menu(&surface);
            }
            wl_keyboard::Event::Leave { .. } => {
                keys.focused = false;
                keys.held = None;
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                keys.mods = [mods_depressed, mods_latched, mods_locked, group];
                if let Some(state) = &mut keys.xkb {
                    state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key { key, state, .. } => match state {
                WEnum::Value(wl_keyboard::KeyState::Pressed) if keys.focused => st.key_down(key),
                _ => {
                    keys.held.take_if(|(held, _)| *held == key);
                }
            },
            wl_keyboard::Event::RepeatInfo { rate, delay } => keys.repeat = Repeat { rate, delay_ms: delay },
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(st: &mut Self, _: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                if !st.overlay.is_menu(&surface) {
                    return;
                }
                if let Some(cursor) = st.wl.as_ref().and_then(|wl| wl.devices.cursor.as_ref()) {
                    cursor.set_shape(serial, Shape::Default);
                }
                st.keys.pointer = Some((surface_x, surface_y));
                st.menu_hover(surface_x, surface_y);
            }
            wl_pointer::Event::Leave { .. } => st.keys.pointer = None,
            wl_pointer::Event::Motion { surface_x, surface_y, .. } if st.keys.pointer.is_some() => {
                st.keys.pointer = Some((surface_x, surface_y));
                st.menu_hover(surface_x, surface_y);
            }
            wl_pointer::Event::Button {
                button: BTN_LEFT,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                if let Some((x, y)) = st.keys.pointer {
                    st.menu_click(x, y);
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WpCursorShapeManagerV1);
delegate_noop!(State: ignore WpCursorShapeDeviceV1);
