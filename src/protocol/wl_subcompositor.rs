//! `wl_subcompositor` protocol handler.
//!
//! The subcompositor global lets clients create subsurfaces — child surfaces
//! positioned relative to a parent and composited together with it.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::ObjectType;
use super::wire_utils::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const GET_SUBSURFACE: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        GET_SUBSURFACE => handle_get_subsurface(state, msg),
        _ => super::reject_unknown_request(state, msg, "wl_subcompositor"),
    }
}

fn handle_get_subsurface(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // get_subsurface args: new_id, object surface, object parent
    let (Some(subsurface_id), Some(surface_id), Some(parent_id)) =
        (args.new_id(), args.u32(), args.u32())
    else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_subcompositor.get_subsurface: malformed args",
        );
        return;
    };

    let client_id = msg.client_id;

    if state
        .cursor_role_surfaces
        .contains(&(client_id, surface_id))
    {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_subcompositor.get_subsurface: surface already has cursor role",
        );
        return;
    }

    if state.dnd_icon_surfaces.contains(&(client_id, surface_id)) {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_subcompositor.get_subsurface: surface already has drag icon role",
        );
        return;
    }

    debug!(
        "wl_subcompositor.get_subsurface: subsurface_id={} surface_id={} parent_id={}",
        subsurface_id, surface_id, parent_id
    );

    // A surface may not end up its own ancestor — see
    // [`CompositorState::is_ancestor`] for why that is fatal rather than
    // merely wrong.
    if state.is_ancestor(client_id, surface_id, parent_id) {
        if let Some(client) = state.clients.get(client_id) {
            // wl_subcompositor.error.bad_parent = 1
            client.send_error(
                msg.message.object_id,
                1,
                "wl_subcompositor.get_subsurface: parent is a descendant of the surface",
            );
        }
        return;
    }

    let Some(client) = state.clients.get(client_id) else {
        return;
    };

    // Register before touching any surface state: a rejected id must not leave
    // a half-built parent-child relationship behind.
    let version = client.version(msg.message.object_id);
    if client
        .register_client_object_with_version(subsurface_id, ObjectType::WlSubsurface, version)
        .is_err()
    {
        return;
    }

    // Set up the parent-child relationship
    if let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id)) {
        surface.parent = Some(parent_id);
        // What tells this apart from an `xdg_popup`, which is parented through
        // the same two fields. Only a subsurface has a commit mode, and a
        // popup put into synchronised mode would never be drawn.
        surface.is_subsurface = true;
        // A subsurface starts synchronised, which the protocol requires: it is
        // created to be part of its parent's next frame rather than to appear
        // on its own before the parent has said where it goes.
        surface.subsurface_sync = true;
    }
    if let Some(parent) = state.surfaces.get_mut(&(client_id, parent_id)) {
        parent.children.push(surface_id);
    }

    // Store the mapping from subsurface object id to the wl_surface id it controls
    state
        .subsurface_map
        .insert((client_id, subsurface_id), surface_id);
}
