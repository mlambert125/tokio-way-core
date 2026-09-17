//! `wl_data_device` protocol handler.
//!
//! A data device is where a client is told two things: what the clipboard
//! holds, and what is being dragged over its surfaces. It is per-seat, and
//! since there is one seat here, per-client in practice — though a client may
//! hold more than one, and each is told separately.
//!
//! The clipboard follows keyboard focus. There is one selection at a time, and
//! the client that has focus is the one told about it.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{
    ClientObjectId, CompositorState, DataInterface, DataOffer, DataSourceRole, OfferKind,
};
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};
use super::{next_serial, wl_data_offer, wl_data_source};

// Request opcodes
const START_DRAG: u16 = 0;
const SET_SELECTION: u16 = 1;
const RELEASE: u16 = 2;

// Event opcodes
pub const DATA_OFFER: u16 = 0;
pub const ENTER: u16 = 1;
pub const LEAVE: u16 = 2;
pub const MOTION: u16 = 3;
pub const DROP: u16 = 4;
pub const SELECTION: u16 = 5;

/// `wl_data_device.error.role`: the surface offered as a drag icon already has
/// a role.
const ERROR_ROLE: u32 = 0;
/// `wl_data_device.error.used_source`: the source has been offered once
/// already.
const ERROR_USED_SOURCE: u32 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        START_DRAG => handle_start_drag(state, msg),
        SET_SELECTION => handle_set_selection(state, msg),
        RELEASE => handle_release(state, msg),
        _ => super::reject_unknown_request(state, msg, "wl_data_device"),
    }
}

/// Take a source for one use, refusing a second.
///
/// A source is single-use: offering the same one as a selection and then as a
/// drag would leave two unrelated transfers sharing one mime list and one
/// `cancelled`. Returns false if the client has been disconnected for it, or if
/// the id does not name a source of its own.
fn claim_source(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    source_id: u32,
    role: DataSourceRole,
) -> bool {
    let key = (msg.client_id, source_id);
    // A `wl_data_source`, specifically. One map holds the sources of all three
    // interfaces, so an id naming a primary-selection or data-control source
    // would otherwise be found here and put on the clipboard, where its owner
    // would be told to send through an interface it never bound.
    let Some(source) = state
        .data_sources
        .get_mut(&key)
        .filter(|source| source.interface == DataInterface::WlData)
    else {
        debug!(
            "wl_data_device: {source_id} is not a data source of client {}",
            msg.client_id
        );
        return false;
    };
    if source.role != DataSourceRole::Unused {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_USED_SOURCE,
                "wl_data_device: this source has already been offered",
            );
        }
        return false;
    }
    source.role = role;
    true
}

fn handle_start_drag(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // start_drag args: object source (nullable), object origin, object icon
    // (nullable), uint serial
    let (Some(source_id), Some(origin_id), Some(icon_id), Some(serial)) =
        (args.u32(), args.u32(), args.u32(), args.u32())
    else {
        super::reject_malformed_request(state, msg, "wl_data_device");
        return;
    };

    let client_id = msg.client_id;

    // Refused quietly, all of it. The ordinary cause of every one of these is
    // the user having let go before the client got round to asking, and there
    // is no way to say no that does not end the connection.
    if state.pointer_grab.is_some() || state.drag.is_some() {
        debug!("wl_data_device.start_drag refused: the pointer is already spoken for");
        return;
    }
    if state.pressed_buttons.is_empty() {
        debug!("wl_data_device.start_drag refused: no button is held");
        return;
    }
    if state.last_button_serial.get(&client_id) != Some(&serial) {
        debug!("wl_data_device.start_drag refused: serial {serial} was not a recent press");
        return;
    }
    let origin = (client_id, origin_id);
    if !state.surfaces.contains_key(&origin) {
        debug!("wl_data_device.start_drag refused: {origin_id} is not a surface of this client");
        return;
    }

    // Past here the two failures the protocol names as errors, both fatal.
    let icon = if icon_id == 0 {
        None
    } else {
        let icon = (client_id, icon_id);
        if !claim_icon_role(state, msg, icon) {
            return;
        }
        Some(icon)
    };

    let source = if source_id == 0 {
        None
    } else {
        if !claim_source(state, msg, source_id, DataSourceRole::Drag) {
            return;
        }
        Some((client_id, source_id))
    };

    debug!("wl_data_device.start_drag: origin={origin:?} source={source:?} icon={icon:?}");
    state.start_drag(source, client_id, origin, icon);
}

