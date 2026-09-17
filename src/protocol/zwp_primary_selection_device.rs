//! `zwp_primary_selection_device_v1` protocol handler.
//!
//! Where a client is told what the middle-click clipboard holds, and where it
//! says what it should hold. Per-seat, and since there is one seat here,
//! per-client in practice — though a client may hold more than one device, and
//! each is told separately.
//!
//! The primary selection follows keyboard focus exactly as the clipboard does,
//! and for the same reason: there is one selection at a time, and the client
//! with focus is the one that may read it. What differs is how it is *set*.
//! `wl_data_device.set_selection` is a deliberate copy, a keystroke the user
//! chose; a primary selection is a side effect of selecting text at all, so a
//! client sets it far more often and from whatever event it happened to be
//! handling. The serial check is the same short history of what the compositor
//! has sent that client, which is what makes that acceptable.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState, DataInterface, DataOffer, OfferKind};
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};
use super::zwp_primary_selection_offer;

pub const INTERFACE: &str = "zwp_primary_selection_device_v1";

// Request opcodes
const SET_SELECTION: u16 = 0;
const DESTROY: u16 = 1;

// Event opcodes
pub const DATA_OFFER: u16 = 0;
pub const SELECTION: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        SET_SELECTION => handle_set_selection(state, msg),
        DESTROY => handle_destroy(state, msg),
        _ => super::reject_unknown_request(state, msg, INTERFACE),
    }
}

fn handle_set_selection(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // set_selection args: object source (nullable), uint serial
    let (Some(source_id), Some(serial)) = (args.u32(), args.u32()) else {
        super::reject_malformed_request(state, msg, INTERFACE);
        return;
    };

    // The serial of whatever event the client was handling when the user made
    // the selection — a button press or a key, usually a whole gesture ago by
    // the time the client gets to it. Checked against the short history rather
    // than the newest serial for that reason.
    if !state.is_recent_input_serial(msg.client_id, serial) {
        debug!("{INTERFACE}.set_selection refused: serial {serial} is not one we sent");
        return;
    }

    let new_selection = if source_id == 0 {
        None
    } else {
        let key = (msg.client_id, source_id);
        // Quietly, because this interface has no errors at all: an id that is
        // not one of this client's primary sources cannot be honoured and
        // cannot be refused either.
        // And one of *this* interface: one map holds the sources of all three
        // now, so an id that names a `wl_data_source` would otherwise be found
        // here and put on the primary selection, where its owner would be told
        // to send through an interface it never bound.
        if state.data_sources.get(&key).map(|s| s.interface)
            != Some(DataInterface::PrimarySelection)
        {
            debug!(
                "{INTERFACE}.set_selection refused: {source_id} is not a primary source of client {}",
                msg.client_id,
            );
            return;
        }
        Some(key)
    };

    debug!("{INTERFACE}.set_selection: {new_selection:?}");
    state.set_primary_selection(new_selection);
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let device_id = msg.message.object_id;
    state
        .primary_devices
        .retain(|d| !(d.client_id == msg.client_id && d.object_id == device_id));
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(device_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

/// Create an offer on one device and tell the client what it contains.
///
/// The order is the protocol's: the client has to have been told the object
/// exists, and what mime types are behind it, before it is told that the object
/// is the selection.
///
/// Returns the offer's key, or `None` if the client's server id space is spent.
fn create_offer(
    state: &mut CompositorState,
    client_id: u32,
    device_id: u32,
    source: ClientObjectId,
) -> Option<ClientObjectId> {
    let client = state.clients.get(client_id)?;
    // No version to carry: this protocol has one, and no event on it is gated.
    let offer_id = client.register_server_object(ObjectType::ZwpPrimarySelectionOffer)?;

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
            kind: OfferKind::Selection,
            accepted: None,
            actions: 0,
            preferred_action: 0,
            resolved_action: 0,
        },
    );

    send_data_offer(state, client_id, device_id, offer_id);
    for mime_type in &mime_types {
        zwp_primary_selection_offer::send_offer(state, offer, mime_type);
    }
    Some(offer)
}

/// Hand a client the current primary selection, on every one of its devices.
///
/// `selection` is a per-device event, so a client holding two devices is told
/// twice and gets an offer for each — an offer belongs to the device it arrived
/// on.
pub fn send_selection_to_client(state: &mut CompositorState, client_id: u32) {
    let devices: Vec<u32> = state
        .primary_devices
        .iter()
        .filter(|d| d.client_id == client_id)
        .map(|d| d.object_id)
        .collect();
    for device_id in devices {
        send_selection_to_device(state, client_id, device_id);
    }
}

/// Hand one device the current primary selection, or tell it there is none.
///
/// The client owning the selection is offered it back like any other: pasting
/// into the window the text was selected in is the ordinary case, and the
/// descriptor relay handles a client talking to itself without noticing.
pub fn send_selection_to_device(state: &mut CompositorState, client_id: u32, device_id: u32) {
    let Some(source) = state.primary_selection else {
        send_selection(state, client_id, device_id, None);
        return;
    };
    let Some(offer) = create_offer(state, client_id, device_id, source) else {
        return;
    };
    send_selection(state, client_id, device_id, Some(offer.1));
}

/// Send `zwp_primary_selection_device_v1.data_offer`, introducing an offer the
/// compositor named.
pub fn send_data_offer(state: &mut CompositorState, client_id: u32, device_id: u32, offer_id: u32) {
    let args = ArgWriter::new().u32(offer_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, DATA_OFFER, args));
    }
}

/// Send `zwp_primary_selection_device_v1.selection`, naming the offer that is
/// now the primary selection. A null offer means there is none.
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
