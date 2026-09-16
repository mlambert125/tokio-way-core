//! The seam between mechanism and policy.
//!
//! The compositor mechanism — protocol handling, input delivery, scene
//! building — needs a handful of answers only a window manager can give:
//! where a new window goes, what is stacked over what, who gets focus, what a
//! key is bound to. [`Shell`] is the whole list. A compositor built on this
//! crate is an implementation of this trait; everything else is wiring.
//!
//! The shell lives *inside* [`CompositorState`], as `state.shell`, so that
//! protocol handlers deep in the call tree can reach it without every
//! function signature carrying it. Two calling conventions follow from that:
//!
//! - **Queries** take `&self` and answer from the shell's own model (its
//!   workspaces, its stacking). They are called directly through
//!   `state.shell` while the rest of the state is also borrowed, which is why
//!   they must not need `&CompositorState` — anything they need from the
//!   state is passed in.
//! - **Hooks** take `&mut self` *and* `&mut CompositorState`, and are called
//!   through [`CompositorState::with_shell`], which detaches the shell for
//!   the duration of the call. While a hook runs, `state.shell` is a
//!   [`NoShell`] — so a hook must answer policy questions from its own
//!   fields, never through state methods that consult `state.shell` (such as
//!   [`CompositorState::surface_output`] or the hit-testing helpers). The
//!   mechanism functions a hook is expected to call — `switch_focus`,
//!   `clear_focus`, the grab starters, the protocol senders — do not consult
//!   the shell.

use crate::input::HitResult;
use crate::state::{ClientObjectId, CompositorState};
use std::sync::Arc;
use tokio_way_backends::input::MouseButton;
use tokio_way_backends::monotonic_timestamp::MonotonicTimeStamp;
use tokio_way_backends::outputs::OutputId;
use tokio_way_backends::scene_graph::{ElementTransform, ShaderEffect};

/// How a toplevel should be drawn this frame, over what the client committed.
///
/// The shell's say in *appearance*, where [`Shell::visible_stack`] is its say
/// in existence and order. The identity presentation — full alpha, identity
/// transform, no effect — is the common case and costs nothing: the scene
/// builder emits the window's elements exactly as it always has. Anything
/// else wraps the window's whole surface tree (subsurfaces and popups
/// included) in a [`tokio_way_backends::scene_graph::SceneGroup`], so a fade
/// dims the window as one quad instead of seaming where its elements overlap,
/// and a transform pivots the window as a unit.
///
/// Element-local semantics apply to the group's quad: `transform` pivots are
/// in window-local logical pixels (the window's centre is `width / 2.0,
/// height / 2.0`), and an `effect`'s `uv` runs corner to corner of the
/// window. Animate by returning different values from successive
/// [`Shell::present`] calls, advancing them in [`Shell::frame`] — the damage
/// tracker treats a changed presentation as a changed element on its own.
#[derive(Debug, Clone)]
pub struct SurfacePresentation {
    /// Whole-window opacity, `0.0..=1.0`.
    pub alpha: f32,
    /// A transform over the window's rectangle — rotation, scale, a
    /// perspective flip. Identity for the ordinary case.
    pub transform: ElementTransform,
    /// A fragment effect over the window's pixels, or `None` to draw plain.
    pub effect: Option<Arc<ShaderEffect>>,
}

impl Default for SurfacePresentation {
    fn default() -> Self {
        Self {
            alpha: 1.0,
            transform: ElementTransform::IDENTITY,
            effect: None,
        }
    }
}

impl SurfacePresentation {
    /// Whether this presentation changes nothing — the case the scene
    /// builder answers by not building a group at all, which is what keeps
    /// the identity path (and the backend's fast paths behind it) free.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.alpha >= 1.0 && self.transform == ElementTransform::IDENTITY && self.effect.is_none()
    }
}

