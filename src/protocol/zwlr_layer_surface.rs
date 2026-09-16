//! `zwlr_layer_surface_v1` protocol handler.
//!
//! One panel, bar, wallpaper or lock screen. The client says which edges to
//! stick to, how big it wants to be, what margins to leave and how much room
//! to reserve; the compositor works out the rectangle and configures the
//! client with it. All of that is double-buffered like any other surface
//! state, so a surface that changes its anchor and its size gets both in one
//! frame.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::layer;
use super::super::state::{Anchor, ClientObjectId, CompositorState, KeyboardInteractivity, Layer};
use super::wire_utils::{ArgReader, ArgWriter, build_message};

// Request opcodes
const SET_SIZE: u16 = 0;
const SET_ANCHOR: u16 = 1;
const SET_EXCLUSIVE_ZONE: u16 = 2;
const SET_MARGIN: u16 = 3;
const SET_KEYBOARD_INTERACTIVITY: u16 = 4;
const GET_POPUP: u16 = 5;
const ACK_CONFIGURE: u16 = 6;
const DESTROY: u16 = 7;
const SET_LAYER: u16 = 8;

// Event opcodes
const CONFIGURE: u16 = 0;
const CLOSED: u16 = 1;

// zwlr_layer_surface_v1.error
const ERROR_INVALID_SIZE: u32 = 1;
const ERROR_INVALID_ANCHOR: u32 = 2;
const ERROR_INVALID_KEYBOARD_INTERACTIVITY: u32 = 3;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        SET_SIZE => set_size(state, msg),
        SET_ANCHOR => set_anchor(state, msg),
        SET_EXCLUSIVE_ZONE => set_exclusive_zone(state, msg),
        SET_MARGIN => set_margin(state, msg),
        SET_KEYBOARD_INTERACTIVITY => set_keyboard_interactivity(state, msg),
        GET_POPUP => get_popup(state, msg),
        ACK_CONFIGURE => ack_configure(state, msg),
        DESTROY => destroy(state, msg),
        SET_LAYER => set_layer(state, msg),
        _ => super::unknown_request(state, msg, "zwlr_layer_surface_v1"),
    }
}

/// Borrow the pending state, or do nothing if the object has gone.
macro_rules! pending {
    ($state:expr, $msg:expr) => {
        match $state
            .layer_surfaces
            .get_mut(&($msg.client_id, $msg.message.object_id))
        {
            Some(layer) => &mut layer.pending,
            None => return,
        }
    };
}

fn set_size(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(width), Some(height)) = (args.u32(), args.u32()) else {
        super::malformed_request(state, msg, "zwlr_layer_surface_v1");
        return;
    };
    // Unsigned on the wire, but the compositor works in `i32` throughout and a
    // size past `i32::MAX` is not a size — it is a number that would wrap the
    // first time it met a coordinate.
    let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height)) else {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_SIZE,
                "zwlr_layer_surface_v1.set_size: that is not a size",
            );
        }
        return;
    };
    pending!(state, msg).size = (width, height);
}

fn set_anchor(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(anchor) = args.u32() else {
        super::malformed_request(state, msg, "zwlr_layer_surface_v1");
        return;
    };
    if anchor & !Anchor::ALL != 0 {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_ANCHOR,
                &format!(
                    "zwlr_layer_surface_v1.set_anchor: {anchor:#x} has bits that are not edges"
                ),
            );
        }
        return;
    }
    pending!(state, msg).anchor = Anchor(anchor);
}

fn set_exclusive_zone(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(zone) = args.i32() else {
        super::malformed_request(state, msg, "zwlr_layer_surface_v1");
        return;
    };
    pending!(state, msg).exclusive_zone = zone;
}

fn set_margin(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(top), Some(right), Some(bottom), Some(left)) =
        (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        super::malformed_request(state, msg, "zwlr_layer_surface_v1");
        return;
    };
    pending!(state, msg).margin = (top, right, bottom, left);
}

