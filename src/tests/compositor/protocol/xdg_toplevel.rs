//! Tests for `xdg_toplevel`: size hints, and who is entitled to start an
//! interactive move or resize.

use tokio_way_core::protocol::wire_utils::ArgWriter;
use tokio_way_core::protocol::xdg_toplevel::{SET_MAX_SIZE, SET_MIN_SIZE, grab_target, handle};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::{WaylandRequest, WaylandRequestWithClientInfo};

const CLIENT: u32 = 1;
const SURFACE: u32 = 10;
const XDG_SURFACE: u32 = 11;
const TOPLEVEL: u32 = 12;
const SERIAL: u32 = 77;
const LEFT_BUTTON: u32 = 0x110;

/// A mapped window, with a button held and a press serial on record — the
/// state a client is in when its title bar is being dragged.
fn state_mid_click() -> CompositorState {
    let mut state = crate::tests::test_state();
    state.create_surface(CLIENT, SURFACE);
    state.create_xdg_surface(CLIENT, XDG_SURFACE, SURFACE);
    state.create_xdg_toplevel(CLIENT, TOPLEVEL, XDG_SURFACE);
    state.pressed_buttons.insert(LEFT_BUTTON);
    state.last_button_serial.insert(CLIENT, SERIAL);
    state
}

/// Deliver a `set_min_size`/`set_max_size` request to a toplevel.
fn send_size_hint(state: &mut CompositorState, op_code: u16, width: i32, height: i32) {
    handle(
        state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: WaylandRequest {
                object_id: TOPLEVEL,
                op_code,
                args: ArgWriter::new().i32(width).i32(height).build(),
            },
        },
    );
}

#[test]
fn size_hints_are_recorded() {
    let mut state = state_mid_click();
    send_size_hint(&mut state, SET_MIN_SIZE, 300, 200);
    send_size_hint(&mut state, SET_MAX_SIZE, 800, 600);

    let toplevel = &state.xdg_toplevels[&(CLIENT, TOPLEVEL)];
    assert_eq!(toplevel.min_size, (300, 200));
    assert_eq!(toplevel.max_size, (800, 600));
}

#[test]
fn a_negative_size_hint_is_refused() {
    let mut state = state_mid_click();
    send_size_hint(&mut state, SET_MIN_SIZE, 300, 200);
    // A negative limit is a protocol error, and must not be stored: it
    // would poison every later resize of this window.
    send_size_hint(&mut state, SET_MIN_SIZE, -1, 200);
    assert_eq!(
        state.xdg_toplevels[&(CLIENT, TOPLEVEL)].min_size,
        (300, 200)
    );
}

#[test]
fn a_client_may_raise_both_limits_past_each_other() {
    let mut state = state_mid_click();
    send_size_hint(&mut state, SET_MIN_SIZE, 100, 100);
    send_size_hint(&mut state, SET_MAX_SIZE, 200, 200);
    // Raising the pair means the new min briefly exceeds the old max. That
    // is the client's own doing mid-update, not an error, so both stick.
    send_size_hint(&mut state, SET_MIN_SIZE, 300, 300);
    send_size_hint(&mut state, SET_MAX_SIZE, 400, 400);

    let toplevel = &state.xdg_toplevels[&(CLIENT, TOPLEVEL)];
    assert_eq!(toplevel.min_size, (300, 300));
    assert_eq!(toplevel.max_size, (400, 400));
}

#[test]
fn a_click_on_a_window_can_start_a_grab() {
    let state = state_mid_click();
    assert_eq!(
        grab_target(&state, CLIENT, TOPLEVEL, SERIAL),
        Some((CLIENT, SURFACE))
    );
}

#[test]
fn a_grab_needs_a_button_to_still_be_held() {
    let mut state = state_mid_click();
    state.pressed_buttons.clear();
    // The user has let go; there is no drag left to follow.
    assert_eq!(grab_target(&state, CLIENT, TOPLEVEL, SERIAL), None);
}

#[test]
fn a_grab_needs_a_serial_we_actually_sent() {
    let state = state_mid_click();
    // Otherwise any client could seize the pointer whenever it liked.
    assert_eq!(grab_target(&state, CLIENT, TOPLEVEL, SERIAL + 1), None);
}

#[test]
fn a_client_cannot_grab_another_clients_window() {
    let mut state = state_mid_click();
    state.last_button_serial.insert(2, SERIAL);
    // Client 2 quotes a serial it was given, but names client 1's toplevel.
    assert_eq!(grab_target(&state, 2, TOPLEVEL, SERIAL), None);
}
