//! Rust API client, generated from the committed OpenAPI document (design §3.2).
//!
//! Nothing in here is written by hand. `task recu` exports the spec from the
//! utoipa annotations on the server handlers, lowers it to the OpenAPI 3.0 subset
//! progenitor understands (ADR-0009) and regenerates [`generated`]. The CI
//! `contract` job re-runs that and fails on any diff, so this client cannot drift
//! from the server it talks to.
//!
//! Consumers should depend on `notify-client-core` rather than this crate
//! directly: it adds authentication, SSE resume and the local cache on top.

// The generated module is machine-written: it does not follow the workspace lints,
// and re-litigating that on every regeneration would be noise. It is excluded from
// the coverage gate for the same reason (design §3.2).
#[rustfmt::skip]
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    missing_debug_implementations,
    unreachable_pub,
    dead_code,
    reason = "machine-generated code, regenerated wholesale by `task recu`"
)]
mod generated;

pub use generated::*;