fn set_keyboard_interactivity(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(value) = args.u32() else {
        super::malformed_request(state, msg, "zwlr_layer_surface_v1");
        return;
    };
    let Some(interactivity) = KeyboardInteractivity::from_repr(value) else {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_KEYBOARD_INTERACTIVITY,
                &format!("zwlr_layer_surface_v1.set_keyboard_interactivity: {value} is not one"),
            );
        }
        return;
    };
    pending!(state, msg).keyboard_interactivity = interactivity;
}

fn set_layer(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(value) = args.u32() else {
        super::malformed_request(state, msg, "zwlr_layer_surface_v1");
        return;
    };
    let Some(layer) = Layer::from_repr(value) else {
        if let Some(client) = state.clients.get(msg.client_id) {
            // The shell's invalid_layer, which is what the protocol reuses here.
            client.send_error(
                msg.message.object_id,
                1,
                &format!("zwlr_layer_surface_v1.set_layer: {value} is not a layer"),
            );
        }
        return;
    };
    pending!(state, msg).layer = layer;
}

/// `get_popup`: hang an `xdg_popup` off this layer surface.
///
/// How a panel opens a menu. The popup was created against an
/// `xdg_positioner` with no parent of its own, and this is what gives it one —
/// so it is placed and stacked relative to the panel rather than to a window.
fn get_popup(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(popup_id) = args.u32() else {
        super::malformed_request(state, msg, "zwlr_layer_surface_v1");
        return;
    };
    let client_id = msg.client_id;
    let Some(layer_surface) = state
        .layer_surfaces
        .get(&(client_id, msg.message.object_id))
        .map(|layer| layer.wl_surface_id)
    else {
        return;
    };
    let Some(popup_surface) = state
        .xdg_popups
        .get(&(client_id, popup_id))
        .and_then(|popup| state.xdg_surfaces.get(&(client_id, popup.xdg_surface_id)))
        .map(|xdg| xdg.wl_surface_id)
    else {
        return;
    };

    // A popup is adopted once. The protocol says this comes before the popup's
    // initial commit and says nothing about a second one, and linking it twice
    // would put it in the parent's child list twice — drawn twice, hit-tested
    // twice, and destroyed once.
    if state
        .surfaces
        .get(&(client_id, popup_surface))
        .is_none_or(|surface| surface.parent.is_some())
    {
        return;
    }

    // Refused rather than allowed to close a loop, exactly as
    // `xdg_surface.get_popup` is: composing and hit-testing recurse the surface
    // tree, so a cycle here is a blown stack rather than a misplaced menu.
    if state.is_ancestor(client_id, popup_surface, layer_surface) {
        return;
    }
    if let Some(surface) = state.surfaces.get_mut(&(client_id, popup_surface)) {
        surface.parent = Some(layer_surface);
    }
    if let Some(parent) = state.surfaces.get_mut(&(client_id, layer_surface)) {
        parent.children.push(popup_surface);
    }

    // Now that it has a parent it can be placed, which is what `get_popup` on
    // the `xdg_surface` could not do: a popup created with a null parent gets
    // neither position nor configure there, because both are measured from a
    // parent it did not yet have.
    let Some((positioner, xdg_surface_id)) = state
        .xdg_popups
        .get(&(client_id, popup_id))
        .map(|popup| (popup.positioner, popup.xdg_surface_id))
    else {
        return;
    };
    let (x, y, width, height) = state
        .place_popup((client_id, positioner), (client_id, layer_surface))
        .unwrap_or((0, 0, 1, 1));
    if let Some(surface) = state.surfaces.get_mut(&(client_id, popup_surface)) {
        surface.subsurface_position = super::super::state::clamp_surface_offset(x, y);
    }
    if let Some(popup) = state.xdg_popups.get_mut(&(client_id, popup_id)) {
        popup.x = x;
        popup.y = y;
        popup.width = width;
        popup.height = height;
    }
    super::xdg_popup::send_configure(state, client_id, popup_id, x, y, width, height);
    super::xdg_surface::send_configure(state, client_id, xdg_surface_id);
    state.dirty = true;
}

