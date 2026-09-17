//! `wl_pointer` protocol handler.
//!
//! Delivers pointer (mouse) events to focused clients: enter, leave,
//! motion, button, and axis (scroll) events.

use crate::protocol::ArgReader;
use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::next_serial;
use super::wire_utils::{ArgWriter, build_message};
use tokio_way_backends::input::ScrollSource;

// Request opcodes
const SET_CURSOR: u16 = 0;
const RELEASE: u16 = 1;

// Event opcodes
pub const ENTER: u16 = 0;
pub const LEAVE: u16 = 1;
pub const MOTION: u16 = 2;
pub const BUTTON: u16 = 3;
pub const AXIS: u16 = 4;
pub const FRAME: u16 = 5;
pub const AXIS_SOURCE: u16 = 6;
pub const AXIS_STOP: u16 = 7;
pub const AXIS_DISCRETE: u16 = 8;
pub const AXIS_VALUE120: u16 = 9;

/// `wl_pointer.axis` values.
pub const AXIS_VERTICAL: u32 = 0;
pub const AXIS_HORIZONTAL: u32 = 1;

/// `wl_pointer.axis_source` values.
const AXIS_SOURCE_WHEEL: u32 = 0;
const AXIS_SOURCE_FINGER: u32 = 1;

/// The version at which the axis detail events appear.
const AXIS_DETAIL_SINCE: u32 = 5;
/// The version at which `axis_value120` replaces `axis_discrete`.
const AXIS_VALUE120_SINCE: u32 = 8;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        SET_CURSOR => {
            process_set_cursor(state, msg);
        }
        RELEASE => {
            let pointer_id = msg.message.object_id;
            state
                .pointers
                .retain(|p| !(p.client_id == msg.client_id && p.object_id == pointer_id));
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(pointer_id);
            } else {
                tracing::warn!("Received message from unknown client {}", msg.client_id);
            }
        }
        _ => super::reject_unknown_request(state, msg, "wl_pointer"),
    }
}

fn process_set_cursor(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let (Some(serial), Some(surface_id), Some(hotspot_x), Some(hotspot_y)) =
        (args.u32(), args.u32(), args.i32(), args.i32())
    else {
        super::reject_malformed_request(state, msg, "wl_pointer");
        return;
    };

    let client_id = msg.client_id;

    // Validate serial matches the most recent enter serial for this client.
    if state.pointer_enter_serial.get(&client_id) != Some(&serial) {
        return;
    }

    // Whichever way the cursor was set last wins, so this clears any shape the
    // client named through `wp_cursor_shape_device_v1` — the two are one choice
    // made two ways, and leaving both set would make which one shows depend on
    // the order the scene happened to look in.
    state.cursor_shapes.remove(&client_id);

    // surface_id == 0 means hide cursor.
    if surface_id == 0 {
        state.cursor_surfaces.insert(client_id, None);
        state.cursor_dirty = true;
        return;
    }

    let surface_key = (client_id, surface_id);

    // Check the surface exists.
    if !state.surfaces.contains_key(&surface_key) {
        return;
    }

    // Role check: surface must not already have another role (subsurface,
    // xdg_surface, or drag icon).
    let is_subsurface = state
        .surfaces
        .get(&surface_key)
        .is_some_and(|s| s.parent.is_some());
    let is_xdg = state
        .xdg_surfaces
        .values()
        .any(|x| x.client_id == client_id && x.wl_surface_id == surface_id);
    let is_dnd_icon = state.dnd_icon_surfaces.contains(&surface_key);

    if is_subsurface || is_xdg || is_dnd_icon {
        return;
    }

    // Assign cursor role (permanent).
    state.cursor_role_surfaces.insert(surface_key);

    state
        .cursor_surfaces
        .insert(client_id, Some((surface_id, hotspot_x, hotspot_y)));
    state.cursor_dirty = true;
}

