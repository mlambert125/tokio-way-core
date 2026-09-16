//! `xdg_surface` protocol handler.
//!
//! An `xdg_surface` wraps a `wl_surface` and adds window-management semantics.
//! Clients assign a role (toplevel or popup) and must ack configure events
//! before committing content.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState};
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};

// Request opcodes
const DESTROY: u16 = 0;
const GET_TOPLEVEL: u16 = 1;
const GET_POPUP: u16 = 2;
const SET_WINDOW_GEOMETRY: u16 = 3;
const ACK_CONFIGURE: u16 = 4;

// Event opcodes
pub const CONFIGURE: u16 = 0;

/// `xdg_wm_base.error.invalid_popup_parent`. Defined on `xdg_wm_base` rather
/// than on `xdg_surface`, but raised from here because this is the only
/// request that can name a popup's parent.
const ERROR_INVALID_POPUP_PARENT: u32 = 3;

// xdg_surface.error
/// A buffer was committed before any configure was acknowledged.
const ERROR_UNCONFIGURED_BUFFER: u32 = 3;
/// The serial acknowledged names a configure that was never sent.
const ERROR_INVALID_SERIAL: u32 = 4;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        GET_TOPLEVEL => handle_get_toplevel(state, msg),
        GET_POPUP => handle_get_popup(state, msg),
        SET_WINDOW_GEOMETRY => handle_set_window_geometry(state, msg),
        ACK_CONFIGURE => handle_ack_configure(state, msg),
        _ => super::unknown_request(state, msg, "xdg_surface"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let xdg_surface_id = msg.message.object_id;
    debug!("xdg_surface.destroy: xdg_surface_id={}", xdg_surface_id);
    state.destroy_xdg_surface(msg.client_id, xdg_surface_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(xdg_surface_id);
    }
}

fn handle_get_toplevel(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    let Some(toplevel_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "xdg_surface.get_toplevel: malformed args",
        );
        return;
    };

    let xdg_surface_id = msg.message.object_id;
    debug!(
        "xdg_surface.get_toplevel: toplevel_id={} xdg_surface_id={}",
        toplevel_id, xdg_surface_id
    );

    let client_id = msg.client_id;
    // The xdg_surface's version, carried down. Without it the toplevel sits at
    // version 1 and every version-gated event on it — `wm_capabilities` and
    // `configure_bounds` — is silently suppressed.
    let version = client.version(xdg_surface_id);
    if client
        .register_with_version(toplevel_id, ObjectType::XdgToplevel, version)
        .is_err()
    {
        return;
    }
    state.create_xdg_toplevel(client_id, toplevel_id, xdg_surface_id);

    // Before the first configure: the client decides what chrome to draw off the
    // back of it, and a configure it has already acted on is too late.
    super::xdg_toplevel::send_wm_capabilities(state, client_id, toplevel_id);

    // The initial configure. A zero size leaves the client to pick its own,
    // which is what it is for on a window that has never been mapped; the
    // bounds that go out with it say how much room there is to pick within.
    super::xdg_toplevel::configure(state, (client_id, toplevel_id), 0, 0);

    // Take focus on the new toplevel
    let wl_surface_id = state
        .xdg_surfaces
        .get(&(client_id, xdg_surface_id))
        .map(|s| s.wl_surface_id);
    if let Some(wl_surface_id) = wl_surface_id {
        crate::input::switch_focus(state, (client_id, wl_surface_id));
    }
}