fn ack_configure(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(serial) = args.u32() else {
        super::malformed_request(state, msg, "zwlr_layer_surface_v1");
        return;
    };
    let key = (msg.client_id, msg.message.object_id);
    let Some(layer) = state.layer_surfaces.get_mut(&key) else {
        return;
    };
    // The same three cases `xdg_surface.ack_configure` sorts, and for the same
    // reasons — see there. A serial newer than anything sent is invented; one
    // the compositor has moved past is merely stale.
    if serial > layer.highest_configure {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                key.1,
                0,
                &format!("zwlr_layer_surface_v1.ack_configure: {serial} was never sent"),
            );
        }
        return;
    }
    if let Some(at) = layer
        .pending_configures
        .iter()
        .position(|&pending| pending == serial)
    {
        layer.pending_configures.drain(..=at);
        layer.configured = true;
    }
}

fn destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    debug!("zwlr_layer_surface_v1.destroy: id={}", key.1);
    if let Some(layer) = state.layer_surfaces.remove(&key) {
        debug!(
            "layer surface {:?} on {:?} is gone",
            layer.namespace, layer.output
        );
        state
            .surface_layer
            .remove(&(layer.client_id, layer.wl_surface_id));
        // The surface goes back to having no role and stops being drawn, and
        // the room it reserved is given back — a panel that exits must not
        // leave a strip of the screen no window can use.
        state.dirty = true;
    }
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(key.1);
    }
}

/// Tell a layer surface what size to be.
///
/// Sent when it is first committed and whenever its geometry changes — a
/// different anchor, another panel taking room, the output resizing. An
/// unchanged size sends nothing, because a configure a client has to answer is
/// not free.
pub fn configure(state: &mut CompositorState, key: ClientObjectId) {
    let Some(geometry) = layer::geometry(state, key) else {
        return;
    };
    let size = (geometry.width, geometry.height);
    if let Some(l) = state.layer_surfaces.get(&key) {
        // Where a panel actually landed, which is the one thing that is hard
        // to tell from the outside: a bar in the wrong place looks like the
        // client's doing until the numbers say otherwise.
        debug!(
            "layer {:?} anchor={:#x} zone={} margin={:?} placed at ({},{}) {}x{}",
            l.namespace,
            l.current.anchor.0,
            l.current.exclusive_zone,
            l.current.margin,
            geometry.x,
            geometry.y,
            geometry.width,
            geometry.height,
        );
    }
    let Some(wl_surface) = state.layer_surfaces.get(&key).map(|l| l.wl_surface_id) else {
        return;
    };
    let Some(layer) = state.layer_surfaces.get_mut(&key) else {
        return;
    };
    // Written onto the `wl_surface` as well, because everything that asks
    // where a surface is — popup placement, `global_position_of`, the walks
    // over a surface tree — reads `position`. A layer surface that left it at
    // the origin would have its menus placed at the top-left of the screen
    // rather than beside the panel they came from.
    let placed = (geometry.x, geometry.y);
    let unchanged = layer.configured_size == Some(size);
    layer.configured_size = Some(size);
    if let Some(surface) = state.surfaces.get_mut(&(key.0, wl_surface)) {
        surface.position = placed;
    }
    // A configure the client has to answer is not free, so an unchanged size
    // sends none — the position above is still refreshed, because a panel can
    // move without resizing when the one beside it changes.
    if unchanged {
        return;
    }
    let Some(layer) = state.layer_surfaces.get_mut(&key) else {
        return;
    };

    let serial = super::next_serial();
    layer.pending_configures.push_back(serial);
    layer.highest_configure = serial;

    let args = ArgWriter::new()
        .u32(serial)
        .u32(size.0.unsigned_abs())
        .u32(size.1.unsigned_abs())
        .build();
    if let Some(client) = state.clients.get(key.0) {
        let _ = client.send(build_message(key.1, CONFIGURE, args));
    }
}

/// Tell a layer surface it is going away, and that it should destroy itself.
///
/// The compositor's half of a surface whose output has been unplugged: the
/// client is told once and is expected to clean up, rather than being left
/// with a panel anchored to a display that is gone.
pub fn send_closed(state: &mut CompositorState, key: ClientObjectId) {
    if let Some(client) = state.clients.get(key.0) {
        let _ = client.send(build_message(key.1, CLOSED, Vec::new()));
    }
}
