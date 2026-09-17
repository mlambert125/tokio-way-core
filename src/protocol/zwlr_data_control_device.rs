//! `zwlr_data_control_device_v1` protocol handler.
//!
//! The interface a clipboard manager binds, and the one place in the data
//! protocols where focus does not come into it.
//!
//! Everything else that is told about a selection is told because it has
//! keyboard focus: that is what makes the clipboard safe, since a client that
//! could read it whenever it liked could read everything the user ever copied.
//! A data-control client is granted exactly that, which is why the interface is
//! privileged — a compositor is expected to hand it out only to clients the
//! user has chosen to run. There is no sandboxing story here to gate it with,
//! so it is advertised to everyone, and that is worth knowing rather than
//! discovering: on this compositor, any client can read the clipboard.
//!
//! What it buys is the thing the clipboard otherwise cannot do. A selection
//! dies with the client that set it, because the bytes only ever existed in
//! that client — copy from a terminal, close it, and there is nothing left to
//! paste. A manager holding this interface takes its own copy while the owner
//! is still alive, and then offers that copy back. The compositor still stores
//! nothing.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{
    ClientObjectId, CompositorState, DataInterface, DataOffer, DataSourceRole, OfferKind,
};
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};
use super::wl_data_offer;

pub const INTERFACE: &str = "zwlr_data_control_device_v1";

// Request opcodes
const SET_SELECTION: u16 = 0;
const DESTROY: u16 = 1;
const SET_PRIMARY_SELECTION: u16 = 2;

// Event opcodes
pub const DATA_OFFER: u16 = 0;
pub const SELECTION: u16 = 1;
// `finished` is opcode 2 and is never sent. It means the compositor will say
// nothing more through this device, and the case the protocol names for it is
// the seat going away — there is one seat here and it never does. Better to say
// so than to carry a sender nothing calls.
pub const PRIMARY_SELECTION: u16 = 3;

/// The version at which the primary selection half of this interface appears.
pub const PRIMARY_SINCE: u32 = 2;

/// `zwlr_data_control_device_v1.error.used_source`: a source offered twice.
const ERROR_USED_SOURCE: u32 = 1;

/// Which selection a request or an event is about.
///
/// The two halves of this interface are the same code with one word changed, so
/// they are one path with this to say which — rather than two that have to be
/// kept in step by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Clipboard,
    Primary,
}

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        SET_SELECTION => handle_set_selection(state, msg, Which::Clipboard),
        DESTROY => handle_destroy(state, msg),
        SET_PRIMARY_SELECTION => handle_set_selection(state, msg, Which::Primary),
        _ => super::reject_unknown_request(state, msg, INTERFACE),
    }
}

