//! `wl_output` protocol handler.
//!
//! Advertises display output properties to clients: geometry, mode,
//! scale factor, and name. Each physical output gets its own `wl_output`
//! global; clients bind each one separately.

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::{ArgWriter, build_message};
use tokio_way_backends::outputs::OutputId;

// Request opcodes
const RELEASE: u16 = 0;

// Event opcodes
const GEOMETRY: u16 = 0;
const MODE: u16 = 1;
const DONE: u16 = 2;
const SCALE: u16 = 3;
const NAME: u16 = 4;
const DESCRIPTION: u16 = 5;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        RELEASE => {
            state
                .output_bindings
                .remove(&(msg.client_id, msg.message.object_id));
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        _ => super::reject_unknown_request(state, msg, "wl_output"),
    }
}

/// Send output info events for a specific output after a client binds its `wl_output` global.
pub fn send_output_info(
    state: &mut CompositorState,
    client_id: u32,
    obj_id: u32,
    target_id: OutputId,
) {
    // Clone the target output to avoid borrow conflict with clients.
    let Some(output) = state.outputs.iter().find(|o| o.id == target_id).cloned() else {
        return;
    };

    let Some(client) = state.clients.get(client_id) else {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    };
    let version = client.version(obj_id);

    // wl_output.geometry
    let args = ArgWriter::new()
        .i32(output.geometry.x)
        .i32(output.geometry.y)
        // Millimetres, not pixels — and this compositor does not know
        // them. Zero is the protocol's "unknown", which is what a
        // client's DPI guesswork should fall back from. Sending the pixel
        // count here told every client the display was nearly two metres
        // wide.
        .i32(0)
        .i32(0)
        .i32(output.geometry.subpixel as i32)
        .string(&output.geometry.make)
        .string(&output.geometry.model)
        .i32(output.geometry.transform as i32)
        .build();
    let _ = client.send(build_message(obj_id, GEOMETRY, args));

    // wl_output.mode (one event per mode)
    for mode in &output.modes {
        let args = ArgWriter::new()
            .u32(mode.flags)
            .i32(mode.width)
            .i32(mode.height)
            .i32(mode.refresh_mhz)
            .build();
        let _ = client.send(build_message(obj_id, MODE, args));
    }

    // wl_output.scale (version 2+). Integer only on the wire — a fractional
    // scale is rounded up, so a client that cannot do better still allocates a
    // buffer large enough; `wp_fractional_scale_v1` is where the exact value
    // would go.
    if version >= 2 {
        let args = ArgWriter::new().i32(output.scale.wl_output_scale()).build();
        let _ = client.send(build_message(obj_id, SCALE, args));

        // wl_output.name and wl_output.description (version 4+)
        if version >= 4 {
            let args = ArgWriter::new().string(&output.name).build();
            let _ = client.send(build_message(obj_id, NAME, args));

            let args = ArgWriter::new().string(&output.description).build();
            let _ = client.send(build_message(obj_id, DESCRIPTION, args));
        }

        // wl_output.done — sent once after all property events for this output.
        let _ = client.send(build_message(obj_id, DONE, Vec::new()));
    }
}

/// Send updated geometry + mode + done to all clients that have bound this output's `wl_output`.
pub fn broadcast_mode(state: &mut CompositorState) {
    // Clone outputs and collect bindings to avoid borrow conflicts.
    let outputs = state.outputs.clone();
    let bindings: Vec<((u32, u32), OutputId)> = state
        .output_bindings
        .iter()
        .map(|(&k, &v)| (k, v))
        .collect();

    for output in &outputs {
        for &((client_id, obj_id), output_id) in &bindings {
            if output_id != output.id {
                continue;
            }

            let Some(client) = state.clients.get(client_id) else {
                continue;
            };
            let version = client.version(obj_id);

            // wl_output.geometry (re-sent so clients see updated dimensions)
            let args = ArgWriter::new()
                .i32(output.geometry.x)
                .i32(output.geometry.y)
                // Millimetres, not pixels — and this compositor does not know
                // them. Zero is the protocol's "unknown", which is what a
                // client's DPI guesswork should fall back from. Sending the pixel
                // count here told every client the display was nearly two metres
                // wide.
                .i32(0)
                .i32(0)
                .i32(output.geometry.subpixel as i32)
                .string(&output.geometry.make)
                .string(&output.geometry.model)
                .i32(output.geometry.transform as i32)
                .build();
            let _ = client.send(build_message(obj_id, GEOMETRY, args));

            // All modes
            for mode in &output.modes {
                let args = ArgWriter::new()
                    .u32(mode.flags)
                    .i32(mode.width)
                    .i32(mode.height)
                    .i32(mode.refresh_mhz)
                    .build();
                let _ = client.send(build_message(obj_id, MODE, args));
            }

            // wl_output.done — once after all property events (version 2+)
            if version >= 2 {
                let _ = client.send(build_message(obj_id, DONE, Vec::new()));
            }
        }
    }
}
