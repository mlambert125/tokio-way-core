//! `zwp_primary_selection_device_manager_v1` protocol handler.
//!
//! The manager global for the middle-click clipboard: it makes the sources a
//! client offers a selection with, and the devices it is told about one on.
//!
//! Advertised beside `wl_data_device_manager` rather than folded into it
//! because the two selections are genuinely separate. Copying with Ctrl+C and
//! selecting text with the mouse set different things, a client reads them
//! through different objects, and neither disturbs the other.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{
    CompositorState, DataInterface, DataSource, DataSourceRole, PrimaryDeviceBinding,
};
use super::ObjectType;
use super::wire_utils::ArgReader;
use super::zwp_primary_selection_device;

pub const INTERFACE: &str = "zwp_primary_selection_device_manager_v1";
/// Version 1, which is every version this protocol has.
pub const VERSION: u32 = 1;

// Request opcodes
const CREATE_SOURCE: u16 = 0;
const GET_DEVICE: u16 = 1;
const DESTROY: u16 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        CREATE_SOURCE => handle_create_source(state, msg),
        GET_DEVICE => handle_get_device(state, msg),
        DESTROY => {
            // The manager is a factory and owns nothing: the sources and
            // devices it made outlive it, exactly as the protocol says.
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        _ => super::unknown_request(state, msg, INTERFACE),
    }
}

fn handle_create_source(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let Some(source_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "zwp_primary_selection_device_manager_v1.create_source: malformed args",
        );
        return;
    };

    debug!("{INTERFACE}.create_source: source_id={source_id}");

    if client
        .register(source_id, ObjectType::ZwpPrimarySelectionSource)
        .is_err()
    {
        return;
    }

    state.data_sources.insert(
        (msg.client_id, source_id),
        DataSource {
            interface: DataInterface::PrimarySelection,
            mime_types: Vec::new(),
            // Neither means anything here: actions belong to a drag, and this
            // interface has no drag. They are on the one source type because
            // the one map holds all three, and `wl_data_source` needs them.
            actions: 0,
            role: DataSourceRole::Unused,
            cancelled: false,
        },
    );
}

fn handle_get_device(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    // Read before the client is borrowed: the selection is global state and the
    // send below needs both.
    let focused_client = state.focused_surface.map(|(client_id, _)| client_id);

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // get_device args: new_id, object seat
    let (Some(device_id), Some(_seat_id)) = (args.new_id(), args.u32()) else {
        client.send_error(
            msg.message.object_id,
            0,
            "zwp_primary_selection_device_manager_v1.get_device: malformed args",
        );
        return;
    };

    debug!("{INTERFACE}.get_device: device_id={device_id}");

    // The seat is discarded: there is one seat, so there is nothing to
    // distinguish. It stays decoded rather than skipped so that a client
    // sending a short request is still caught as malformed.
    if client
        .register(device_id, ObjectType::ZwpPrimarySelectionDevice)
        .is_err()
    {
        return;
    }

    state.primary_devices.push(PrimaryDeviceBinding {
        client_id: msg.client_id,
        object_id: device_id,
    });

    // A client binds this well after its first window is focused, and the
    // selection is otherwise only sent on a focus change — so without this a
    // client that never loses and regains focus would find the middle-click
    // clipboard empty for the rest of its life.
    if focused_client == Some(msg.client_id) {
        zwp_primary_selection_device::send_selection_to_device(state, msg.client_id, device_id);
    }
}
