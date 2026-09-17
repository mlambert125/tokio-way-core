//! `wp_viewport` protocol handler.
//!
//! Handles `set_source` (crop rectangle) and `set_destination` (output size)
//! requests. Values are stored as pending state and applied on the next
//! `wl_surface.commit,` matching the double-buffered semantics of the protocol.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::ArgReader;

// wp_viewport request opcodes
const DESTROY: u16 = 0;
const SET_SOURCE: u16 = 1;
const SET_DESTINATION: u16 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        SET_SOURCE => handle_set_source(state, msg),
        SET_DESTINATION => handle_set_destination(state, msg),
        _ => super::reject_unknown_request(state, msg, "wp_viewport"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let viewport_id = msg.message.object_id;
    debug!("wp_viewport.destroy: viewport_id={}", viewport_id);
    state.destroy_viewport(msg.client_id, viewport_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(viewport_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

fn handle_set_source(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // All four args are wl_fixed_t (24.8 fixed-point)
    let (Some(x), Some(y), Some(width), Some(height)) =
        (args.fixed(), args.fixed(), args.fixed(), args.fixed())
    else {
        super::reject_malformed_request(state, msg, "wp_viewport");
        return;
    };

    let viewport_id = msg.message.object_id;
    debug!(
        "wp_viewport.set_source: viewport_id={} src=({}, {}, {}, {})",
        viewport_id, x, y, width, height
    );

    // 1.0 is unset, need to use epsilon compare because of floats
    let unset = |x: f64| (x - 1.0).abs() < f64::EPSILON;

    if let Some(vp) = state.viewports.get_mut(&(msg.client_id, viewport_id)) {
        if unset(x) && unset(y) && unset(width) && unset(height) {
            vp.pending_source = None;
        } else {
            vp.pending_source = Some((x, y, width, height));
        }
    }
}

fn handle_set_destination(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(width), Some(height)) = (args.i32(), args.i32()) else {
        super::reject_malformed_request(state, msg, "wp_viewport");
        return;
    };

    let viewport_id = msg.message.object_id;
    debug!(
        "wp_viewport.set_destination: viewport_id={} dst=({}, {})",
        viewport_id, width, height
    );

    if let Some(vp) = state.viewports.get_mut(&(msg.client_id, viewport_id)) {
        if width == -1 && height == -1 {
            vp.pending_destination = None;
        } else {
            vp.pending_destination = Some((width, height));
        }
    }
}
