//! `xdg_popup` protocol handler.
//!
//! A popup is a short-lived surface (menu, tooltip, combo-box) positioned
//! relative to a parent `xdg_surface` using an `xdg_positioner`. The compositor
//! sends configure events with the computed position and size.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::{ArgReader, ArgWriter, build_message};

// Request opcodes
const DESTROY: u16 = 0;
const GRAB: u16 = 1;
const REPOSITION: u16 = 2;

// Event opcodes
const CONFIGURE: u16 = 0;
const POPUP_DONE: u16 = 1;
const REPOSITIONED: u16 = 2;

/// The version at which `reposition` and `repositioned` appear.
const REPOSITION_SINCE: u32 = 3;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        GRAB => handle_grab(state, msg),
        REPOSITION => handle_reposition(state, msg),
        _ => super::reject_unknown_request(state, msg, "xdg_popup"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let popup_id = msg.message.object_id;
    let client_id = msg.client_id;
    debug!("xdg_popup.destroy: popup_id={}", popup_id);

    // Remove the popup's wl_surface from its parent's children list
    if let Some(popup) = state.xdg_popups.get(&(client_id, popup_id)) {
        let xdg_surface_id = popup.xdg_surface_id;
        if let Some(xdg_surface) = state.xdg_surfaces.get(&(client_id, xdg_surface_id)) {
            let wl_surface_id = xdg_surface.wl_surface_id;
            if let Some(surface) = state.surfaces.get(&(client_id, wl_surface_id)) {
                let parent_id = surface.parent;
                if let Some(parent_id) = parent_id
                    && let Some(parent) = state.surfaces.get_mut(&(client_id, parent_id))
                {
                    parent.children.retain(|&id| id != wl_surface_id);
                }
            }
            // Clear the parent link
            if let Some(surface) = state.surfaces.get_mut(&(client_id, wl_surface_id)) {
                surface.parent = None;
            }
        }
    }

    state.grabbed_popups.retain(|k| *k != (client_id, popup_id));
    state.destroy_xdg_popup(client_id, popup_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(popup_id);
    }
}

fn handle_grab(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let popup_id = msg.message.object_id;
    let client_id = msg.client_id;
    debug!("xdg_popup.grab: popup_id={}", popup_id);
    state.grabbed_popups.push((client_id, popup_id));
}

/// Send `xdg_popup.configure` event with the computed position and size.
pub fn send_configure(
    state: &mut CompositorState,
    client_id: u32,
    popup_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) {
    let args = ArgWriter::new()
        .i32(x)
        .i32(y)
        .i32(width)
        .i32(height)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(popup_id, CONFIGURE, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `xdg_popup.popup_done` to dismiss the popup.
#[allow(dead_code)]
pub fn send_popup_done(state: &mut CompositorState, client_id: u32, popup_id: u32) {
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(popup_id, POPUP_DONE, Vec::new()));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Move a popup that is already on screen to wherever a new positioner puts it.
///
/// The token is the client's, and comes back untouched in `repositioned`: it is
/// how a client that has asked more than once tells which answer belongs to
/// which request. That event goes first, then the new geometry, then the
/// `xdg_surface.configure` that makes the pair take effect — a client acting on
/// the geometry before it knows which request produced it would be guessing.
fn handle_reposition(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(positioner_id), Some(token)) = (args.u32(), args.u32()) else {
        super::reject_malformed_request(state, msg, "xdg_popup");
        return;
    };

    let client_id = msg.client_id;
    let popup_id = msg.message.object_id;
    let key = (client_id, popup_id);

    let Some(popup) = state.xdg_popups.get(&key) else {
        return;
    };
    let (xdg_surface_id, parent_xdg_surface_id) =
        (popup.xdg_surface_id, popup.parent_xdg_surface_id);

    let parent_surface = state
        .xdg_surfaces
        .get(&(client_id, parent_xdg_surface_id))
        .map(|xdg| (client_id, xdg.wl_surface_id));
    let Some((x, y, width, height)) =
        parent_surface.and_then(|parent| state.place_popup((client_id, positioner_id), parent))
    else {
        debug!("xdg_popup.reposition: {positioner_id} is not a positioner of this client");
        return;
    };

    debug!("xdg_popup.reposition: {key:?} -> ({x}, {y}) {width}x{height} token={token}");
    if let Some(popup) = state.xdg_popups.get_mut(&key) {
        popup.x = x;
        popup.y = y;
        popup.width = width;
        popup.height = height;
    }
    // The popup's `wl_surface` hangs off its parent as a child, so its position
    // is the offset within that parent rather than anything global.
    if let Some(wl_surface_id) = state
        .xdg_surfaces
        .get(&(client_id, xdg_surface_id))
        .map(|xdg| xdg.wl_surface_id)
        && let Some(surface) = state.surfaces.get_mut(&(client_id, wl_surface_id))
    {
        surface.subsurface_position = super::super::state::clamp_surface_offset(x, y);
    }

    send_repositioned(state, client_id, popup_id, token);
    send_configure(state, client_id, popup_id, x, y, width, height);

    super::xdg_surface::send_configure(state, client_id, xdg_surface_id);
    state.dirty = true;
}

/// Send `xdg_popup.repositioned`, naming which reposition request this answers.
fn send_repositioned(state: &mut CompositorState, client_id: u32, popup_id: u32, token: u32) {
    let args = ArgWriter::new().u32(token).build();
    if let Some(client) = state.clients.get(client_id)
        && client.version(popup_id) >= REPOSITION_SINCE
    {
        let _ = client.send(build_message(popup_id, REPOSITIONED, args));
    }
}
