//! `wl_data_device_manager` protocol handler.
//!
//! The data device manager global lets clients create data sources (content
//! they can produce, for the clipboard or a drag) and get data devices (where
//! they are told what the clipboard holds and what is being dragged over them).

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::super::state::{DataDeviceBinding, DataInterface, DataSource, DataSourceRole};
use super::ObjectType;
use super::wire_utils::ArgReader;
use super::wl_data_device;

// Request opcodes
const CREATE_DATA_SOURCE: u16 = 0;
const GET_DATA_DEVICE: u16 = 1;

/// `wl_data_device_manager.dnd_action`, the actions a drag can settle on.
///
/// Kept here because this is the interface that defines the enum, though both
/// `wl_data_source` and `wl_data_offer` negotiate with it.
pub const DND_ACTION_NONE: u32 = 0;
pub const DND_ACTION_COPY: u32 = 1;
pub const DND_ACTION_MOVE: u32 = 2;
pub const DND_ACTION_ASK: u32 = 4;
/// Every action the protocol defines. A mask with anything else in it is a
/// client out of step with the interface, and is an error on both sides.
pub const DND_ACTION_ALL: u32 = DND_ACTION_COPY | DND_ACTION_MOVE | DND_ACTION_ASK;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        CREATE_DATA_SOURCE => handle_create_data_source(state, msg),
        GET_DATA_DEVICE => handle_get_data_device(state, msg),
        _ => super::unknown_request(state, msg, "wl_data_device_manager"),
    }
}

fn handle_create_data_source(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let Some(source_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_data_device_manager.create_data_source: malformed args",
        );
        return;
    };

    debug!(
        "wl_data_device_manager.create_data_source: source_id={}",
        source_id
    );

    // The manager's version, carried down to the object it creates. Registering
    // without it would leave the source at version 1, where every one of the
    // drag-and-drop action events is version-gated off — the negotiation would
    // run and none of its results would ever reach the client.
    let version = client.version(msg.message.object_id);
    if client
        .register_with_version(source_id, ObjectType::WlDataSource, version)
        .is_err()
    {
        return;
    }

    state.data_sources.insert(
        (msg.client_id, source_id),
        DataSource {
            interface: DataInterface::WlData,
            mime_types: Vec::new(),
            actions: 0,
            role: DataSourceRole::Unused,
            cancelled: false,
        },
    );
}

fn handle_get_data_device(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    // Read before the client is borrowed: the selection is global state and the
    // send below needs both.
    let focused_client = state.focused_surface.map(|(client_id, _)| client_id);

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // get_data_device args: new_id, object seat
    let (Some(device_id), Some(_seat_id)) = (args.new_id(), args.u32()) else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_data_device_manager.get_data_device: malformed args",
        );
        return;
    };

    debug!(
        "wl_data_device_manager.get_data_device: device_id={}",
        device_id
    );

    // The seat is discarded: there is one seat, so there is nothing to
    // distinguish. It stays decoded rather than skipped so that a client
    // sending a short request is still caught as malformed.
    let version = client.version(msg.message.object_id);
    if client
        .register_with_version(device_id, ObjectType::WlDataDevice, version)
        .is_err()
    {
        return;
    }

    state.data_devices.push(DataDeviceBinding {
        client_id: msg.client_id,
        object_id: device_id,
    });

    // A client usually gets its data device well after its first window is
    // focused, and the selection is otherwise only sent on a focus change — so
    // without this a client that never loses and regains focus would have an
    // empty clipboard for the rest of its life. The same shape `wl_seat`
    // already uses to send the keymap the moment a keyboard exists.
    if focused_client == Some(msg.client_id) {
        wl_data_device::send_selection_to_device(state, msg.client_id, device_id);
    }
}
