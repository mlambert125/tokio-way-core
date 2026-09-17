//! `wl_data_offer` protocol handler.
//!
//! An offer is the compositor's name for a `wl_data_source` belonging to some
//! other client, handed to a client that may read from it — the clipboard, or
//! the content of a drag passing over one of its surfaces.
//!
//! The id is the compositor's rather than the client's, which has a consequence
//! worth keeping in mind throughout this file: `wl_display.delete_id` is never
//! sent for a server id, so nothing but the client's own `destroy` takes the id
//! out of its object map. When the compositor stops caring about an offer — the
//! clipboard has been replaced, the drag has left the window — it forgets the
//! offer's *contents* and keeps its *identity*. Every request here therefore
//! has to survive its backing [`DataOffer`] having gone.

use std::os::fd::OwnedFd;

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState, OfferKind};
use super::wire_utils::{ArgReader, ArgWriter, build_message};
use super::{wl_data_device_manager, wl_data_source};

// Request opcodes
const ACCEPT: u16 = 0;
pub const RECEIVE: u16 = 1;
const DESTROY: u16 = 2;
const FINISH: u16 = 3;
const SET_ACTIONS: u16 = 4;

// Event opcodes
pub const OFFER: u16 = 0;
pub const SOURCE_ACTIONS: u16 = 1;
pub const ACTION: u16 = 2;

/// `wl_data_offer.error.invalid_finish`: finished before the drop.
const ERROR_INVALID_FINISH: u32 = 0;
/// `wl_data_offer.error.invalid_action_mask`: a mask with undefined bits.
const ERROR_INVALID_ACTION_MASK: u32 = 1;
/// `wl_data_offer.error.invalid_action`: a preference outside the mask.
const ERROR_INVALID_ACTION: u32 = 2;

/// `_fds` carries the pipe passed with `receive`. Dropping it closes our end,
/// which gives the requesting client an immediate EOF rather than a hang — the
/// right answer for a stale offer, a source that has gone, and a mime type that
/// was never offered.
pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo, fds: Vec<OwnedFd>) {
    match msg.message.op_code {
        ACCEPT => handle_accept(state, msg),
        RECEIVE => handle_receive(state, msg, fds),
        DESTROY => handle_destroy(state, msg),
        FINISH => handle_finish(state, msg),
        SET_ACTIONS => handle_set_actions(state, msg),
        _ => super::reject_unknown_request(state, msg, "wl_data_offer"),
    }
}

fn handle_accept(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // accept args: uint serial, string mime_type (nullable)
    let (Some(_serial), Some(mime_type)) = (args.u32(), args.string_or_null()) else {
        super::reject_malformed_request(state, msg, "wl_data_offer");
        return;
    };

    // The serial is meant to be the one from `wl_data_device.enter`, and is not
    // checked. There is no non-fatal way to refuse it, the worst a stale one
    // costs is a briefly wrong drag cursor, and disconnecting a client over
    // that is out of all proportion.
    let key = (msg.client_id, msg.message.object_id);
    let Some(offer) = state.data_offers.get_mut(&key) else {
        return;
    };
    offer.accepted.clone_from(&mime_type);
    let source = offer.source;

    if let Some(source) = source {
        wl_data_source::send_target(state, source, mime_type.as_deref());
    }
    state.resolve_offer_action(key);
}

