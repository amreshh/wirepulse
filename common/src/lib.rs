//! Shared between the backend (native) and the frontend (wasm32).
//!
//! Everything here is plain data plus pure functions, so the exact same
//! aggregation, layout and selector logic runs on both sides and is unit
//! tested once.

pub mod aggregate;
pub mod filter;
pub mod layout;
pub mod model;
pub mod selector;

pub use model::*;
