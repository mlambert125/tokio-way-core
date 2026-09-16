#![warn(clippy::pedantic)]

//! The mechanism half of a Wayland compositor.
//!
//! This crate owns everything a compositor must do the same way no matter how
//! it manages windows: speaking the protocol ([`protocol`]), keeping the state
//! the protocol describes ([`state`]), delivering input with correct serials
//! and enter/leave discipline ([`input`]), turning surfaces into a scene the
//! backend can draw ([`scene`]), and the event loop that holds the ordering of
//! all of it together ([`run_compositor`]).
//!
//! What it deliberately does not own is policy: where a window opens, what is
//! stacked over what, who has focus, what a keybinding does. Those questions
//! are asked through the [`Shell`] trait, and a compositor built on this crate
//! is exactly an implementation of that trait plus a `main` that wires the
//! socket and backend crates to [`run_compositor`] — see `way-small` for the
//! reference shell.

// Lint posture: this crate's public surface is large — protocol handlers, state queries,
// input delivery — and most of it was extracted from a binary where these
// lints only apply to the crate root. Annotating every query with
// `#[must_use]` and every fallible sender with an `# Errors` section is a
// documentation pass of its own; until it happens, these stay off rather
// than half-done.
#![allow(
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::new_without_default,
    clippy::result_unit_err
)]

// The test suite migrated from way-small with its `tokio_way_core::` paths
// intact; this alias is what lets those paths resolve from inside the crate.
#[cfg(test)]
extern crate self as tokio_way_core;

pub mod input;
pub mod keys;
pub mod layer;
pub mod protocol;
mod run;
pub mod scene;
pub mod shell;
pub mod state;
#[cfg(test)]
mod tests;

pub use run::{FramePacer, run_compositor};
pub use shell::{NoShell, Shell};

use std::time::Duration;

/// How often the housekeeping arm of the loop runs with no other traffic.
pub(crate) const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(16);

/// The evdev keycode for Escape, which abandons a drag in progress.
pub(crate) const KEY_ESC: u32 = 1;

/// Logical pixels one wheel detent scrolls.
pub const SCROLL_STEP: f64 = 10.0;

/// The narrowest an interactive resize will make a window.
pub const MIN_WINDOW_WIDTH: i32 = 120;
/// The shortest an interactive resize will make a window.
pub const MIN_WINDOW_HEIGHT: i32 = 80;

/// The mechanism's knobs, set once at startup.
///
/// These are the choices the mechanism itself has to know about — how to
/// scroll, what to compile into the keymap, what to clear an output to. They
/// are values rather than [`Shell`] methods because none of them changes per
/// window or per event; the shell's own configuration (keybinds, startup
/// programs, workspace rules) never reaches this crate at all.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Whether touchpad scrolling is "natural" — content follows the fingers.
    pub natural_scrolling: bool,
    /// Multiplier applied to touchpad scroll distance.
    pub scroll_factor: f64,
    /// Override the scale every output is reported at; `None` means whatever
    /// the backend says.
    pub output_scale: Option<f64>,
    /// Whether a client should draw its own window decorations, answered via
    /// `zxdg_decoration_manager_v1`. This crate never draws decorations, so
    /// `false` means borderless, not a compositor-drawn title bar.
    pub client_side_decorations: bool,
    /// Extra xkb options compiled into the keymap, comma-separated as xkb
    /// takes them — `"caps:escape"` being the classic.
    pub xkb_options: Option<String>,
    /// What every output is cleared to before its elements draw.
    pub background_color: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            natural_scrolling: true,
            scroll_factor: 1.0,
            output_scale: None,
            client_side_decorations: true,
            xkb_options: None,
            background_color: 0xff1a_1a2e,
        }
    }
}
