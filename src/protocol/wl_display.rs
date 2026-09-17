//! `wl_display` protocol handler (object id 1).
//!
//! Handles the two core requests every client makes first:
//! - sync: creates a transient `wl_callback`, fires done event, then deletes it
//! - `get_registry`: creates a `wl_registry` and advertises all globals

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::{ArgReader, ArgWriter, ObjectType, build_message, next_serial, wl_registry};

// The wl_display global is always object id 1 for every client.
pub const OBJECT_ID: u32 = 1;

// Request opcodes
const SYNC: u16 = 0;
const GET_REGISTRY: u16 = 1;

// Event opcodes
pub const ERROR: u16 = 0;
pub const DELETE_ID: u16 = 1;

// wl_callback event opcodes
const WL_CALLBACK_DONE: u16 = 0;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        SYNC => {
            let Some(client) = state.clients.get(msg.client_id) else {
                return;
            };
            handle_sync(client, msg);
        }
        GET_REGISTRY => handle_get_registry(state, msg),
        _ => super::reject_unknown_request(state, msg, "wl_display"),
    }
}

fn handle_sync(state: &mut super::super::state::ClientState, msg: &WaylandRequestWithClientInfo) {
    let Some(callback_id) = ArgReader::new(&msg.message.args).new_id() else {
        state.send_error(OBJECT_ID, 0, "wl_display.sync: missing callback id");
        return;
    };
    debug!("wl_display.sync -> callback_id={}", callback_id);

    if state
        .register_client_object(callback_id, ObjectType::WlCallback)
        .is_err()
    {
        return;
    }

    let serial = next_serial();
    let args = ArgWriter::new().u32(serial).build();
    if state
        .send(build_message(callback_id, WL_CALLBACK_DONE, args))
        .is_err()
    {
        return;
    }

    state.unregister(callback_id);
}

fn handle_get_registry(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        return;
    };

    let Some(registry_id) = ArgReader::new(&msg.message.args).new_id() else {
        client.send_error(OBJECT_ID, 0, "wl_display.get_registry: missing registry id");
        return;
    };
    if client
        .register_client_object(registry_id, ObjectType::WlRegistry)
        .is_err()
    {
        return;
    }
    debug!("wl_display.get_registry -> registry_id={}", registry_id);

    wl_registry::advertise_globals(state, msg.client_id, registry_id);
}
