//! psst's own small virtual-keyboard keymap, uploaded once per connection.
//!
//! Like wtype's, but keycodes are the real evdev ones (KEY_V, KEY_LEFTCTRL,
//! KEY_LEFTSHIFT), so apps that look at physical key codes (Chromium's
//! `KeyboardEvent.code`) see the same key a real keyboard would send.

use std::io::Write;
use std::os::fd::AsFd;

use rustix::fs::{MemfdFlags, memfd_create};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

/// evdev codes as sent in `zwp_virtual_keyboard_v1.key` (xkb keycode − 8).
pub const KEY_V: u32 = 47;
pub const KEY_2: u32 = 3;
pub const KEY_A: u32 = 30;
pub const KEY_LEFTMETA: u32 = 125;

/// Core xkb modifier masks (fixed order: Shift, Lock, Control, Mod1..Mod5).
pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CTRL: u32 = 1 << 2;

/// The default: wtype-style, with `include "complete"` (what localwhisper's
/// wtype sends today, proven in the owner's apps).
pub const PASTE_KEYMAP: &str = r#"xkb_keymap {
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

/// Self-contained alternative (no includes; `paste-spike bench --keymap
/// minimal`). Measured: no difference in the app's handling of the keymap
/// switch, so the wtype-style keymap stays the default.
pub const PASTE_KEYMAP_MINIMAL: &str = r#"xkb_keymap {
xkb_keycodes "psst" {
    minimum = 8;
    maximum = 255;
    <LCTL> = 37;
    <LFSH> = 50;
    <AB04> = 55;
};
xkb_types "psst" {
    type "ONE_LEVEL" {
        modifiers = none;
        level_name[Level1] = "Any";
    };
    type "ALPHABETIC" {
        modifiers = Shift+Lock;
        map[Shift] = Level2;
        map[Lock] = Level2;
        level_name[Level1] = "Base";
        level_name[Level2] = "Caps";
    };
};
xkb_compatibility "psst" {
    interpret Shift_L { action = SetMods(modifiers=Shift); };
    interpret Control_L { action = SetMods(modifiers=Control); };
};
xkb_symbols "psst" {
    key <AB04> { type = "ALPHABETIC", [ v, V ] };
    key <LCTL> { type = "ONE_LEVEL", [ Control_L ] };
    key <LFSH> { type = "ONE_LEVEL", [ Shift_L ] };
    modifier_map Control { <LCTL> };
    modifier_map Shift { <LFSH> };
};
};
"#;

/// For simulating physical keys on the sandbox's headless host: the nested
/// niri reads raw evdev codes and applies its own layout to them.
pub const HOST_KEYMAP: &str = r#"xkb_keymap {
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

pub fn upload(vk: &ZwpVirtualKeyboardV1, keymap: &str) -> std::io::Result<()> {
    let fd = memfd_create("psst-keymap", MemfdFlags::CLOEXEC)?;
    let mut f = std::fs::File::from(fd);
    f.write_all(keymap.as_bytes())?;
    f.write_all(&[0])?;
    // format 1 = xkb_v1; size includes the NUL terminator.
    vk.keymap(1, f.as_fd(), keymap.len() as u32 + 1);
    Ok(())
}