/// Send `wl_pointer.enter` to a client's pointer object.
pub fn send_enter(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    surface_id: u32,
    x: f64,
    y: f64,
) {
    let serial = next_serial();
    state.pointer_enter_serial.insert(client_id, serial);
    state.record_input_serial(client_id, serial);
    let args = ArgWriter::new()
        .u32(serial)
        .u32(surface_id)
        .fixed(x)
        .fixed(y)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(pointer_id, ENTER, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `wl_pointer.leave` to a client's pointer object.
pub fn send_leave(state: &mut CompositorState, client_id: u32, pointer_id: u32, surface_id: u32) {
    let serial = next_serial();
    let args = ArgWriter::new().u32(serial).u32(surface_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(pointer_id, LEAVE, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `wl_pointer.motion` to a client's pointer object.
pub fn send_motion(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    x: f64,
    y: f64,
) {
    let args = ArgWriter::new().u32(time_ms).fixed(x).fixed(y).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(pointer_id, MOTION, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `wl_pointer.button` to a client's pointer object.
///
/// Returns the serial, which the compositor records so a client can quote it
/// when asking to start an interactive move or resize.
pub fn send_button(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    button: u32,
    pressed: bool,
) -> u32 {
    let serial = next_serial();
    // Recorded on release as well as press: a client may set the selection on
    // button-up, and quoting the serial of the event it is handling is exactly
    // what it is supposed to do.
    state.record_input_serial(client_id, serial);
    let btn_state: u32 = u32::from(pressed); // 1 for pressed, 0 for released
    let args = ArgWriter::new()
        .u32(serial)
        .u32(time_ms)
        .u32(button)
        .u32(btn_state)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(pointer_id, BUTTON, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
    serial
}

/// Send `wl_pointer.axis` to a client's pointer object.
pub fn send_axis(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    axis: u32,
    value: f64,
) {
    let args = ArgWriter::new().u32(time_ms).u32(axis).fixed(value).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(pointer_id, AXIS, args));
    }
}

/// Send `wl_pointer.frame` to indicate end of a group of events (version 5+).
pub fn send_frame(state: &mut CompositorState, client_id: u32, pointer_id: u32) {
    if let Some(client) = state.clients.get(client_id)
        && client.version(pointer_id) >= 5
    {
        let _ = client.send(build_message(pointer_id, FRAME, Vec::new()));
    }
}

/// Send `wl_pointer.axis_source`, saying what did the scrolling.
///
/// Goes out once per frame, before the axis events it describes. A client uses
/// it to decide whether the scroll can have momentum: a wheel clicks and stops,
/// a touchpad glides and is let go of.
pub fn send_axis_source(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    source: ScrollSource,
) {
    let value = match source {
        ScrollSource::Wheel => AXIS_SOURCE_WHEEL,
        ScrollSource::Finger => AXIS_SOURCE_FINGER,
    };
    let args = ArgWriter::new().u32(value).build();
    if let Some(client) = state.clients.get(client_id)
        && client.version(pointer_id) >= AXIS_DETAIL_SINCE
    {
        let _ = client.send(build_message(pointer_id, AXIS_SOURCE, args));
    }
}

/// Send `wl_pointer.axis_stop`: this axis has stopped scrolling.
///
/// Only meaningful for a source that can stop. A client cannot tell a scroll
/// that has paused from one that has ended by watching the deltas, and kinetic
/// scrolling turns on exactly that difference.
pub fn send_axis_stop(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    time_ms: u32,
    axis: u32,
) {
    let args = ArgWriter::new().u32(time_ms).u32(axis).build();
    if let Some(client) = state.clients.get(client_id)
        && client.version(pointer_id) >= AXIS_DETAIL_SINCE
    {
        let _ = client.send(build_message(pointer_id, AXIS_STOP, args));
    }
}

/// Tell a client how far a wheel clicked, in whichever unit its version speaks.
///
/// The two events are alternatives, not companions: version 8 replaced
/// `axis_discrete` with `axis_value120`, and sending both would have a client
/// that understands the newer one count every detent twice. So the version
/// decides which goes out, and exactly one does.
///
/// `v120` is 120ths of a detent, which is what lets a high-resolution wheel
/// report a fraction of a click without a separate event for it. An older
/// client is sent whole detents, and a movement smaller than one rounds to
/// nothing — which is the best that unit can say.
pub fn send_axis_steps(
    state: &mut CompositorState,
    client_id: u32,
    pointer_id: u32,
    axis: u32,
    v120: i32,
) {
    if v120 == 0 {
        return;
    }
    let Some(version) = state.clients.version_of(client_id, pointer_id) else {
        return;
    };
    if version < AXIS_DETAIL_SINCE {
        return;
    }

    let (op_code, args) = if version >= AXIS_VALUE120_SINCE {
        (AXIS_VALUE120, ArgWriter::new().u32(axis).i32(v120).build())
    } else {
        // Whole detents only, and a partial one is silence rather than zero.
        // `axis_discrete` counts clicks of a wheel, so a zero is a statement
        // that the wheel moved by no clicks — which is not something that
        // happens. A high-resolution wheel between detents has genuinely
        // nothing to report to a client that can only count them; the `axis`
        // event alongside still carries the distance.
        let detents = v120 / 120;
        if detents == 0 {
            return;
        }
        (
            AXIS_DISCRETE,
            ArgWriter::new().u32(axis).i32(detents).build(),
        )
    };
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(pointer_id, op_code, args));
    }
}
