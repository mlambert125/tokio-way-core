//! `zwlr_data_control_offer_v1` protocol handler.
//!
//! What a clipboard manager reads a selection through. Identical in shape to
//! the other two offer interfaces — a list of mime types and a `receive` that
//! hands over a pipe — and identical in opcode too, which is why
//! [`super::wl_data_offer::send_offer`] is not duplicated for it.
//!
//! The id is the compositor's, with the consequence `wl_data_offer` sets out at
//! length: nothing but the client's own `destroy` takes it out of the object
//! map, so every request here has to survive the offer's source having gone.

use std::os::fd::OwnedFd;

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::ArgReader;

pub const INTERFACE: &str = "zwlr_data_control_offer_v1";

// Request opcodes
pub const RECEIVE: u16 = 0;
const DESTROY: u16 = 1;

// Event opcodes
pub const OFFER: u16 = 0;

// Its one event is `offer`: opcode 0 carrying a single string, which is exactly
// `wl_data_offer.offer`. So that builder sends this interface's too, rather than
// a copy of it living here — and this checks the assumption at compile time
// instead of leaving it to be discovered when one of the two moves.
const _: () = assert!(OFFER == super::wl_data_offer::OFFER);

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo, fds: Vec<OwnedFd>) {
    match msg.message.op_code {
        RECEIVE => handle_receive(state, msg, fds),
        DESTROY => handle_destroy(state, msg),
        _ => super::reject_unknown_request(state, msg, INTERFACE),
    }
}

/// Relay the manager's pipe to whoever owns the selection.
///
/// This is the request the whole interface exists for: it is how a clipboard
/// manager takes a copy of a selection while its owner is still running, so
/// that the content can outlive the client that produced it. The compositor
/// still copies nothing — it moves one descriptor, exactly as it does for an
/// ordinary paste.
fn handle_receive(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    fds: Vec<OwnedFd>,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(mime_type) = args.string() else {
        super::reject_malformed_request(state, msg, INTERFACE);
        return;
    };
    let Some(fd) = fds.into_iter().next() else {
        return;
    };

    let key = (msg.client_id, msg.message.object_id);
    let Some(source) = state.data_offers.get(&key).and_then(|o| o.source) else {
        debug!("{INTERFACE}.receive: offer {key:?} has no source, closing the pipe");
        return;
    };
    let offered = state
        .data_sources
        .get(&source)
        .is_some_and(|s| s.mime_types.iter().any(|m| m == &mime_type));
    if !offered {
        debug!("{INTERFACE}.receive: {mime_type} was never offered, closing the pipe");
        return;
    }

    super::send_source_content(state, source, &mime_type, fd);
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    state.data_offers.remove(&key);
    if let Some(client) = state.clients.get(msg.client_id) {
        // A server id, so no `delete_id` — the client allocated nothing.
        client.unregister(msg.message.object_id);
    }
}