fn handle_get_popup(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    // get_popup args: new_id popup, object parent (xdg_surface), object positioner
    let (Some(popup_id), Some(parent_xdg_surface_id), Some(positioner_id)) =
        (args.new_id(), args.u32(), args.u32())
    else {
        client.send_error(
            msg.message.object_id,
            0,
            "xdg_surface.get_popup: malformed args",
        );
        return;
    };

    let xdg_surface_id = msg.message.object_id;
    let client_id = msg.client_id;
    debug!(
        "xdg_surface.get_popup: popup_id={} parent={} positioner={}",
        popup_id, parent_xdg_surface_id, positioner_id
    );

    // The two `wl_surface`s the popup is about to be linked between. Both are
    // needed before anything is built, because whether the link is legal is
    // decided from them.
    let popup_wl_surface = state
        .xdg_surfaces
        .get(&(client_id, xdg_surface_id))
        .map(|s| s.wl_surface_id);
    let parent_wl_surface = state
        .xdg_surfaces
        .get(&(client_id, parent_xdg_surface_id))
        .map(|s| s.wl_surface_id);

    // A null parent is legal, and is how a layer surface's popup is made. The
    // layer shell says so outright: the popup "should have been created via
    // xdg_surface::get_popup with the parent set to NULL", and
    // `zwlr_layer_surface_v1.get_popup` is what then gives it one. So this is
    // not an unparented popup, it is a popup whose parent has not been named
    // yet — it gets no placement and no configure here, because both of those
    // are measured from a parent it does not have.
    //
    // A parent that was *named* and does not exist is still an error, as is
    // one that is the popup itself or below it: those would put a surface in
    // its own ancestry, which composing and hit-testing recurse straight into
    // — see [`CompositorState::is_ancestor`].
    //
    // Refused before the popup object is registered, so a rejected request
    // leaves no half-built popup and no half-built parent link behind.
    let parented = match (popup_wl_surface, parent_wl_surface) {
        (Some(_), None) if parent_xdg_surface_id == 0 => None,
        (Some(popup_wl), Some(parent_wl)) if !state.is_ancestor(client_id, popup_wl, parent_wl) => {
            Some((popup_wl, parent_wl))
        }
        _ => {
            if let Some(client) = state.clients.get(client_id) {
                client.send_error(
                    msg.message.object_id,
                    ERROR_INVALID_POPUP_PARENT,
                    "xdg_surface.get_popup: parent is missing, the popup itself, \
                     or one of its descendants",
                );
            }
            return;
        }
    };

    // Position from the positioner, kept on screen where the client asked for
    // that. Needs the parent's `wl_surface` because the constraining region is
    // the parent's output expressed relative to the parent.
    let (x, y, width, height) = parented
        .and_then(|(_, parent_wl)| {
            state.place_popup((client_id, positioner_id), (client_id, parent_wl))
        })
        .unwrap_or((0, 0, 1, 1));

    // NLL: the borrow taken to decode the arguments ends above, because placing
    // the popup reads the outputs and the surface tree — state the client
    // borrow would otherwise lock out.
    let Some(client) = state.clients.get(client_id) else {
        return;
    };
    // Carried down for the same reason as a toplevel's: `repositioned` is
    // gated on version 3, and a popup left at version 1 would never get one.
    let version = client.version(xdg_surface_id);
    if client
        .register_with_version(popup_id, ObjectType::XdgPopup, version)
        .is_err()
    {
        return;
    }
    state.create_xdg_popup(
        client_id,
        popup_id,
        xdg_surface_id,
        parent_xdg_surface_id,
        positioner_id,
        x,
        y,
        width,
        height,
    );

    // Parent the popup's wl_surface under the parent's wl_surface. Checked
    // acyclic above, before any of this was built.
    if let Some((popup_wl, parent_wl)) = parented {
        if let Some(surface) = state.surfaces.get_mut(&(client_id, popup_wl)) {
            surface.parent = Some(parent_wl);
            surface.subsurface_position = super::super::state::clamp_surface_offset(x, y);
        }
        if let Some(parent) = state.surfaces.get_mut(&(client_id, parent_wl)) {
            parent.children.push(popup_wl);
        }
    }

    // Send xdg_popup.configure with the computed position
    // A popup with no parent yet cannot be configured: its position is
    // measured from a parent it has not been given. `zwlr_layer_surface_v1`
    // sends both configures when it adopts one.
    if parented.is_some() {
        super::xdg_popup::send_configure(state, client_id, popup_id, x, y, width, height);
        // And the xdg_surface.configure whose serial the client must acknowledge.
        send_configure(state, client_id, xdg_surface_id);
    }
}

