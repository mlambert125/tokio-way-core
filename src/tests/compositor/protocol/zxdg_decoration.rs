//! Tests for `zxdg_decoration_manager_v1`/`zxdg_toplevel_decoration_v1`: the
//! compositor answers from config, not from what the client asks for.

use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::wire_utils::ArgWriter;
use tokio_way_core::protocol::{zxdg_decoration_manager, zxdg_toplevel_decoration};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::{WaylandEvent, WaylandRequest, WaylandRequestWithClientInfo};

const CLIENT: u32 = 1;
const MANAGER: u32 = 5;
const TOPLEVEL: u32 = 12;
const DECORATION: u32 = 13;

/// A connected client with the decoration manager object already bound.
fn client_with_manager() -> (CompositorState, Receiver<WaylandEvent>) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(64);
    state.clients.create(CLIENT, tx, CancellationToken::new());
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object(
            MANAGER,
            tokio_way_core::protocol::ObjectType::ZxdgDecorationManager,
        )
        .unwrap();
    (state, rx)
}

fn get_toplevel_decoration(state: &mut CompositorState, decoration_id: u32, toplevel_id: u32) {
    zxdg_decoration_manager::handle(
        state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: WaylandRequest {
                object_id: MANAGER,
                op_code: 1, // get_toplevel_decoration
                args: ArgWriter::new().u32(decoration_id).u32(toplevel_id).build(),
            },
        },
    );
}

/// The `mode` argument of the last `configure` event sent to `decoration_id`.
fn last_configured_mode(rx: &mut Receiver<WaylandEvent>, decoration_id: u32) -> u32 {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|m| m.object_id == decoration_id && m.op_code == 0)
        .last()
        .map(|m| u32::from_le_bytes(m.args[..4].try_into().unwrap()))
        .expect("decoration object should have been sent a configure")
}

#[test]
fn client_side_is_the_default() {
    let (mut state, mut rx) = client_with_manager();
    get_toplevel_decoration(&mut state, DECORATION, TOPLEVEL);

    assert_eq!(last_configured_mode(&mut rx, DECORATION), 1); // client_side
}

#[test]
fn config_can_ask_for_server_side() {
    let (mut state, mut rx) = client_with_manager();
    state.settings.client_side_decorations = false;
    get_toplevel_decoration(&mut state, DECORATION, TOPLEVEL);

    assert_eq!(last_configured_mode(&mut rx, DECORATION), 2); // server_side
}

#[test]
fn a_second_decoration_on_the_same_toplevel_is_refused() {
    let (mut state, mut rx) = client_with_manager();
    get_toplevel_decoration(&mut state, DECORATION, TOPLEVEL);
    std::iter::from_fn(|| rx.try_recv().ok()).for_each(drop);

    let second = DECORATION + 1;
    get_toplevel_decoration(&mut state, second, TOPLEVEL);

    // Refused with an error naming the object the request would have
    // created, not registered as a live object.
    assert!(
        !state.decorations.contains_key(&(CLIENT, second)),
        "the second decoration must not have been created"
    );
    let named_object = std::iter::from_fn(|| rx.try_recv().ok())
        .find(|m| {
            m.object_id == tokio_way_core::protocol::wl_display::OBJECT_ID
                && m.op_code == tokio_way_core::protocol::wl_display::ERROR
        })
        .map(|m| u32::from_le_bytes(m.args[..4].try_into().unwrap()));
    assert_eq!(
        named_object,
        Some(second),
        "the client should hear why its second request failed"
    );
}

#[test]
fn set_mode_does_not_change_the_compositors_answer() {
    let (mut state, mut rx) = client_with_manager();
    get_toplevel_decoration(&mut state, DECORATION, TOPLEVEL);
    std::iter::from_fn(|| rx.try_recv().ok()).for_each(drop);

    // The client asks for server-side; the compositor's config says
    // otherwise, and its answer does not move.
    zxdg_toplevel_decoration::handle(
        &mut state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: WaylandRequest {
                object_id: DECORATION,
                op_code: 1, // set_mode
                args: ArgWriter::new().u32(2).build(),
            },
        },
    );

    assert_eq!(last_configured_mode(&mut rx, DECORATION), 1); // still client_side
}

#[test]
fn destroying_the_toplevel_frees_its_decoration_slot() {
    let mut state = crate::tests::test_state();
    state.create_surface(CLIENT, 10);
    state.create_xdg_surface(CLIENT, 11, 10);
    state.create_xdg_toplevel(CLIENT, TOPLEVEL, 11);
    state.create_decoration(CLIENT, DECORATION, TOPLEVEL);

    state.destroy_xdg_toplevel(CLIENT, TOPLEVEL);

    assert!(!state.toplevel_decoration.contains_key(&(CLIENT, TOPLEVEL)));
    assert!(!state.decorations.contains_key(&(CLIENT, DECORATION)));
}
