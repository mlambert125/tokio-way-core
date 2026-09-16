//! `wl_keyboard` protocol handler.
//!
//! Delivers keyboard events to focused clients: keymap, enter, leave,
//! key, and modifiers events.

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::io::FromRawFd;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::next_serial;
use super::wire_utils::{ArgWriter, build_message};
use tokio_way_sock::WaylandEvent;

// Request opcodes
const RELEASE: u16 = 0;

// Event opcodes
pub const KEYMAP: u16 = 0;
pub const ENTER: u16 = 1;
pub const LEAVE: u16 = 2;
pub const KEY: u16 = 3;
pub const MODIFIERS: u16 = 4;
pub const REPEAT_INFO: u16 = 5;

// Keymap format
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        RELEASE => {
            let keyboard_id = msg.message.object_id;
            state
                .keyboards
                .retain(|k| !(k.client_id == msg.client_id && k.object_id == keyboard_id));
            if let Some(client) = state.clients.get(msg.client_id) {
                client.unregister(keyboard_id);
            }
        }
        _ => super::unknown_request(state, msg, "wl_keyboard"),
    }
}

/// Send the XKB keymap to a client's keyboard object via an FD.
pub fn send_keymap(state: &mut CompositorState, client_id: u32, keyboard_id: u32) {
    // The one keymap the compositor compiled — the same one its modifier state
    // is read from, so what a client is told and what it is sent agree.
    let Some(keymap_str) = state.keyboard.keymap_string() else {
        tracing::warn!("No xkb keymap to send");
        return;
    };
    let Some(client) = state.clients.get(client_id) else {
        tracing::warn!("Received message from unknown client {}", client_id);
        return;
    };

    let keymap_size = keymap_str.len() + 1; // include null terminator

    // Write keymap to a memfd
    // MFD_CLOEXEC so the keymap does not leak into any process we later spawn.
    let fd = unsafe { libc::memfd_create(c"keymap".as_ptr().cast(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        tracing::warn!("Failed to create memfd for keymap");
        return;
    }

    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    if file.write_all(keymap_str.as_bytes()).is_err() || file.write_all(&[0]).is_err() {
        tracing::warn!("Failed to write keymap to memfd");
        return;
    }

    unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) };

    let args = ArgWriter::new()
        .u32(KEYMAP_FORMAT_XKB_V1)
        .u32(u32::try_from(keymap_size).expect("Keymap size should be < u32::MAX"))
        .build();

    let msg = WaylandEvent {
        object_id: keyboard_id,
        op_code: KEYMAP,
        args,
        // The message owns the memfd from here on. `SCM_RIGHTS` duplicates it
        // into the client, so our copy still has to be closed afterwards —
        // dropping the message does that on every path, sent or not.
        fds: vec![OwnedFd::from(file)],
    };

    if client.send(msg).is_err() {
        return;
    }

    // Send repeat_info (version 4+): rate=25 keys/sec, delay=600ms
    if client.version(keyboard_id) >= 4 {
        let args = ArgWriter::new().i32(25).i32(600).build();
        let _ = client.send(build_message(keyboard_id, REPEAT_INFO, args));
    }
}

/// Send `wl_keyboard`.enter to a client's keyboard object.
pub fn send_enter(state: &mut CompositorState, client_id: u32, keyboard_id: u32, surface_id: u32) {
    let serial = next_serial();
    state.record_input_serial(client_id, serial);
    // The keys currently held, as a `wl_array` of evdev keycodes.
    let keys: Vec<u32> = state.pressed_keys.iter().copied().collect();
    let args = ArgWriter::new()
        .u32(serial)
        .u32(surface_id)
        .array_u32(&keys)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(keyboard_id, ENTER, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `wl_keyboard.leave` to a client's keyboard object.
pub fn send_leave(state: &mut CompositorState, client_id: u32, keyboard_id: u32, surface_id: u32) {
    let serial = next_serial();
    let args = ArgWriter::new().u32(serial).u32(surface_id).build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(keyboard_id, LEAVE, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send `wl_keyboard.key` to a client's keyboard object.
pub fn send_key(
    state: &mut CompositorState,
    client_id: u32,
    keyboard_id: u32,
    time_ms: u32,
    key: u32,
    pressed: bool,
) {
    let serial = next_serial();
    // This is the one that makes Ctrl+C work: a client setting the clipboard
    // quotes the serial of the key press that asked for it.
    state.record_input_serial(client_id, serial);
    let key_state: u32 = u32::from(pressed); // 1 for pressed, 0 for released
    let args = ArgWriter::new()
        .u32(serial)
        .u32(time_ms)
        .u32(key)
        .u32(key_state)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(keyboard_id, KEY, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}

/// Send the compositor's current modifier state to one keyboard object.
///
/// The protocol owes a client `modifiers` whenever what it believes could have
/// drifted from the truth, and that is not only when a modifier key moves: a
/// client that gains focus, or that binds a keyboard for a surface that
/// already has it, has been told nothing at all and so believes nothing is
/// held. Focusing a window with Ctrl down and having its next click arrive as
/// a plain click is what that looks like from the outside.
pub fn send_current_modifiers(state: &mut CompositorState, client_id: u32, keyboard_id: u32) {
    let modifiers = state.modifiers;
    send_modifiers(
        state,
        client_id,
        keyboard_id,
        modifiers.depressed,
        modifiers.latched,
        modifiers.locked,
        modifiers.group,
    );
}

/// Send `wl_keyboard.modifiers` to a client's keyboard object.
pub fn send_modifiers(
    state: &mut CompositorState,
    client_id: u32,
    keyboard_id: u32,
    mods_depressed: u32,
    mods_latched: u32,
    mods_locked: u32,
    group: u32,
) {
    let serial = next_serial();
    let args = ArgWriter::new()
        .u32(serial)
        .u32(mods_depressed)
        .u32(mods_latched)
        .u32(mods_locked)
        .u32(group)
        .build();
    if let Some(client) = state.clients.get(client_id) {
        let _ = client.send(build_message(keyboard_id, MODIFIERS, args));
    } else {
        tracing::warn!("Received message from unknown client {}", client_id);
    }
}