/// Take a selection on behalf of a manager.
///
/// No serial, and no check that the client has focus — both deliberate, and
/// both the point of the interface. A manager restoring a selection the user
/// copied ten minutes ago has no recent input event to quote and no window to
/// focus, so the rules that make `wl_data_device.set_selection` safe would make
/// this impossible instead.
fn handle_set_selection(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    which: Which,
) {
    let mut args = ArgReader::new(&msg.message.args);
    // set_selection args: object source (nullable)
    let Some(source_id) = args.u32() else {
        super::reject_malformed_request(state, msg, INTERFACE);
        return;
    };

    let new_selection = if source_id == 0 {
        None
    } else {
        let key = (msg.client_id, source_id);
        // A source of this interface, and one not already spent. Unlike the
        // primary selection's, this interface does have an error for the second
        // use — `used_source` — so a source offered twice is fatal rather than
        // ignored.
        if state.data_sources.get(&key).map(|s| s.interface) != Some(DataInterface::DataControl) {
            debug!(
                "{INTERFACE}.set_selection refused: {source_id} is not a data control source of \
                 client {}",
                msg.client_id,
            );
            return;
        }
        // "Already used before", which is not the same as "is the current
        // selection": a source cancelled and replaced has still been used, and
        // offering it again would put two unrelated transfers behind one mime
        // list and one `cancelled`. The role every source carries records
        // exactly that, and is what `wl_data_device` refuses on too.
        if state
            .data_sources
            .get(&key)
            .is_some_and(|s| s.role != DataSourceRole::Unused)
        {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.send_error(
                    msg.message.object_id,
                    ERROR_USED_SOURCE,
                    "zwlr_data_control_device_v1: this source has already been offered",
                );
            }
            return;
        }
        if let Some(source) = state.data_sources.get_mut(&key) {
            source.role = DataSourceRole::Selection;
        }
        Some(key)
    };

    debug!("{INTERFACE}.set_selection: {which:?} {new_selection:?}");
    match which {
        Which::Clipboard => state.set_selection(new_selection),
        Which::Primary => state.set_primary_selection(new_selection),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let device_id = msg.message.object_id;
    state
        .data_control_devices
        .retain(|d| !(d.client_id == msg.client_id && d.object_id == device_id));
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(device_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

/// Create an offer on one device and tell the client what is in it.
///
/// Returns the offer's key, or `None` if the client's server id space is spent.
fn create_offer(
    state: &mut CompositorState,
    client_id: u32,
    device_id: u32,
    source: ClientObjectId,
) -> Option<ClientObjectId> {
    let client = state.clients.get(client_id)?;
    let version = client.version(device_id);
    let offer_id =
        client.register_server_object_with_version(ObjectType::ZwlrDataControlOffer, version)?;

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
        // `zwlr_data_control_offer_v1.offer` and `wl_data_offer.offer` are the
        // same opcode carrying the same one string, so this is the same event
        // however the object is named.
        wl_data_offer::send_offer(state, offer, mime_type);
    }
    Some(offer)
}

/// Tell every data-control device what the clipboard now holds.
pub fn send_selection_to_all(state: &mut CompositorState) {
    announce(state, Which::Clipboard);
}

/// Tell every data-control device what the primary selection now holds.
pub fn send_primary_selection_to_all(state: &mut CompositorState) {
    announce(state, Which::Primary);
}

/// Hand one device the selection it has just been created with.
///
/// Both selections, because a manager binding mid-session has missed every
/// change that came before and would otherwise believe both were empty until
/// the user next copied something.
pub fn send_current_selections(state: &mut CompositorState, client_id: u32, device_id: u32) {
    send_to_device(state, client_id, device_id, Which::Clipboard);
    send_to_device(state, client_id, device_id, Which::Primary);
}

/// The fan-out. Collected first because `Clients::get` borrows mutably, so the
/// list cannot be held while the sends happen.
fn announce(state: &mut CompositorState, which: Which) {
    let devices: Vec<(u32, u32)> = state
        .data_control_devices
        .iter()
        .map(|d| (d.client_id, d.object_id))
        .collect();
    for (client_id, device_id) in devices {
        send_to_device(state, client_id, device_id, which);
    }
}

fn send_to_device(state: &mut CompositorState, client_id: u32, device_id: u32, which: Which) {
    let (event, source) = match which {
        Which::Clipboard => (SELECTION, state.selection),
        Which::Primary => (PRIMARY_SELECTION, state.primary_selection),
    };
    // A version 1 manager has no primary selection half at all. Sending it the
    // event anyway would be an opcode its object does not have.
    if which == Which::Primary
        && state
            .clients
            .get(client_id)
            .is_none_or(|client| client.version(device_id) < PRIMARY_SINCE)
    {
        return;
    }

    let Some(source) = source else {
        send_selection(state, client_id, device_id, event, None);
        return;
    };
    let Some(offer) = create_offer(state, client_id, device_id, source) else {
        return;
    };
    send_selection(state, client_id, device_id, event, Some(offer.1));
}

/// Send `zwlr_data_control_device_v1.data_offer`.
pub fn send_data_offer(state: &mut CompositorState, client_id: u32, device_id: u32, offer_id: u32) {
    let args = ArgWriter::new().u32(offer_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, DATA_OFFER, args));
    }
}

/// Send `selection` or `primary_selection`, naming the offer that now holds it.
/// A null offer means there is nothing on that selection.
fn send_selection(
    state: &mut CompositorState,
    client_id: u32,
    device_id: u32,
    event: u16,
    offer_id: Option<u32>,
) {
    let args = ArgWriter::new().object(offer_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(device_id, event, args));
    }
}
