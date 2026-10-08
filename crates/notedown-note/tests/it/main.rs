//! All integration tests of this crate in one test binary: one link step instead of one per
//! file, which keeps `cargo test` fast and the target folder small.

mod pdf;
mod render;
mod strokes;
mod structure;
mod textboxes;
