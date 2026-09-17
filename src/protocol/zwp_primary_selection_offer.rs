//! `zwp_primary_selection_offer_v1` protocol handler.
//!
//! An offer is the compositor's name for a primary selection source belonging
//! to some other client, handed to whoever may read from it — which, the
//! selection following keyboard focus, is the focused client.
//!
//! The id is the compositor's rather than the client's, with the consequence
//! `wl_data_offer` documents at more length: `wl_display.delete_id` is never
//! sent for a server id, so nothing but the client's own `destroy` takes the id
//! out of its object map. When the compositor stops caring about an offer —
//! the selection has been replaced, its source has gone — it forgets the
//! offer's *contents* and keeps its *identity*, so `receive` here has to
//! survive its backing source having gone.

use std::os::fd::OwnedFd;

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{ClientObjectId, CompositorState};
use super::wire_utils::{ArgReader, ArgWriter, build_message};

pub const INTERFACE: &str = "zwp_primary_selection_offer_v1";

// Request opcodes
pub const RECEIVE: u16 = 0;
const DESTROY: u16 = 1;

// Event opcodes
pub const OFFER: u16 = 0;

/// `fds` carries the pipe passed with `receive`. Dropping it closes our end,
/// which gives the requesting client an immediate EOF rather than a hang — the
/// right answer for a stale offer, a source that has gone, and a mime type that
/// was never offered.
pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo, fds: Vec<OwnedFd>) {
    match msg.message.op_code {
        RECEIVE => handle_receive(state, msg, fds),
        DESTROY => handle_destroy(state, msg),
        _ => super::reject_unknown_request(state, msg, INTERFACE),
    }
}

/// Relay the receiving client's pipe to the source client.
///
/// The compositor moves one descriptor and reads nothing. Every refusal drops
/// the descriptor instead of sending an error: this interface defines none, and
/// a client pasting from a selection whose owner has exited has done nothing
/// that warrants losing its connection. It gets an empty paste, which is what
/// actually happened.
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
    // The dispatcher guarantees exactly one, having claimed it from the
    // client's queue against `request_fd_count`.
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

    // Which interface the source speaks is not this one's business: a
    // data-control client can own the primary selection, and its source is told
    // to send through an interface of its own.
    super::send_source_content(state, source, &mime_type, fd);
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    state.data_offers.remove(&key);
    if let Some(client) = state.clients.get(msg.client_id) {
        // A server id, so this removes the object without a `delete_id` — the
        // client allocated nothing and has nothing to be told is free.
        client.unregister(msg.message.object_id);
    }
}

/// Send `zwp_primary_selection_offer_v1.offer`, naming one mime type the source
/// can produce.
pub fn send_offer(state: &mut CompositorState, offer: ClientObjectId, mime_type: &str) {
    let args = ArgWriter::new().string(mime_type).build();
    if let Some(client) = state.clients.get(offer.0) {
        let _ = client.send(build_message(offer.1, OFFER, args));
    }
}
