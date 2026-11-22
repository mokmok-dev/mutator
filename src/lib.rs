//! Diff-driven mutation testing for Rust on top of `cargo-mutants`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::float_cmp
    )
)]

pub mod accept;
pub mod cache;
pub mod cli;
pub mod diff;
pub mod exec;
pub mod mutant;
pub mod report;
