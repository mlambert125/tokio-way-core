//! `wp_fractional_scale_manager_v1` protocol handler.
//!
//! Hands out one [`super::wp_fractional_scale`] object per surface. A second
//! one for the same surface is an error, which is the only rule here.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{CompositorState, FractionalScaleBinding};
use super::ObjectType;
use super::wire_utils::ArgReader;
use super::wp_fractional_scale;

pub const INTERFACE: &str = "wp_fractional_scale_manager_v1";
/// Version 1, which is every version this protocol has.
pub const VERSION: u32 = 1;

// Request opcodes. `destroy` leads here, unlike most managers.
const DESTROY: u16 = 0;
const GET_FRACTIONAL_SCALE: u16 = 1;

/// `wp_fractional_scale_manager_v1.error.fractional_scale_exists`.
const ERROR_FRACTIONAL_SCALE_EXISTS: u32 = 0;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            // The objects it made outlive it.
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        GET_FRACTIONAL_SCALE => handle_get_fractional_scale(state, msg),
        _ => super::unknown_request(state, msg, INTERFACE),
    }
}

fn handle_get_fractional_scale(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // get_fractional_scale args: new_id, object surface
    let (Some(object_id), Some(surface_id)) = (args.new_id(), args.u32()) else {
        super::malformed_request(state, msg, INTERFACE);
        return;
    };

    // One per surface, and the protocol makes a second one fatal. Checked
    // against what this client already holds rather than against the surface
    // alone: the surface is this client's either way, since an id names an
    // object only within the connection that made it.
    let taken = state
        .fractional_scales
        .iter()
        .any(|(&(cid, _), binding)| cid == msg.client_id && binding.surface_id == surface_id);
    if taken {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_FRACTIONAL_SCALE_EXISTS,
                "wp_fractional_scale_manager_v1.get_fractional_scale: this surface already has one",
            );
        }
        return;
    }

    let surface = (msg.client_id, surface_id);
    if !state.surfaces.contains_key(&surface) {
        debug!("{INTERFACE}.get_fractional_scale: {surface_id} is not a surface of this client");
        return;
    }

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    if client
        .register(object_id, ObjectType::WpFractionalScale)
        .is_err()
    {
        return;
    }

    let object = (msg.client_id, object_id);
    state.fractional_scales.insert(
        object,
        FractionalScaleBinding {
            surface_id,
            sent: None,
        },
    );

    // At once, rather than waiting for the per-frame pass. A client asks for
    // this before its first commit, precisely so that the first frame it draws
    // is the right size — and at that point the surface is on no output yet, so
    // the answer comes from where the window would open. Waiting would mean
    // every client's first frame was drawn at 1× and then thrown away.
    let scale = state.preferred_scale(surface).as_120ths();
    debug!("{INTERFACE}.get_fractional_scale: surface={surface_id} scale={scale}/120");
    wp_fractional_scale::send_preferred_scale(state, object, scale);
    if let Some(binding) = state.fractional_scales.get_mut(&object) {
        binding.sent = Some(scale);
    }
}