/// Give a surface the drag-icon role, or refuse it because it has another.
///
/// Permanent, like the cursor role: a surface that has been a drag icon can
/// never become anything else, which is what makes the check on the other side
/// — in `wl_pointer.set_cursor` and `wl_subcompositor.get_subsurface` — worth
/// making at all.
fn claim_icon_role(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    icon: ClientObjectId,
) -> bool {
    let taken = !state.surfaces.contains_key(&icon)
        || state.cursor_role_surfaces.contains(&icon)
        || state.dnd_icon_surfaces.contains(&icon)
        || state
            .surfaces
            .get(&icon)
            .is_some_and(|s| s.parent.is_some())
        || state
            .xdg_surfaces
            .values()
            .any(|x| x.client_id == icon.0 && x.wl_surface_id == icon.1);

    if taken {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_ROLE,
                "wl_data_device.start_drag: the icon surface already has a role",
            );
        }
        return false;
    }
    state.dnd_icon_surfaces.insert(icon);
    true
}

fn handle_set_selection(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // set_selection args: object source (nullable), uint serial
    let (Some(source_id), Some(serial)) = (args.u32(), args.u32()) else {
        super::reject_malformed_request(state, msg, "wl_data_device");
        return;
    };

    // A keyboard serial, usually — this is Ctrl+C far more often than it is a
    // click — so it is checked against the short history of everything the
    // client has been sent rather than against the button currently held.
    if !state.is_recent_input_serial(msg.client_id, serial) {
        debug!("wl_data_device.set_selection refused: serial {serial} is not one we sent");
        return;
    }

    let new_selection = if source_id == 0 {
        None
    } else {
        if !claim_source(state, msg, source_id, DataSourceRole::Selection) {
            return;
        }
        Some((msg.client_id, source_id))
    };

    debug!("wl_data_device.set_selection: {new_selection:?}");
    state.set_selection(new_selection);
}

fn handle_release(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let device_id = msg.message.object_id;
    state
        .data_devices
        .retain(|d| !(d.client_id == msg.client_id && d.object_id == device_id));
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(device_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

/// Create an offer on one device and tell the client what it contains.
///
/// The order is forced by the protocol: the client has to have been told the
/// object exists, and what mime types are behind it, before it is told what the
/// object is *for* — the `selection` or `enter` that follows.
///
/// Returns the offer's key, or `None` if the client's server id space is spent.
pub fn create_offer(
    state: &mut CompositorState,
    client_id: u32,
    device_id: u32,
    source: ClientObjectId,
    kind: OfferKind,
) -> Option<ClientObjectId> {
    let client = state.clients.get(client_id)?;
    // The device's version, so the offer's own events are gated on what the
    // client actually bound rather than on the version 1 default.
    let version = client.version(device_id);
    let offer_id = client.register_server_object_with_version(ObjectType::WlDataOffer, version)?;

    let mime_types = state
        .data_sources
        .get(&source)
        .map(|s| s.mime_types.clone())
        .unwrap_or_default();

    let offer = (client_id, offer_id);
    state.data_offers.insert(
        offer,
        DataOffer {
            client_id,
            source: Some(source),
            kind,
            accepted: None,
            actions: 0,
            preferred_action: 0,
            resolved_action: 0,
        },
    );

    send_data_offer(state, client_id, device_id, offer_id);
    for mime_type in &mime_types {
        wl_data_offer::send_offer(state, offer, mime_type);
    }
    Some(offer)
}

/// Hand a client the current clipboard, on every one of its data devices.
///
/// `selection` is a per-device event, so a client holding two devices is told
/// twice and gets an offer for each — an offer belongs to the device it
/// arrived on.
pub fn send_selection_to_client(state: &mut CompositorState, client_id: u32) {
    let devices: Vec<u32> = state
        .data_devices
        .iter()
        .filter(|d| d.client_id == client_id)
        .map(|d| d.object_id)
        .collect();
    for device_id in devices {
        send_selection_to_device(state, client_id, device_id);
    }
}

/// Hand one device the current clipboard, or tell it there is none.
///
/// A client owning the selection is offered it back like any other. It has to
/// be: copying and pasting within one application goes through this path, and
/// the descriptor relay handles a client talking to itself without noticing.
pub fn send_selection_to_device(state: &mut CompositorState, client_id: u32, device_id: u32) {
    let Some(source) = state.selection else {
        send_selection(state, client_id, device_id, None);
        return;
    };
    let Some(offer) = create_offer(state, client_id, device_id, source, OfferKind::Selection)
    else {
        return;
    };
    send_selection(state, client_id, device_id, Some(offer.1));
}

/// Send `wl_data_device.data_offer`, introducing an offer the compositor named.
pub fn send_data_offer(state: &mut CompositorState, client_id: u32, device_id: u32, offer_id: u32) {
    let args = ArgWriter::new().u32(offer_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, DATA_OFFER, args));
    }
}

