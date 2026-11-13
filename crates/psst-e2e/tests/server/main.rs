//! End-to-end tests of `psst-server`: the real binary (`psst-server-e2e`) with the scripted fake
//! engine, driven through the client protocol, the admin CLI, HTTP, and `psst dict`.

mod access;
mod browser;
mod dict_cli;
mod dictionary;
mod engine;
mod harness;
mod login;
mod profiles;
mod reload;
mod sessions;
