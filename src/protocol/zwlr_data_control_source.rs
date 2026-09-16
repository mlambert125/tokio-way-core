//! `zwlr_data_control_source_v1` protocol handler.
//!
//! What a clipboard manager offers a selection *with*. The same list of mime
//! types the other two source interfaces are, and told to send through the same
//! pipe relay — what differs is who may use it, and when.
//!
//! One source of this interface can be put on either selection, which is why
//! the compositor keeps the sources of all three interfaces in one map: a
//! selection names a source, and "which selection" and "which interface" are
//! not the same question.

use std::os::fd::OwnedFd;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState};
use super::wire_utils::{ArgReader, ArgWriter, build_message, build_message_with_fds};

pub const INTERFACE: &str = "zwlr_data_control_source_v1";

// Request opcodes
const OFFER: u16 = 0;
const DESTROY: u16 = 1;

// Event opcodes
pub const SEND: u16 = 0;
pub const CANCELLED: u16 = 1;

/// `zwlr_data_control_source_v1.error.invalid_offer`: a mime type added after
/// the source was cancelled.
const ERROR_INVALID_OFFER: u32 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        OFFER => handle_offer(state, msg),
        DESTROY => handle_destroy(state, msg),
        _ => super::unknown_request(state, msg, INTERFACE),
    }
}

fn handle_offer(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(mime_type) = args.string() else {
        super::malformed_request(state, msg, INTERFACE);
        return;
    };

    let key = (msg.client_id, msg.message.object_id);
    // Unlike the other two source interfaces, this one makes adding a mime type
    // to a cancelled source an error rather than something to ignore. A
    // cancelled source is still in the map — the client owns the id until it
    // destroys it — so this reads the flag rather than inferring it from
    // absence.
    if state.data_sources.get(&key).is_none_or(|s| s.cancelled) {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                ERROR_INVALID_OFFER,
                "zwlr_data_control_source_v1.offer: this source has been cancelled",
            );
        }
        return;
    }

    if let Some(source) = state.data_sources.get_mut(&key)
        && !source.mime_types.iter().any(|m| m == &mime_type)
    {
        source.mime_types.push(mime_type);
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    // No `cancelled`: the client is destroying the source itself.
    state.retire_data_source(key, false);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(msg.message.object_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

/// Hand the source client the descriptor a receiver is waiting to read from.
///
/// Reached through [`super::send_source_content`], since the client reading may
/// be holding an offer of any of the three interfaces — a data-control source
/// is read from by ordinary clients pasting, not only by other managers.
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

/// Send `zwlr_data_control_source_v1.cancelled`: something else owns the
/// selection this source was on, and the client should destroy it.
pub fn send_cancelled(state: &mut CompositorState, source: ClientObjectId) {
    if let Some(client) = state.clients.get(source.0) {
        let _ = client.send(build_message(source.1, CANCELLED, Vec::new()));
    }
}