fn handle_set_window_geometry(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(x), Some(y), Some(w), Some(h)) = (args.i32(), args.i32(), args.i32(), args.i32())
    else {
        super::malformed_request(state, msg, "xdg_surface");
        return;
    };
    let xdg_surface_id = msg.message.object_id;
    debug!(
        "xdg_surface.set_window_geometry: {}x{} at ({},{})",
        w, h, x, y
    );
    if let Some(xdg_surface) = state.xdg_surfaces.get_mut(&(msg.client_id, xdg_surface_id)) {
        xdg_surface.geometry = Some((x, y, w, h));
    }
}

fn handle_ack_configure(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(serial) = args.u32() else {
        super::malformed_request(state, msg, "xdg_surface");
        return;
    };
    let key = (msg.client_id, msg.message.object_id);
    debug!(
        "xdg_surface.ack_configure: xdg_surface_id={} serial={serial}",
        key.1
    );
    let Some(xdg_surface) = state.xdg_surfaces.get_mut(&key) else {
        return;
    };

    // A serial newer than anything sent names a configure that never happened.
    // There is nothing to do with it and nothing sensible to assume, and a
    // client this far out of step will misread whatever comes next.
    if serial > xdg_surface.highest_configure {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                key.1,
                ERROR_INVALID_SERIAL,
                &format!("xdg_surface.ack_configure: {serial} was never sent"),
            );
        }
        return;
    }

    // Acknowledging one configure acknowledges every earlier one with it: the
    // protocol lets a client answer only the most recent of a burst, which is
    // exactly what a client that fell behind a resize drag does.
    if let Some(at) = xdg_surface
        .pending_configures
        .iter()
        .position(|&pending| pending == serial)
    {
        xdg_surface.pending_configures.drain(..=at);
        xdg_surface.configured = true;
        return;
    }

    // Not outstanding, and not invented: the client is acknowledging a
    // configure this surface has already moved past — one it answered before,
    // or one dropped from the front of a very long queue. Ordinary and
    // harmless, and emphatically not worth a disconnection.
    debug!(
        "xdg_surface.ack_configure: serial {serial} is stale for xdg_surface {}",
        key.1
    );
}

/// Check that a surface is allowed to show content yet.
///
/// A client must acknowledge a configure before it commits a buffer. The
/// configure is where the compositor says how big the window may be and what
/// state it is in, so content committed before one is content sized against
/// nothing — and the protocol makes it an error rather than letting the
/// window appear at a size neither side agreed.
///
/// Only for a surface that has an xdg role. A `wl_surface` with no
/// `xdg_surface` — a cursor, a drag icon, a subsurface — has no configure to
/// wait for.
pub(crate) fn check_configured_before_buffer(
    state: &mut CompositorState,
    surface: ClientObjectId,
) -> bool {
    // A layer surface has the same rule and its own bookkeeping: it may not
    // show content before it has agreed the size the compositor proposed.
    if let Some(layer_key) = state.layer_surface_of(surface) {
        return state
            .layer_surfaces
            .get(&layer_key)
            .is_some_and(|layer| layer.configured);
    }
    let Some(xdg_key) = state.xdg_surface_of(surface) else {
        return true;
    };
    let configured = state
        .xdg_surfaces
        .get(&xdg_key)
        .is_some_and(|xdg| xdg.configured);
    if configured {
        return true;
    }
    tracing::warn!(
        "client {}: xdg_surface {} committed a buffer before acknowledging a configure",
        surface.0,
        xdg_key.1,
    );
    if let Some(client) = state.clients.get(surface.0) {
        client.send_error(
            xdg_key.1,
            ERROR_UNCONFIGURED_BUFFER,
            "xdg_surface: a buffer was committed before a configure was acknowledged",
        );
    }
    false
}

/// Send an `xdg_surface.configure`, and remember the serial it carried.
///
/// The only thing that allocates a configure serial. Every configure the
/// compositor sends has to be one the client can legally acknowledge, and the
/// two halves — sending the serial and recording it — must not be separable,
/// or a configure goes out that `ack_configure` will then reject as invented.
/// The callers used to allocate their own serial and pass it in, which made
/// that exactly one forgotten line away.
pub fn send_configure(state: &mut CompositorState, client_id: u32, xdg_surface_id: u32) {
    let serial = super::next_serial();
    state.record_configure((client_id, xdg_surface_id), serial);
    let args = ArgWriter::new().u32(serial).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(xdg_surface_id, CONFIGURE, args));
    }
}
