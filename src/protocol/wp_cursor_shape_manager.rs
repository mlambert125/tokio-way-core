//! `wp_cursor_shape_manager_v1` protocol handler.
//!
//! Hands out a [`super::wp_cursor_shape_device`] for a pointer, which is the
//! object a client names shapes through.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::ObjectType;
use super::wire_utils::ArgReader;

pub const INTERFACE: &str = "wp_cursor_shape_manager_v1";
/// Version 1. Version 2 adds two shapes — `dnd-ask` and `all-resize` — and this
/// advertises the version whose shapes it can actually draw, so that a client
/// asking for one of those is told `invalid_shape` rather than shown something
/// that is not what it asked for.
pub const VERSION: u32 = 1;

// Request opcodes. `destroy` leads, as in `wp_fractional_scale_manager_v1`.
const DESTROY: u16 = 0;
const GET_POINTER: u16 = 1;
const GET_TABLET_TOOL_V2: u16 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        DESTROY => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        GET_POINTER => handle_get_device(state, msg, "get_pointer"),
        // In version 1 of this interface from the start, so a client is entitled
        // to send it and the opcode cannot be refused as unknown. It can only
        // ever be sent with a `zwp_tablet_tool_v2`, and there is no tablet
        // protocol here for a client to have got one from — so the device is
        // created, because the client is owed the object it named, and nothing
        // will ever arrive on it.
        GET_TABLET_TOOL_V2 => handle_get_device(state, msg, "get_tablet_tool_v2"),
        _ => super::reject_unknown_request(state, msg, INTERFACE),
    }
}

/// Both constructors are the same shape — a new id and the input object it is
/// for — and neither keeps the second argument. There is one pointer and one
/// cursor, so which pointer a device names changes nothing; it stays decoded so
/// a short request is still caught as malformed.
fn handle_get_device(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    request: &str,
) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(device_id), Some(_input_object)) = (args.new_id(), args.u32()) else {
        super::reject_malformed_request(state, msg, INTERFACE);
        return;
    };

    debug!("{INTERFACE}.{request}: device_id={device_id}");

    let version = state
        .clients
        .get(msg.client_id)
        .map_or(1, |client| client.version(msg.message.object_id));
    if let Some(client) = state.clients.get(msg.client_id) {
        let _ = client.register_client_object_with_version(
            device_id,
            ObjectType::WpCursorShapeDevice,
            version,
        );
    }
}
