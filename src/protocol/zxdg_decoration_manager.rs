//! `zxdg_decoration_manager_v1` protocol handler.
//!
//! Lets a client ask whether it or the compositor should draw a toplevel's
//! window chrome. way-small draws none of its own, so there is nothing to
//! negotiate per window: the answer is fixed at startup by `client_side_decorations`
//! in the config file and handed out unchanged to every toplevel that asks —
//! see [`super::zxdg_toplevel_decoration::send_configure`].

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::ObjectType;
use super::wire_utils::ArgReader;

// Request opcodes
const DESTROY: u16 = 0;
const GET_TOPLEVEL_DECORATION: u16 = 1;

/// `zxdg_toplevel_decoration_v1.error.already_constructed`
const ERROR_ALREADY_CONSTRUCTED: u32 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        GET_TOPLEVEL_DECORATION => handle_get_toplevel_decoration(state, msg),
        _ => super::reject_unknown_request(state, msg, "zxdg_decoration_manager_v1"),
    }
}

fn handle_get_toplevel_decoration(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    // get_toplevel_decoration args: new_id id, object toplevel
    let (Some(decoration_id), Some(toplevel_id)) = (args.new_id(), args.u32()) else {
        client.send_error(
            msg.message.object_id,
            0,
            "zxdg_decoration_manager_v1.get_toplevel_decoration: malformed args",
        );
        return;
    };

    debug!(
        "zxdg_decoration_manager_v1.get_toplevel_decoration: decoration_id={} toplevel_id={}",
        decoration_id, toplevel_id
    );

    // Only one decoration object per toplevel — the error is on the object
    // this request would have created, per the protocol's own error enum.
    if state
        .toplevel_decoration
        .contains_key(&(msg.client_id, toplevel_id))
    {
        client.send_error(
            decoration_id,
            ERROR_ALREADY_CONSTRUCTED,
            "toplevel already has a decoration object",
        );
        return;
    }

    if client
        .register_client_object(decoration_id, ObjectType::ZxdgToplevelDecoration)
        .is_err()
    {
        return;
    }

    state.create_decoration(msg.client_id, decoration_id, toplevel_id);
    super::zxdg_toplevel_decoration::send_configure(state, msg.client_id, decoration_id);
}
