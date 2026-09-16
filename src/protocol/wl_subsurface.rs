//! `wl_subsurface` protocol handler.
//!
//! Controls a subsurface's position, z-order, and commit mode relative
//! to its parent surface.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const SET_POSITION: u16 = 1;
const PLACE_ABOVE: u16 = 2;
const PLACE_BELOW: u16 = 3;
const SET_SYNC: u16 = 4;
const SET_DESYNC: u16 = 5;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        SET_POSITION => handle_set_position(state, msg),
        PLACE_ABOVE => handle_place_above(state, msg),
        PLACE_BELOW => handle_place_below(state, msg),
        SET_SYNC => handle_set_sync(state, msg, true),
        SET_DESYNC => handle_set_sync(state, msg, false),
        _ => super::unknown_request(state, msg, "wl_subsurface"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let subsurface_id = msg.message.object_id;
    let client_id = msg.client_id;
    debug!("wl_subsurface.destroy: subsurface_id={}", subsurface_id);

    // Remove from parent's children list
    if let Some(&surface_id) = state.subsurface_map.get(&(client_id, subsurface_id)) {
        if let Some(surface) = state.surfaces.get(&(client_id, surface_id)) {
            let parent_id = surface.parent;
            // Clear parent reference
            if let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id)) {
                surface.parent = None;
                // And the role with it. The `wl_surface` outlives the
                // `wl_subsurface` and goes back to being an ordinary surface,
                // so it must stop being treated as a synchronised one —
                // leaving this set would leave a surface whose every commit
                // goes into a cache that nothing is left to apply, which is a
                // surface that never appears again.
                surface.is_subsurface = false;
                surface.subsurface_sync = false;
            }
            // Remove from parent's children
            if let Some(parent_id) = parent_id
                && let Some(parent) = state.surfaces.get_mut(&(client_id, parent_id))
            {
                parent.children.retain(|&id| id != surface_id);
            }
        }
        state.subsurface_map.remove(&(client_id, subsurface_id));

        // Anything cached while it was synchronised is owed to the screen, the
        // same as on `set_desync`: the client committed it, and there is no
        // longer a parent whose commit could apply it.
        let key = (client_id, surface_id);
        let cached = state
            .surfaces
            .get_mut(&key)
            .and_then(|surface| surface.cached.take())
            .unwrap_or_default();
        super::wl_surface::apply_surface_state(state, key, cached);
        state.dirty = true;
    }

    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(subsurface_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

fn handle_set_position(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y)) = (args.i32(), args.i32()) else {
        super::malformed_request(state, msg, "wl_subsurface");
        return;
    };

    let subsurface_id = msg.message.object_id;
    let client_id = msg.client_id;
    if let Some(&surface_id) = state.subsurface_map.get(&(client_id, subsurface_id))
        && let Some(surface) = state.surfaces.get_mut(&(client_id, surface_id))
    {
        surface.subsurface_position = super::super::state::clamp_surface_offset(x, y);
        state.dirty = true;
    }
}

fn handle_place_above(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(sibling_id) = args.u32() else {
        super::malformed_request(state, msg, "wl_subsurface");
        return;
    };

    let subsurface_id = msg.message.object_id;
    let client_id = msg.client_id;
    let Some(&surface_id) = state.subsurface_map.get(&(client_id, subsurface_id)) else {
        return;
    };
    let Some(surface) = state.surfaces.get(&(client_id, surface_id)) else {
        return;
    };
    let Some(parent_id) = surface.parent else {
        return;
    };

    if let Some(parent) = state.surfaces.get_mut(&(client_id, parent_id)) {
        parent.children.retain(|&id| id != surface_id);
        if let Some(pos) = parent.children.iter().position(|&id| id == sibling_id) {
            parent.children.insert(pos + 1, surface_id);
        } else {
            parent.children.push(surface_id);
        }
        state.dirty = true;
    }
}

fn handle_place_below(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(sibling_id) = args.u32() else {
        super::malformed_request(state, msg, "wl_subsurface");
        return;
    };

    let subsurface_id = msg.message.object_id;
    let client_id = msg.client_id;
    let Some(&surface_id) = state.subsurface_map.get(&(client_id, subsurface_id)) else {
        return;
    };
    let Some(surface) = state.surfaces.get(&(client_id, surface_id)) else {
        return;
    };
    let Some(parent_id) = surface.parent else {
        return;
    };

    if let Some(parent) = state.surfaces.get_mut(&(client_id, parent_id)) {
        parent.children.retain(|&id| id != surface_id);
        if let Some(pos) = parent.children.iter().position(|&id| id == sibling_id) {
            parent.children.insert(pos, surface_id);
        } else {
            parent.children.insert(0, surface_id);
        }
        state.dirty = true;
    }
}

fn handle_set_sync(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo, sync: bool) {
    let subsurface_id = msg.message.object_id;
    let client_id = msg.client_id;
    let Some(&surface_id) = state.subsurface_map.get(&(client_id, subsurface_id)) else {
        return;
    };
    let key = (client_id, surface_id);
    if let Some(surface) = state.surfaces.get_mut(&key) {
        surface.subsurface_sync = sync;
    }

    // Desyncing is not just a flag: whatever was committed while synchronised
    // is owed to the screen now. The protocol puts it as the cached state
    // being applied immediately — and "immediately" is conditional, because a
    // subsurface under a *synchronised* parent stays effectively synchronised
    // however it sets its own mode, and its cache must go on waiting.
    if !sync && !state.is_effectively_synced(key) {
        let cached = state
            .surfaces
            .get_mut(&key)
            .and_then(|surface| surface.cached.take())
            .unwrap_or_default();
        super::wl_surface::apply_surface_state(state, key, cached);
        state.dirty = true;
    }
}
