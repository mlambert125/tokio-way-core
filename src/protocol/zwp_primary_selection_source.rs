//! `zwp_primary_selection_source_v1` protocol handler.
//!
//! The primary selection's answer to `wl_data_source`: a list of mime types a
//! client says it can produce, offered as the middle-click clipboard. It holds
//! no data, for the same reason and by the same means — what moves the bytes is
//! a pipe the two clients share, and this interface's `send` event is where the
//! compositor hands one end across.
//!
//! What it has not got is the drag half. A primary source is only ever a
//! selection: there are no actions to negotiate, no `target`, and no `finish`.
//! That is most of why this is shorter than the interface it mirrors rather
//! than a transcription of it.

use std::os::fd::OwnedFd;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState};
use super::wire_utils::{ArgReader, ArgWriter, build_message, build_message_with_fds};

pub const INTERFACE: &str = "zwp_primary_selection_source_v1";

// Request opcodes
const OFFER: u16 = 0;
const DESTROY: u16 = 1;

// Event opcodes
pub const SEND: u16 = 0;
pub const CANCELLED: u16 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        OFFER => handle_offer(state, msg),
        DESTROY => handle_destroy(state, msg),
        _ => super::reject_unknown_request(state, msg, INTERFACE),
    }
}

fn handle_offer(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(mime_type) = args.string() else {
        super::reject_malformed_request(state, msg, INTERFACE);
        return;
    };

    let key = (msg.client_id, msg.message.object_id);
    let Some(source) = state.data_sources.get_mut(&key) else {
        return;
    };

    // A duplicate says nothing new, and this interface defines no error to
    // refuse it with — the only way to say no would be to end the connection
    // over a repetition that changes nothing.
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

/// Hand the source client the descriptor a receiver is waiting to read from.
///
/// The whole of the transfer, exactly as on `wl_data_source`: the descriptor
/// came from the receiving client on `zwp_primary_selection_offer_v1.receive`
/// and is passed across untouched. If this send fails, dropping the message
/// closes our copy, which the reader sees as an end of file rather than a hang.
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

/// Send `zwp_primary_selection_source_v1.cancelled`: the source no longer owns
/// the primary selection, and the client should destroy it.
///
/// Unlike `wl_data_source.cancelled` this has one cause. There is no drag to
/// end, so a primary source is cancelled when — and only when — another
/// selection replaces it.
pub fn send_cancelled(state: &mut CompositorState, source: ClientObjectId) {
    if let Some(client) = state.clients.get(source.0) {
        let _ = client.send(build_message(source.1, CANCELLED, Vec::new()));
    }
}
