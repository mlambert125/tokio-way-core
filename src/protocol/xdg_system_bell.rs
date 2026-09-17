//! `xdg_system_bell` protocol handler.
//!
//! Enables clients to ring the system bell

use tracing::{debug, info};

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const RING: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            debug!("xdg_system_bell: destroy");
            // The id has to come back, not just the object. `unregister` is
            // what sends `wl_display.delete_id`, and without it the client is
            // never told the id is free — so libwayland, which recycles ids
            // eagerly, hands the same one to the next object and the
            // compositor rejects it as already in use, taking the connection
            // down over a bell.
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        RING => handle_ring(state, msg),
        _ => super::reject_unknown_request(state, msg, "xdg_system_bell_v1"),
    }
}

/// Alert the user that a client wants attention.
///
/// There is no audio in this compositor, so the bell is visual: the output the
/// surface is on flashes briefly. That is the classic answer for a terminal
/// bell with the speaker muted, and it is a real answer rather than a stub —
/// the request does something the user can see.
///
/// The surface argument is nullable, and a null one means the client is not
/// naming a window. That still deserves an alert, so it goes to whichever
/// output the pointer is on.
fn handle_ring(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(surface_id) = args.u32() else {
        super::reject_malformed_request(state, msg, "xdg_system_bell_v1");
        return;
    };

    let output = if surface_id == 0 {
        state.output_for_new_window()
    } else {
        // `visible_on` is what the compositor knows rather than what the client
        // has been told, so this works for a client that never bound
        // `wl_output` — which is most of them.
        state
            .surfaces
            .get(&(msg.client_id, surface_id))
            .and_then(|surface| surface.visible_on.iter().copied().next())
            .or_else(|| state.output_for_new_window())
    };

    let Some(output_id) = output else {
        debug!("xdg_system_bell.ring: no output to flash");
        return;
    };
    info!("xdg_system_bell: ring on {output_id:?}");
    state.ring_bell(output_id);
}