/// Relay the receiving client's pipe to the source client.
///
/// The compositor moves one descriptor and reads nothing. Every refusal here
/// drops the descriptor instead of sending an error: a client pasting from a
/// clipboard whose owner has exited, or asking for a mime type that was never
/// offered, has done nothing that warrants losing its connection — it gets an
/// empty paste, which is what actually happened.
fn handle_receive(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    fds: Vec<OwnedFd>,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(mime_type) = args.string() else {
        super::reject_malformed_request(state, msg, "wl_data_offer");
        return;
    };
    // The dispatcher guarantees exactly one, having claimed it from the
    // client's queue against `request_fd_count`.
    let Some(fd) = fds.into_iter().next() else {
        return;
    };

    let key = (msg.client_id, msg.message.object_id);
    let Some(source) = state.data_offers.get(&key).and_then(|o| o.source) else {
        debug!("wl_data_offer.receive: offer {key:?} has no source, closing the pipe");
        return;
    };
    let offered = state
        .data_sources
        .get(&source)
        .is_some_and(|s| s.mime_types.iter().any(|m| m == &mime_type));
    if !offered {
        debug!("wl_data_offer.receive: {mime_type} was never offered, closing the pipe");
        return;
    }

    // Not necessarily a `wl_data_source` behind it: a data-control client can
    // own the clipboard, and its source is told to send through its own
    // interface. This is the one call that has to care, and it does not.
    super::send_source_content(state, source, &mime_type, fd);
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    state.data_offers.remove(&key);
    // The target has bowed out through this offer. A release still to come
    // resolves as a cancel, unless the client kept another offer it had
    // accepted through.
    if let Some(drag) = state.drag.as_mut() {
        drag.focus_offers.retain(|&o| o != key);
    }
    if let Some(client) = state.clients.get(msg.client_id) {
        // A server id, so this removes the object without a `delete_id` — the
        // client allocated nothing and has nothing to be told is free.
        client.unregister(msg.message.object_id);
    }
}

fn handle_finish(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    let kind = state.data_offers.get(&key).map(|o| o.kind);

    if kind != Some(OfferKind::Dropped) {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_FINISH,
                "wl_data_offer.finish: nothing has been dropped on this offer",
            );
        }
        return;
    }

    // Spent: the source has been told the transfer is done, so there is nothing
    // left to read. The object stays until the client destroys it.
    let source = state
        .data_offers
        .get_mut(&key)
        .and_then(|o| o.source.take());
    if let Some(source) = source {
        wl_data_source::send_dnd_finished(state, source);
    }
}

fn handle_set_actions(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(actions), Some(preferred)) = (args.u32(), args.u32()) else {
        super::reject_malformed_request(state, msg, "wl_data_offer");
        return;
    };

    if actions & !wl_data_device_manager::DND_ACTION_ALL != 0 {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_ACTION_MASK,
                "wl_data_offer.set_actions: mask has actions the protocol does not define",
            );
        }
        return;
    }
    // A preference must be a single action, and one the client said it would
    // accept — otherwise it is asking for something it has just refused.
    if preferred != 0 && (!preferred.is_power_of_two() || preferred & actions == 0) {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_ACTION,
                "wl_data_offer.set_actions: preferred action is not one of the accepted ones",
            );
        }
        return;
    }

    let key = (msg.client_id, msg.message.object_id);
    if let Some(offer) = state.data_offers.get_mut(&key) {
        offer.actions = actions;
        offer.preferred_action = preferred;
    }
    state.resolve_offer_action(key);
}

/// Send `wl_data_offer.offer`, naming one mime type the source can produce.
pub fn send_offer(state: &mut CompositorState, offer: ClientObjectId, mime_type: &str) {
    let args = ArgWriter::new().string(mime_type).build();
    if let Some(client) = state.clients.get(offer.0) {
        let _ = client.send(build_message(offer.1, OFFER, args));
    }
}

/// Send `wl_data_offer.source_actions`: everything the source will allow.
///
/// Sent before `wl_data_device.enter`, so the client has the whole picture at
/// the moment it decides what to accept.
pub fn send_source_actions(state: &mut CompositorState, offer: ClientObjectId, actions: u32) {
    let args = ArgWriter::new().u32(actions).build();
    if let Some(client) = state.clients.get(offer.0)
        && client.version(offer.1) >= wl_data_source::ACTIONS_SINCE
    {
        let _ = client.send(build_message(offer.1, SOURCE_ACTIONS, args));
    }
}

/// Send `wl_data_offer.action`: the action the two sides have settled on.
pub fn send_action(state: &mut CompositorState, offer: ClientObjectId, action: u32) {
    let args = ArgWriter::new().u32(action).build();
    if let Some(client) = state.clients.get(offer.0)
        && client.version(offer.1) >= wl_data_source::ACTIONS_SINCE
    {
        let _ = client.send(build_message(offer.1, ACTION, args));
    }
}
