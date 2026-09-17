//! `zwlr_data_control_manager_v1` protocol handler.
//!
//! The manager global a clipboard manager binds: it makes the sources such a
//! client sets a selection with, and the devices it is told about every
//! selection on.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::{
    CompositorState, DataControlDeviceBinding, DataInterface, DataSource, DataSourceRole,
};
use super::ObjectType;
use super::wire_utils::ArgReader;
use super::zwlr_data_control_device;

pub const INTERFACE: &str = "zwlr_data_control_manager_v1";
/// Version 2, which adds the primary selection half of the device interface.
/// Everything in it is implemented, so there is nothing to hold back for.
pub const VERSION: u32 = 2;

// Request opcodes
const CREATE_DATA_SOURCE: u16 = 0;
const GET_DATA_DEVICE: u16 = 1;
const DESTROY: u16 = 2;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        CREATE_DATA_SOURCE => handle_create_data_source(state, msg),
        GET_DATA_DEVICE => handle_get_data_device(state, msg),
        DESTROY => {
            // A factory owning nothing: the devices and sources it made outlive
            // it, which the protocol says in as many words.
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        _ => super::reject_unknown_request(state, msg, INTERFACE),
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
            "zwlr_data_control_manager_v1.create_data_source: malformed args",
        );
        return;
    };

    debug!("{INTERFACE}.create_data_source: source_id={source_id}");

    let version = client.version(msg.message.object_id);
    if client
        .register_client_object_with_version(source_id, ObjectType::ZwlrDataControlSource, version)
        .is_err()
    {
        return;
    }

    state.data_sources.insert(
        (msg.client_id, source_id),
        DataSource {
            interface: DataInterface::DataControl,
            mime_types: Vec::new(),
            // No drag reaches this interface, so neither is ever read. See the
            // note on the same two fields in the primary selection's manager.
            actions: 0,
            role: DataSourceRole::Unused,
            cancelled: false,
        },
    );
}

fn handle_get_data_device(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
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
            "zwlr_data_control_manager_v1.get_data_device: malformed args",
        );
        return;
    };

    debug!("{INTERFACE}.get_data_device: device_id={device_id}");

    // The manager's version carried down, because the device's primary
    // selection half is gated on it: a device left at version 1 would never be
    // told about the primary selection at all.
    let version = client.version(msg.message.object_id);
    if client
        .register_client_object_with_version(device_id, ObjectType::ZwlrDataControlDevice, version)
        .is_err()
    {
        return;
    }

    state.data_control_devices.push(DataControlDeviceBinding {
        client_id: msg.client_id,
        object_id: device_id,
    });

    // At once, and both selections. A manager is started with the session and
    // binds long after the first copy; unlike a `wl_data_device`, no focus
    // change will come along later to tell it what it missed.
    zwlr_data_control_device::send_current_selections(state, msg.client_id, device_id);
}
