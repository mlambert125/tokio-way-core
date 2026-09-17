//! `wp_fractional_scale_v1` protocol handler.
//!
//! One object per surface, carrying one event: the scale the compositor would
//! like that surface drawn at, in 120ths.
//!
//! It exists because `wl_output.scale` is a whole number and displays are not.
//! A client on a 1.5× display told "scale 2" draws twice as large and is scaled
//! down, which is blurry and wasteful; told "scale 1" it draws too small and is
//! scaled up, which is worse. This is the only way for the compositor to say
//! 1.5 — `wl_surface.preferred_buffer_scale` is also whole numbers, and arrived
//! in `wl_compositor` version 6, which is past the version advertised here.
//!
//! The compositor is already exact about this internally: an output's scale is
//! kept as a count of 120ths for precisely this protocol's sake, so nothing has
//! to be rounded on the way out.

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState};
use super::wire_utils::{ArgWriter, build_message};

pub const INTERFACE: &str = "wp_fractional_scale_v1";

// Request opcodes
const DESTROY: u16 = 0;

// Event opcodes
pub const PREFERRED_SCALE: u16 = 0;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            let key = (msg.client_id, msg.message.object_id);
            state.fractional_scales.remove(&key);
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        _ => super::reject_unknown_request(state, msg, INTERFACE),
    }
}

/// Send `wp_fractional_scale_v1.preferred_scale`, in 120ths of one.
pub fn send_preferred_scale(
    state: &mut CompositorState,
    object: ClientObjectId,
    scale_120ths: u32,
) {
    let args = ArgWriter::new().u32(scale_120ths).build();
    if let Some(client) = state.clients.get(object.0) {
        let _ = client.send(build_message(object.1, PREFERRED_SCALE, args));
    }
}
