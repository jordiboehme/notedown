//! All integration tests of this crate in one test binary: one link step instead of one per
//! file, which keeps `cargo test` fast and the target folder small.

mod api;
mod auth;
mod diag;
mod fake_device;
mod files;
mod fsutil;
mod import;
mod journal;
mod mutate;
mod pages;
mod serve;
mod socket;
mod store;
mod upload;
