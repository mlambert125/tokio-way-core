//! `wl_compositor` protocol handler.
//!
//! The compositor global allows clients to create surfaces and regions.
//! Surfaces are the fundamental drawing primitive in Wayland — clients
//! attach buffers to them and commit to make content visible.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::ObjectType;
use super::wire_utils::ArgReader;

// Request opcodes
const CREATE_SURFACE: u16 = 0;
const CREATE_REGION: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        CREATE_SURFACE => handle_create_surface(state, msg),
        CREATE_REGION => handle_create_region(state, msg),
        _ => super::unknown_request(state, msg, "wl_compositor"),
    }
}

fn handle_create_surface(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let Some(surface_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_compositor.create_surface: malformed args",
        );
        return;
    };

    debug!("wl_compositor.create_surface: surface_id={}", surface_id);

    let version = client.version(msg.message.object_id);
    if client
        .register_with_version(surface_id, ObjectType::WlSurface, version)
        .is_err()
    {
        return;
    }
    state.create_surface(msg.client_id, surface_id);
}

fn handle_create_region(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let Some(region_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_compositor.create_region: malformed args",
        );
        return;
    };

    debug!("wl_compositor.create_region: region_id={}", region_id);

    let version = client.version(msg.message.object_id);
    if client
        .register_with_version(region_id, ObjectType::WlRegion, version)
        .is_err()
    {
        return;
    }
    state.create_region(msg.client_id, region_id);
}
