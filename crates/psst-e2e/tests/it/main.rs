//! The end-to-end tests: the real binaries, the server, and the desktop in nested compositors.
//!
//! One binary for all of them, so the crate's tests compile and link once.

mod binaries;
mod daemon;
mod desktop;
mod server;
mod support;
mod wayland;
