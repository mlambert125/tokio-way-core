//! Tests for `wl_registry.bind`, and for the claim a client makes with it.
//!
//! A bind carries both a global name and the interface the client believes
//! that name to be. Acting on the name alone and discarding the string is what
//! hands a confused client a working object of the wrong type, whose every
//! later request is then decoded against an interface nobody meant.

use super::{CLIENT, deliver, was_sent_an_error};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::{GLOBALS, ObjectType, WL_OUTPUT_INTERFACE, wire_utils::ArgWriter};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::WaylandEvent;

const REGISTRY: u32 = 2;
const NEW_ID: u32 = 50;
const BIND: u16 = 0;

/// The registry name of a static global, by interface.
fn name_of(interface: &str) -> u32 {
    let index = GLOBALS
        .iter()
        .position(|g| g.interface == interface)
        .expect("interface is not advertised");
    u32::try_from(index).unwrap()
}

fn version_of(interface: &str) -> u32 {
    GLOBALS
        .iter()
        .find(|g| g.interface == interface)
        .expect("interface is not advertised")
        .version
}

fn client_with_a_registry() -> (CompositorState, CancellationToken, Receiver<WaylandEvent>) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(CLIENT, tx, token.clone());
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(REGISTRY, ObjectType::WlRegistry)
        .unwrap();
    (state, token, rx)
}

fn bind(state: &mut CompositorState, name: u32, interface: &str, version: u32, new_id: u32) {
    deliver(
        state,
        REGISTRY,
        BIND,
        ArgWriter::new()
            .u32(name)
            .string(interface)
            .u32(version)
            .u32(new_id)
            .build(),
    );
}

#[test]
fn binding_a_global_under_the_wrong_interface_is_refused() {
    let (mut state, token, mut rx) = client_with_a_registry();

    // The name of `wl_compositor`, claimed as a `wl_seat`. The name is what
    // decides the type, so without the check the client would be handed a
    // working `wl_compositor` and would go on to send `wl_seat` requests at it.
    bind(&mut state, name_of("wl_compositor"), "wl_seat", 1, NEW_ID);

    assert!(was_sent_an_error(&mut rx), "the client must be told why");
    assert!(token.is_cancelled());
    assert!(
        !state
            .clients
            .get(CLIENT)
            .unwrap()
            .objects
            .contains_key(&NEW_ID),
        "and nothing must have been created under that id",
    );
}

#[test]
fn a_client_may_not_name_an_id_from_the_compositors_half() {
    let (mut state, token, mut rx) = client_with_a_registry();

    // The id space is split so neither side has to ask the other what is free.
    // An id above the line is one `allocate_id` will hand out later, quietly
    // replacing whatever the client put there — and `unregister` never
    // announces a server id as free, so the client would not be told either.
    bind(
        &mut state,
        name_of("wl_compositor"),
        "wl_compositor",
        1,
        tokio_way_core::state::SERVER_ID_BASE + 5,
    );

    assert!(was_sent_an_error(&mut rx), "the client must be told why");
    assert!(token.is_cancelled());
}

#[test]
fn binding_a_name_nothing_was_advertised_under_is_refused() {
    let (mut state, token, mut rx) = client_with_a_registry();

    bind(&mut state, 9999, "wl_compositor", 1, NEW_ID);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
}

#[test]
fn binding_version_zero_is_refused() {
    let (mut state, token, mut rx) = client_with_a_registry();

    // Nothing speaks version zero of anything. Clamping it rather than
    // refusing would leave every version-gated event on the object suppressed
    // for its whole life, which looks exactly like the feature being missing.
    bind(
        &mut state,
        name_of("wl_compositor"),
        "wl_compositor",
        0,
        NEW_ID,
    );

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
}

#[test]
fn a_matching_bind_is_still_accepted() {
    let (mut state, token, mut rx) = client_with_a_registry();

    bind(
        &mut state,
        name_of("wl_compositor"),
        "wl_compositor",
        version_of("wl_compositor"),
        NEW_ID,
    );

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert_eq!(
        state.clients.get(CLIENT).unwrap().objects.get(&NEW_ID),
        Some(&ObjectType::WlCompositor),
    );
}

#[test]
fn a_version_above_what_is_advertised_is_still_clamped_rather_than_refused() {
    let (mut state, token, mut rx) = client_with_a_registry();

    // Asking for more than the compositor has is ordinary: it is how a client
    // built against a newer protocol negotiates down. Only the interface and
    // a zero version are worth refusing over.
    bind(
        &mut state,
        name_of("wl_compositor"),
        "wl_compositor",
        version_of("wl_compositor") + 10,
        NEW_ID,
    );

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert_eq!(
        state.clients.get(CLIENT).unwrap().version(NEW_ID),
        version_of("wl_compositor"),
    );
}

#[test]
fn an_outputs_dynamic_global_is_checked_the_same_way() {
    let (mut state, token, mut rx) = client_with_a_registry();
    // An output global's name is handed out at runtime rather than being an
    // index into the static table, so it takes a different path to the same
    // check.
    let name = 100;
    state
        .output_global_names
        .insert(tokio_way_backends::outputs::OutputId(1), name);

    bind(&mut state, name, "wl_seat", 1, NEW_ID);
    assert!(was_sent_an_error(&mut rx), "not what was advertised there");
    assert!(token.is_cancelled());

    let (mut state, token, mut rx) = client_with_a_registry();
    state
        .output_global_names
        .insert(tokio_way_backends::outputs::OutputId(1), name);
    state.outputs.push(crate::tests::compositor::test_output(
        tokio_way_backends::outputs::OutputId(1),
    ));

    bind(&mut state, name, WL_OUTPUT_INTERFACE, 1, NEW_ID);
    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert_eq!(
        state.clients.get(CLIENT).unwrap().objects.get(&NEW_ID),
        Some(&ObjectType::WlOutput),
    );
}
