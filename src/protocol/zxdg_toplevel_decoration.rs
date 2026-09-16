//! `zxdg_toplevel_decoration_v1` protocol handler.
//!
//! One object per toplevel, created by `zxdg_decoration_manager_v1`. The mode
//! is decided outright by config, not negotiated — `set_mode`/`unset_mode`
//! are accepted but do not change the answer, matching the protocol's own
//! allowance for the compositor to enforce whatever mode it prefers.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::{ArgWriter, build_message};

// Request opcodes
const DESTROY: u16 = 0;
const SET_MODE: u16 = 1;
const UNSET_MODE: u16 = 2;

// Event opcodes
const CONFIGURE: u16 = 0;

// `zxdg_toplevel_decoration_v1.mode`
const MODE_CLIENT_SIDE: u32 = 1;
const MODE_SERVER_SIDE: u32 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        // The answer never depends on what is asked for — see
        // `send_configure` — so both just re-confirm the compositor's choice.
        SET_MODE | UNSET_MODE => {
            send_configure(state, msg.client_id, msg.message.object_id);
        }
        _ => super::unknown_request(state, msg, "zxdg_toplevel_decoration_v1"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let decoration_id = msg.message.object_id;
    debug!(
        "zxdg_toplevel_decoration_v1.destroy: decoration_id={}",
        decoration_id
    );
    state.destroy_decoration(msg.client_id, decoration_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(decoration_id);
    }
}

/// Tell the client which mode is in effect: client-side unless
/// `client_side_decorations = false` in the config file.
///
/// Sent once when the decoration object is created, and again whenever the
/// client asks — never anything but this compositor-wide, config-fixed
/// answer, since nothing here varies per toplevel.
pub fn send_configure(state: &mut CompositorState, client_id: u32, decoration_id: u32) {
    let mode = if state.settings.client_side_decorations {
        MODE_CLIENT_SIDE
    } else {
        MODE_SERVER_SIDE
    };
    let args = ArgWriter::new().u32(mode).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(decoration_id, CONFIGURE, args));
    }
}