/// Window-management policy, answered by the compositor built on this crate.
///
/// Every method has a behavior-free default where one exists, so a shell
/// implements only what it has an opinion about; the queries that decide
/// what is on screen have no useful default and must be provided.
pub trait Shell: Send + 'static {
    /// The shell as `Any`, for tests and tooling that know the concrete type.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    // --- Queries: answered from the shell's own model. ---

    /// Which output the toplevel rooted at `key` is on, if it is placed.
    /// `key` is the toplevel's `wl_surface` key.
    fn toplevel_output(&self, key: ClientObjectId) -> Option<OutputId>;

    /// The toplevels showing on an output, bottom to top. This is the draw
    /// order: the scene builder and the hit tester both walk it.
    fn visible_stack(&self, output: OutputId) -> Vec<ClientObjectId>;

    /// The topmost visible toplevel on an output, if any.
    fn top_visible(&self, output: OutputId) -> Option<ClientObjectId> {
        self.visible_stack(output).last().copied()
    }

    /// Whether the toplevel rooted at `key` is currently shown anywhere.
    /// `key` is a root surface key; subsurfaces are resolved to their root
    /// before the mechanism asks.
    fn is_visible(&self, key: ClientObjectId) -> bool;

    /// Put a toplevel on top of whatever it shares a stack with. Called by
    /// the mechanism when a client action implies a raise — today, from
    /// [`CompositorState::raise_with_children`] for each member of the
    /// family being raised.
    fn raise(&mut self, key: ClientObjectId);

    /// How the toplevel rooted at `key` should be drawn right now — its
    /// fade, its transform, its shader, if the shell has one going. The
    /// default is the identity presentation, which draws the window exactly
    /// as committed and keeps the plain-element path in the scene builder.
    fn present(&self, key: ClientObjectId) -> SurfacePresentation {
        let _ = key;
        SurfacePresentation::default()
    }

    /// Advance animations to `now`, and say whether any is still running.
    ///
    /// Called with the backend's predicted presentation time when an output
    /// asks for a frame — compose for the instant the frame will reach the
    /// screen — and from the housekeeping tick with the current time, which
    /// is what keeps an animation moving under a backend that never asks
    /// (the null backend, an idle output). Returning true marks the state
    /// dirty, so the next publish recomposes with the values
    /// [`Self::present`] now returns; returning false costs nothing.
    fn frame(&mut self, state: &mut CompositorState, now: MonotonicTimeStamp) -> bool {
        let _ = (state, now);
        false
    }

    // --- Lifecycle hooks. ---

    /// A new toplevel exists and wants a place: position it (its surface's
    /// `position` is the shell's to write) and record where it lives. Called
    /// from `create_xdg_toplevel`, before the initial configure goes out, so
    /// the bounds sent with that configure describe the output chosen here.
    fn toplevel_created(&mut self, state: &mut CompositorState, key: ClientObjectId) {
        let _ = (state, key);
    }

    /// A toplevel is gone; forget it. Focus bookkeeping on the state itself
    /// is already done by the caller.
    fn toplevel_destroyed(&mut self, state: &mut CompositorState, key: ClientObjectId) {
        let _ = (state, key);
    }

    /// A client's connection ended and its resources are being torn down;
    /// forget every window of its.
    fn client_removed(&mut self, state: &mut CompositorState, client_id: u32) {
        let _ = (state, client_id);
    }

    /// Focus has been left empty by something the shell did not decide — a
    /// focused client disconnecting. Give focus to whatever should have it.
    fn refocus(&mut self, state: &mut CompositorState) {
        let _ = state;
    }

    /// The set of outputs changed: one appeared, one left, or the backend
    /// re-announced them. Reconcile whatever the shell keeps per output.
    fn outputs_changed(&mut self, state: &mut CompositorState) {
        let _ = state;
    }

    /// One output's geometry changed. A window that fit a moment ago may not
    /// any more; what to do about that is the shell's call.
    fn output_changed(&mut self, state: &mut CompositorState, output: OutputId) {
        let _ = (state, output);
    }

    /// An interactive move carried a toplevel onto another output.
    fn toplevel_dragged_to_output(
        &mut self,
        state: &mut CompositorState,
        key: ClientObjectId,
        output: OutputId,
    ) {
        let _ = (state, key, output);
    }

    /// The housekeeping tick, for policy that has to notice things no event
    /// announces — a window left homeless by an output that vanished.
    /// Return true if anything visible changed.
    fn housekeeping(&mut self, state: &mut CompositorState) -> bool {
        let _ = state;
        false
    }

    // --- Input hooks. ---

    /// A key went down; `state.pressed_keys` and the modifier state already
    /// include it. Return true to consume it — the client never sees a
    /// consumed press, and the mechanism swallows the matching release.
    fn key_pressed(&mut self, state: &mut CompositorState, evdev_key: u32) -> bool {
        let _ = (state, evdev_key);
        false
    }

    /// A button went down over `hit` (hit-tested before the call, while the
    /// shell was still attached) with no grab or drag in progress. Return
    /// true to consume the press — starting a move or resize grab is the
    /// expected reason.
    fn pointer_pressed(
        &mut self,
        state: &mut CompositorState,
        button: MouseButton,
        hit: Option<&HitResult>,
    ) -> bool {
        let _ = (state, button, hit);
        false
    }

    /// A press landed on a window other than the focused one, and no grab,
    /// binding, or popup dismissal claimed it. Raising and focusing — or
    /// deliberately not — happens here.
    fn toplevel_clicked(&mut self, state: &mut CompositorState, hit: &HitResult) {
        let _ = (state, hit);
    }
}

/// The shell that answers nothing: no window is placed, visible, or focused.
///
/// It exists for two reasons: it is what `state.shell` holds while a real
/// shell is detached inside [`CompositorState::with_shell`], and it lets the
/// mechanism be driven bare in tests that never map a window. Its queries
/// return the empty answer rather than panicking because the detached case is
/// reachable by mistake — a hook calling back into a state method that
/// consults the shell — and a wrong answer that shows up as a missing window
/// beats an abort in the middle of the compositor loop. Each such call is
/// logged for exactly that reason.
#[derive(Debug, Default)]
pub struct NoShell;

impl Shell for NoShell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn toplevel_output(&self, key: ClientObjectId) -> Option<OutputId> {
        tracing::debug!("NoShell asked for the output of {key:?}");
        None
    }

    fn visible_stack(&self, _output: OutputId) -> Vec<ClientObjectId> {
        Vec::new()
    }

    fn is_visible(&self, key: ClientObjectId) -> bool {
        tracing::debug!("NoShell asked whether {key:?} is visible");
        false
    }

    fn raise(&mut self, key: ClientObjectId) {
        tracing::debug!("NoShell asked to raise {key:?}");
    }
}
