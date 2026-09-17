//! `wp_viewporter` protocol handler.
//!
//! Allows clients to crop (`set_source`) and scale (`set_destination`) surface
//! content. The `wp_viewporter` global creates `wp_viewport` objects bound to
//! a specific `wl_surface`. Source and destination are double-buffered and
//! applied on `wl_surface.commit`.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::ObjectType;
use super::wire_utils::ArgReader;

// wp_viewporter request opcodes
const DESTROY: u16 = 0;
const GET_VIEWPORT: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        GET_VIEWPORT => handle_get_viewport(state, msg),
        _ => super::reject_unknown_request(state, msg, "wp_viewporter"),
    }
}

fn handle_get_viewport(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let (Some(viewport_id), Some(surface_id)) = (args.new_id(), args.u32()) else {
        super::reject_malformed_request(state, msg, "wp_viewporter");
        return;
    };

    debug!(
        "wp_viewporter.get_viewport: viewport_id={} surface_id={}",
        viewport_id, surface_id
    );

    // Check that the surface doesn't already have a viewport
    if state
        .surface_viewport
        .contains_key(&(msg.client_id, surface_id))
    {
        client.send_error(
            msg.message.object_id,
            0, // error::viewport_exists
            "surface already has a viewport",
        );
        return;
    }

    if client
        .register_client_object(viewport_id, ObjectType::WpViewport)
        .is_err()
    {
        return;
    }

    state.create_viewport(msg.client_id, viewport_id, surface_id);
}
