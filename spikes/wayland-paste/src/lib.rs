//! Phase 0 paste prototype for psst (spike, not production code).
//!
//! - [`clipboard`]: the pure clipboard state machine (snapshot, inheritance,
//!   freezing, restore, generations), unit-tested without Wayland.
//! - [`paster`]: the Wayland thread. One connection, data-control
//!   `set_selection` → `wl_display.sync` → optional delay → virtual-keyboard
//!   chord → `wl_display.sync`, plus snapshot reads and restores.
//! - [`dc`]: ext/wlr data-control behind one API.
//! - [`guard`]: refuses to run outside the offscreen sandbox.

pub mod clipboard;
pub mod clock;
pub mod dc;
pub mod guard;
pub mod keymap;
pub mod paster;
