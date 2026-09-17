//! `wl_seat` protocol handler.
//!
//! A seat represents a group of input devices (pointer, keyboard, touch).
//! Clients bind `wl_seat` to receive capability events, then request specific
//! input device objects (`wl_pointer`, `wl_keyboard`).

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};

// Request opcodes
const GET_POINTER: u16 = 0;
const GET_KEYBOARD: u16 = 1;
const GET_TOUCH: u16 = 2;
const RELEASE: u16 = 3;

// Event opcodes
pub const CAPABILITIES: u16 = 0;
pub const NAME: u16 = 1;

// Capability flags
const CAP_POINTER: u32 = 1;
const CAP_KEYBOARD: u32 = 2;
const CAP_TOUCH: u32 = 4;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        GET_POINTER => handle_get_pointer(state, msg),
        GET_KEYBOARD => handle_get_keyboard(state, msg),
        GET_TOUCH => handle_get_touch(state, msg),
        RELEASE => {
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(msg.message.object_id);
            }
        }
        _ => super::reject_unknown_request(state, msg, "wl_seat"),
    }
}

fn handle_get_pointer(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    let Some(pointer_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_seat.get_pointer: malformed args",
        );
        return;
    };

    debug!("wl_seat.get_pointer: pointer_id={}", pointer_id);

    let seat_version = client.version(msg.message.object_id);
    if client
        .register_client_object_with_version(pointer_id, ObjectType::WlPointer, seat_version)
        .is_err()
    {
        return;
    }
    state.pointers.push(super::super::state::PointerBinding {
        client_id: msg.client_id,
        object_id: pointer_id,
    });
}

/// Hand back a `wl_touch` that reports nothing.
///
/// There is no touch device, and the seat says so in its capabilities — but the
/// client names the id here, so the object still has to be registered. Dropping
/// the request instead leaves the client holding an id the compositor does not
/// know, and its eventual `release` would disconnect it. See [`super::wl_touch`].
fn handle_get_touch(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };
    let mut args = ArgReader::new(&msg.message.args);
    let Some(touch_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_seat.get_touch: malformed args",
        );
        return;
    };

    debug!("wl_seat.get_touch: touch_id={touch_id}");

    let seat_version = client.version(msg.message.object_id);
    if client
        .register_client_object_with_version(touch_id, ObjectType::WlTouch, seat_version)
        .is_err()
    {
        return;
    }
    state.touches.push(super::super::state::TouchBinding {
        client_id: msg.client_id,
        object_id: touch_id,
    });
}

fn handle_get_keyboard(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let Some(keyboard_id) = args.new_id() else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_seat.get_keyboard: malformed args",
        );
        return;
    };

    debug!("wl_seat.get_keyboard: keyboard_id={}", keyboard_id);

    let seat_version = client.version(msg.message.object_id);
    if client
        .register_client_object_with_version(keyboard_id, ObjectType::WlKeyboard, seat_version)
        .is_err()
    {
        return;
    }
    state.keyboards.push(super::super::state::KeyboardBinding {
        client_id: msg.client_id,
        object_id: keyboard_id,
    });

    // Send keymap to the client (required before any key events)
    super::wl_keyboard::send_keymap(state, msg.client_id, keyboard_id);

    // A toplevel takes focus the moment it is given the role (see
    // `xdg_surface::handle_get_toplevel`), which can be before the client
    // gets around to asking for a keyboard at all. Without this, a client
    // whose window is already focused by the time it calls `get_keyboard`
    // never receives the `enter` that tells it so — `switch_focus` already
    // ran and will not run again for a surface that is already focused — and
    // it has no correct reason to act on the `key`/`modifiers` events the
    // compositor still sends it. Catch this one up on what it missed.
    if let Some((focused_client, focused_surface)) = state.focused_surface
        && focused_client == msg.client_id
    {
        super::wl_keyboard::send_enter(state, msg.client_id, keyboard_id, focused_surface);
        // And the modifier state that goes with it. `enter` says which surface
        // has focus; without this the client is left assuming no modifier is
        // held, which is only true by accident.
        super::wl_keyboard::send_current_modifiers(state, msg.client_id, keyboard_id);
    }
}

/// Send `wl_seat.capabilities` and `wl_seat.name` after a client binds.
pub fn send_seat_info(state: &mut CompositorState, client_id: u32, seat_id: u32) {
    let Some(client) = state.clients.get(client_id) else {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    };
    let mut caps: u32 = 0;
    if state.seat.has_pointer {
        caps |= CAP_POINTER;
    }
    if state.seat.has_keyboard {
        caps |= CAP_KEYBOARD;
    }
    if state.seat.has_touch {
        caps |= CAP_TOUCH;
    }

    let args = ArgWriter::new().u32(caps).build();
    let _ = client.send(build_message(seat_id, CAPABILITIES, args));

    let args = ArgWriter::new().string("default").build();
    let _ = client.send(build_message(seat_id, NAME, args));
}

/// Tell every client that has bound the seat what it can now do.
///
/// Capabilities are not fixed for the life of the compositor: a touchscreen is
/// only known to exist once it is touched, and a client that bound the seat
/// before that would otherwise never hear about it.
pub fn broadcast_capabilities(state: &mut CompositorState) {
    let seats: Vec<(u32, u32)> = state
        .clients
        .iter()
        .flat_map(|(client_id, client)| {
            let client_id = *client_id;
            client
                .objects
                .iter()
                .filter(|(_, object_type)| **object_type == ObjectType::WlSeat)
                .map(move |(object_id, _)| (client_id, *object_id))
        })
        .collect();
    for (client_id, seat_id) in seats {
        send_seat_info(state, client_id, seat_id);
    }
}
