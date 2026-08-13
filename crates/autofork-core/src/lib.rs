//! autofork-core: the tool-agnostic semantics of autofork.
//!
//! Everything in this crate is synchronous and side-effect-light so the full
//! semantic surface (frontmatter parsing, moment matching, dependency
//! layering, wake-payload building, store invariants) is unit-testable
//! without a daemon or a Claude Code installation.

pub mod config;
pub mod discovery;
pub mod duration;
pub mod frontmatter;
pub mod hooks;
pub mod moments;
pub mod notification;
pub mod project;
pub mod protocol;
pub mod schedule;
pub mod store;
pub mod tags;
pub mod wake;

/// Wire/CLI/daemon protocol version. Bump only on breaking frame changes;
/// the `shutdown` frame shape is frozen forever at proto 1.
pub const PROTO_VERSION: u32 = 1;
