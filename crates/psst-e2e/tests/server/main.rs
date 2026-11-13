//! End-to-end tests of `psst-server`: the real binary (`psst-server-e2e`) with the scripted fake
//! engine, driven through the client protocol, the admin CLI and HTTP.

mod access;
mod dictionary;
mod engine;
mod harness;
mod reload;
mod sessions;
