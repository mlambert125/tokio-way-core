//! `wl_data_source` protocol handler.
//!
//! A data source is a list of mime types a client says it can produce, offered
//! either as the clipboard or as the content of a drag. It holds no data: what
//! moves the bytes is a pipe the two clients share, and this interface's
//! `send` event is where the compositor hands one end across.

use std::os::fd::OwnedFd;

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState};
use super::wire_utils::{ArgReader, ArgWriter, build_message, build_message_with_fds};
use super::wl_data_device_manager;

// Request opcodes
const OFFER: u16 = 0;
const DESTROY: u16 = 1;
const SET_ACTIONS: u16 = 2;

// Event opcodes
pub const TARGET: u16 = 0;
pub const SEND: u16 = 1;
pub const CANCELLED: u16 = 2;
pub const DND_DROP_PERFORMED: u16 = 3;
pub const DND_FINISHED: u16 = 4;
pub const ACTION: u16 = 5;

/// `wl_data_source.error.invalid_action_mask`: a mask with bits the protocol
/// does not define.
const ERROR_INVALID_ACTION_MASK: u32 = 0;

/// The version at which the drag-and-drop action requests and events appear.
pub const ACTIONS_SINCE: u32 = 3;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        OFFER => handle_offer(state, msg),
        DESTROY => handle_destroy(state, msg),
        SET_ACTIONS => handle_set_actions(state, msg),
        _ => super::reject_unknown_request(state, msg, "wl_data_source"),
    }
}

fn handle_offer(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(mime_type) = args.string() else {
        super::reject_malformed_request(state, msg, "wl_data_source");
        return;
    };

    let key = (msg.client_id, msg.message.object_id);
    let Some(source) = state.data_sources.get_mut(&key) else {
        return;
    };

    // A duplicate is ignored rather than refused. It says nothing new, and the
    // only way to refuse it would disconnect a client over a repetition that
    // changes nothing.
    if !source.mime_types.iter().any(|m| m == &mime_type) {
        source.mime_types.push(mime_type);
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    // No `cancelled` — the client is destroying the source itself and does not
    // need telling that it is gone.
    state.retire_data_source(key, false);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(msg.message.object_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

fn handle_set_actions(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(actions) = args.u32() else {
        super::reject_malformed_request(state, msg, "wl_data_source");
        return;
    };

    if actions & !wl_data_device_manager::DND_ACTION_ALL != 0 {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_ACTION_MASK,
                "wl_data_source.set_actions: mask has actions the protocol does not define",
            );
        }
        return;
    }

    let key = (msg.client_id, msg.message.object_id);
    if let Some(source) = state.data_sources.get_mut(&key) {
        source.actions = actions;
    }

    // The protocol also has this callable only once, and only on a drag source.
    // Neither is enforced: both are rules about how the object was used
    // earlier, the only way to refuse is fatal, and a false positive kills an
    // application over something the user cannot see. A mask with undefined
    // bits is different in kind — that is the client and the compositor
    // disagreeing about the interface itself.
    state.resolve_drag_actions();
}

/// Send `wl_data_source.target`, naming what the drag target will accept.
///
/// A null mime type says it will take nothing, which is how a source knows to
/// draw a "no drop" cursor. It is a genuinely null string rather than an empty
/// one — see [`ArgWriter::string_or_null`].
pub fn send_target(state: &mut CompositorState, source: ClientObjectId, mime_type: Option<&str>) {
    let args = ArgWriter::new().string_or_null(mime_type).build();
    if let Some(client) = state.clients.get(source.0) {
        let _ = client.send(build_message(source.1, TARGET, args));
    }
}

/// Hand the source client the descriptor a receiver is waiting to read from.
///
/// This is the whole of the transfer. The descriptor came from the receiving
/// client on `wl_data_offer.receive` and is passed across untouched; the two
/// clients then talk directly through the pipe and the compositor reads
/// nothing. If this send fails, dropping the message closes our copy, which the
/// reader sees as an end of file rather than a hang.
pub fn send_send(
    state: &mut CompositorState,
    source: ClientObjectId,
    mime_type: &str,
    fd: OwnedFd,
) {
    let args = ArgWriter::new().string(mime_type).build();
    if let Some(client) = state.clients.get(source.0) {
        let _ = client.send(build_message_with_fds(source.1, SEND, args, vec![fd]));
    }
}

/// Send `wl_data_source.cancelled`: the source is no longer being offered.
///
/// Sent when a selection is replaced, when a drag ends without a drop, and when
/// the compositor can no longer make good on the source for any other reason.
pub fn send_cancelled(state: &mut CompositorState, source: ClientObjectId) {
    if let Some(client) = state.clients.get(source.0) {
        let _ = client.send(build_message(source.1, CANCELLED, Vec::new()));
    }
}

/// Send `wl_data_source.dnd_drop_performed`: the user let go over a target that
/// accepted. The source may stop drawing the drag, but must not yet destroy
/// itself — the target has still to read the data.
pub fn send_dnd_drop_performed(state: &mut CompositorState, source: ClientObjectId) {
    if let Some(client) = state.clients.get(source.0)
        && client.version(source.1) >= ACTIONS_SINCE
    {
        let _ = client.send(build_message(source.1, DND_DROP_PERFORMED, Vec::new()));
    }
}

/// Send `wl_data_source.dnd_finished`: the target has finished with the data.
///
/// Only a version 3 target sends the `finish` that triggers this, so an older
/// one leaves the source never hearing it. That is what the protocol gives it.
pub fn send_dnd_finished(state: &mut CompositorState, source: ClientObjectId) {
    if let Some(client) = state.clients.get(source.0)
        && client.version(source.1) >= ACTIONS_SINCE
    {
        let _ = client.send(build_message(source.1, DND_FINISHED, Vec::new()));
    }
}

/// Send `wl_data_source.action`: the action the two sides have settled on.
pub fn send_action(state: &mut CompositorState, source: ClientObjectId, action: u32) {
    debug!("wl_data_source.action: source={source:?} action={action}");
    let args = ArgWriter::new().u32(action).build();
    if let Some(client) = state.clients.get(source.0)
        && client.version(source.1) >= ACTIONS_SINCE
    {
        let _ = client.send(build_message(source.1, ACTION, args));
    }
}
