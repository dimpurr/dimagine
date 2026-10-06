//! Page handlers.
//!
//! One module per page, plus the JSON endpoints agents use. The route table in
//! `lib.rs` wires them up; nothing here registers a route itself.

pub mod collections;
pub mod folders;
pub mod image;
pub mod library;
pub mod search;
