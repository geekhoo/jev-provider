//! Provider-neutral Jev/System One gateway core: the public contract, request
//! validation, prompt and schema preparation, deterministic answer mapping,
//! idempotency bookkeeping, and per-provider request/response mapping.
//!
//! This crate holds no server or async runtime code; the `jev-provider` binary
//! owns HTTP serving, pooled upstream I/O, admission control, and deadlines.

pub mod error;
pub mod idempotency;
pub mod map;
pub mod model;
pub mod prompt;
pub mod provider;
pub mod strict_json;
pub mod validate;
