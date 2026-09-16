//! `wl_touch` protocol handler.
//!
//! Touch is multi-point: several fingers are down at once and every event names
//! which one it is about. A point belongs to the surface it *started* on for
//! its whole life, so a finger dragged off a window keeps reporting to the
//! client that owns it — which is what makes a swipe that leaves the window
//! still reach the thing being swiped.

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::next_serial;
use super::wire_utils::{ArgWriter, build_message};

// Request opcodes
const RELEASE: u16 = 0;

// Event opcodes
pub const DOWN: u16 = 0;
pub const UP: u16 = 1;
pub const MOTION: u16 = 2;
pub const FRAME: u16 = 3;
pub const CANCEL: u16 = 4;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        RELEASE => {
            let touch_id = msg.message.object_id;
            state
                .touches
                .retain(|t| !(t.client_id == msg.client_id && t.object_id == touch_id));
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(touch_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        _ => super::unknown_request(state, msg, "wl_touch"),
    }
}

/// Every `wl_touch` object a client holds.
///
/// Collected rather than borrowed, because sending needs the clients borrowed
/// mutably and the list lives in the same state.
pub fn touches_of(state: &CompositorState, client_id: u32) -> Vec<u32> {
    state
        .touches
        .iter()
        .filter(|t| t.client_id == client_id)
        .map(|t| t.object_id)
        .collect()
}

/// Send `wl_touch.down`: a finger has landed on a surface.
///
/// Wide, because a touch down genuinely carries this much: which finger, when,
/// where, and on what. Bundling them into a struct would only move the same
/// fields somewhere else.
#[allow(clippy::too_many_arguments)]
pub fn send_down(
    state: &mut CompositorState,
    client_id: u32,
    touch_id: u32,
    time_ms: u32,
    surface_id: u32,
    point_id: i32,
    x: f64,
    y: f64,
) {
    let serial = next_serial();
    state.record_input_serial(client_id, serial);
    let args = ArgWriter::new()
        .u32(serial)
        .u32(time_ms)
        .u32(surface_id)
        .i32(point_id)
        .fixed(x)
        .fixed(y)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(touch_id, DOWN, args));
    }
}

/// Send `wl_touch.up`: a finger has been lifted.
pub fn send_up(
    state: &mut CompositorState,
    client_id: u32,
    touch_id: u32,
    time_ms: u32,
    point_id: i32,
) {
    let serial = next_serial();
    state.record_input_serial(client_id, serial);
    let args = ArgWriter::new()
        .u32(serial)
        .u32(time_ms)
        .i32(point_id)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(touch_id, UP, args));
    }
}

/// Send `wl_touch.motion`: a finger already down has moved.
pub fn send_motion(
    state: &mut CompositorState,
    client_id: u32,
    touch_id: u32,
    time_ms: u32,
    point_id: i32,
    x: f64,
    y: f64,
) {
    let args = ArgWriter::new()
        .u32(time_ms)
        .i32(point_id)
        .fixed(x)
        .fixed(y)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(touch_id, MOTION, args));
    }
}

/// Send `wl_touch.frame`: the end of a set of touch events that belong together.
///
/// Several fingers moving at once arrive as separate events, and a client that
/// acted on each in turn would see states that never existed — two fingers
/// mid-pinch, one moved and one not. The frame is what says the picture is now
/// consistent.
pub fn send_frame(state: &mut CompositorState, client_id: u32, touch_id: u32) {
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(touch_id, FRAME, Vec::new()));
    }
}

/// Send `wl_touch.cancel`: the whole sequence is void.
///
/// Not the same as every finger lifting. A client that receives this must undo
/// what the gesture was doing rather than complete it, because something else
/// — a gesture recogniser, or the compositor — has taken the sequence over.
pub fn send_cancel(state: &mut CompositorState, client_id: u32, touch_id: u32) {
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(touch_id, CANCEL, Vec::new()));
    }
}
