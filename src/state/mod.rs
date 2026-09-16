//! State the compositor keeps: what it shares across every client
//! ([`CompositorState`]) and what it keeps about one connection
//! ([`ClientState`]).
//!
//! Re-exported flatly so callers write `state::CompositorState` and
//! `state::ClientState` without caring which file actually defines them.

mod client_state;
mod compositor_state;

pub use client_state::*;
pub use compositor_state::*;