/// Send `wl_data_device.selection`, naming the offer that is now the clipboard.
///
/// A null offer means there is nothing on the clipboard.
pub fn send_selection(
    state: &mut CompositorState,
    client_id: u32,
    device_id: u32,
    offer_id: Option<u32>,
) {
    let args = ArgWriter::new().object(offer_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, SELECTION, args));
    }
}

/// Send `wl_data_device.enter`: a drag has arrived over one of this client's
/// surfaces.
///
/// A null offer is a drag with no source — one client dragging within itself,
/// which has nothing to hand anyone.
pub fn send_enter(
    state: &mut CompositorState,
    client_id: u32,
    device_id: u32,
    surface_id: u32,
    x: f64,
    y: f64,
    offer_id: Option<u32>,
) {
    let serial = next_serial();
    let args = ArgWriter::new()
        .u32(serial)
        .u32(surface_id)
        .fixed(x)
        .fixed(y)
        .object(offer_id)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, ENTER, args));
    }
}

/// Send `wl_data_device.leave`: the drag has gone elsewhere.
pub fn send_leave(state: &mut CompositorState, client_id: u32, device_id: u32) {
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, LEAVE, Vec::new()));
    }
}

/// Send `wl_data_device.motion`: the drag has moved within this surface.
pub fn send_motion(
    state: &mut CompositorState,
    client_id: u32,
    device_id: u32,
    time_ms: u32,
    x: f64,
    y: f64,
) {
    let args = ArgWriter::new().u32(time_ms).fixed(x).fixed(y).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, MOTION, args));
    }
}

/// Send `wl_data_device.drop`: the user let go here.
pub fn send_drop(state: &mut CompositorState, client_id: u32, device_id: u32) {
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, DROP, Vec::new()));
    }
}

/// Every data device belonging to a client.
///
/// Collected rather than borrowed because `Clients::get` takes the whole
/// collection mutably, so a send cannot happen while the list is held.
pub fn devices_of(state: &CompositorState, client_id: u32) -> Vec<u32> {
    state
        .data_devices
        .iter()
        .filter(|d| d.client_id == client_id)
        .map(|d| d.object_id)
        .collect()
}

/// Tell a source that its drag target will take nothing, and forget what it
/// had accepted. Used when a drag leaves a surface.
pub fn clear_accepted(state: &mut CompositorState, offer: ClientObjectId) {
    let source = match state.data_offers.get_mut(&offer) {
        Some(o) => {
            o.accepted = None;
            o.source
        }
        None => return,
    };
    if let Some(source) = source {
        wl_data_source::send_target(state, source, None);
    }
}
