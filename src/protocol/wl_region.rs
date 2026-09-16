//! `wl_region` protocol handler.
//!
//! A region is a set of rectangles used to describe input and opaque areas
//! of a surface. Clients add/subtract rectangles, then assign the region
//! to a surface via `set_opaque_region` or `set_input_region`.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{CompositorState, RegionOp, RegionRect};
use super::wire_utils::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const ADD: u16 = 1;
const SUBTRACT: u16 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        ADD => handle_add(state, msg),
        SUBTRACT => handle_subtract(state, msg),
        _ => super::unknown_request(state, msg, "wl_region"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let region_id = msg.message.object_id;
    debug!("wl_region.destroy: region_id={}", region_id);
    state.destroy_region(msg.client_id, region_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(region_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

fn handle_add(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    push_rect(state, msg, RegionOp::Add);
}

fn handle_subtract(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    push_rect(state, msg, RegionOp::Subtract);
}

/// Append one rectangle to a region. Adds and subtracts share a single list so
/// that the order the client issued them in is preserved — see `region_contains`.
fn push_rect(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo, op: RegionOp) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y), Some(width), Some(height)) =
        (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        super::malformed_request(state, msg, "wl_region");
        return;
    };
    let region_id = msg.message.object_id;
    if let Some(region) = state.regions.get_mut(&(msg.client_id, region_id)) {
        region.rects.push(RegionRect {
            op,
            x,
            y,
            width,
            height,
        });
    }
}
