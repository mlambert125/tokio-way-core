//! `wl_buffer` protocol handler.
//!
//! A buffer represents pixel data that can be attached to a surface.
//! The only client request is destroy. The compositor sends the release
//! event when it's done reading the buffer's contents.

use crate::protocol::build_message;
use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;

// Request opcodes
const DESTROY: u16 = 0;

// Event opcodes (sent by compositor)
pub const RELEASE: u16 = 0;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            let buffer_id = msg.message.object_id;
            state.destroy_buffer(msg.client_id, buffer_id);
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(buffer_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        _ => super::reject_unknown_request(state, msg, "wl_buffer"),
    }
}

#[allow(dead_code)]
pub fn send_release(state: &mut CompositorState, client_id: u32, buffer_id: u32) {
    let Some(client) = state.clients.get(client_id) else {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    };
    let _ = client.send(build_message(buffer_id, RELEASE, vec![]));
}
