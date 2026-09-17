//! `wl_callback` protocol handler.
//!
//! Callbacks are one-shot objects created by the compositor (e.g. for
//! `wl_display.sync` or `wl_surface.frame).` The interface has no requests at
//! all, so anything arriving here is a protocol error.

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    super::reject_unknown_request(state, msg, "wl_callback");
}
