//! wtype-style, but with the real evdev keycodes and keysyms `[v, V]`, so Ctrl+Shift+V reaches an
//! app the way a real keyboard's would, including apps that look at physical key codes.

use std::io::Write;
use std::os::fd::AsFd;

use rustix::fs::{MemfdFlags, memfd_create};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

use crate::Chord;

/// evdev code (xkb keycode − 8).
const KEY_V: u32 = 47;

/// Core xkb order: Shift, Lock, Control, Mod1..Mod5.
const MOD_SHIFT: u32 = 1 << 0;
const MOD_CTRL: u32 = 1 << 2;

const PASTE_KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "psst" {
    minimum = 8;
    maximum = 255;
    <LCTL> = 37;
    <LFSH> = 50;
    <AB04> = 55;
};
xkb_types "psst" { include "complete" };
xkb_compatibility "psst" { include "complete" };
xkb_symbols "psst" {
    key <AB04> { [ v, V ] };
    key <LCTL> { [ Control_L ] };
    key <LFSH> { [ Shift_L ] };
    modifier_map Control { <LCTL> };
    modifier_map Shift { <LFSH> };
};
};
"#;

pub(crate) fn upload(vk: &ZwpVirtualKeyboardV1) -> std::io::Result<()> {
    let fd = memfd_create("psst-keymap", MemfdFlags::CLOEXEC)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(PASTE_KEYMAP.as_bytes())?;
    file.write_all(&[0])?;
    // Format 1 is xkb_v1; the size includes the NUL terminator.
    vk.keymap(1, file.as_fd(), PASTE_KEYMAP.len() as u32 + 1);
    Ok(())
}

/// No modifier key events, so a physically held Super doesn't reach the app.
pub(crate) fn send_chord(vk: &ZwpVirtualKeyboardV1, chord: Chord, time_ms: u32) {
    let mods = match chord {
        Chord::CtrlV => MOD_CTRL,
        Chord::CtrlShiftV => MOD_CTRL | MOD_SHIFT,
    };
    vk.modifiers(mods, 0, 0, 0);
    vk.key(time_ms, KEY_V, 1);
    vk.key(time_ms, KEY_V, 0);
    vk.modifiers(0, 0, 0, 0);
}
