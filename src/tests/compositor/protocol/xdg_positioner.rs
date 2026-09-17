//! Tests for `xdg_positioner`'s enum arguments.
//!
//! `anchor` and `gravity` arrive as bare `u32`s and only nine values name
//! either. A value outside that range is a client fault with an error of its
//! own, and must not be one the compositor takes personally.

use super::{CLIENT, POSITIONER, deliver, was_sent_an_error};
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::{ObjectType, wire_utils::ArgWriter};
use tokio_way_core::state::{CompositorState, XdgPositionerAnchor, XdgPositionerGravity};
use tokio_way_sock::WaylandEvent;

const SET_ANCHOR: u16 = 3;
const SET_GRAVITY: u16 = 4;

/// The largest value either enum defines. `bottom_right` is 8 in both.
const LAST_VALID: u32 = 8;

fn client_with_a_positioner() -> (CompositorState, CancellationToken, Receiver<WaylandEvent>) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(CLIENT, tx, token.clone());
    state.create_xdg_positioner(CLIENT, POSITIONER);
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object(POSITIONER, ObjectType::XdgPositioner)
        .unwrap();
    (state, token, rx)
}

#[test]
fn an_anchor_the_protocol_does_not_define_is_refused() {
    let (mut state, token, mut rx) = client_with_a_positioner();

    deliver(
        &mut state,
        POSITIONER,
        SET_ANCHOR,
        ArgWriter::new().u32(LAST_VALID + 1).build(),
    );

    assert!(was_sent_an_error(&mut rx), "the client must be told why");
    assert!(token.is_cancelled());
    assert!(
        matches!(
            state.xdg_positioners[&(CLIENT, POSITIONER)].anchor,
            XdgPositionerAnchor::None
        ),
        "and the positioner keeps the anchor it had",
    );
}

#[test]
fn a_gravity_the_protocol_does_not_define_is_refused() {
    let (mut state, token, mut rx) = client_with_a_positioner();

    deliver(
        &mut state,
        POSITIONER,
        SET_GRAVITY,
        ArgWriter::new().u32(u32::MAX).build(),
    );

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert!(matches!(
        state.xdg_positioners[&(CLIENT, POSITIONER)].gravity,
        XdgPositionerGravity::None
    ));
}

#[test]
fn every_anchor_and_gravity_the_protocol_defines_is_accepted() {
    for value in 0..=LAST_VALID {
        let (mut state, token, mut rx) = client_with_a_positioner();

        deliver(
            &mut state,
            POSITIONER,
            SET_ANCHOR,
            ArgWriter::new().u32(value).build(),
        );
        deliver(
            &mut state,
            POSITIONER,
            SET_GRAVITY,
            ArgWriter::new().u32(value).build(),
        );

        assert!(!was_sent_an_error(&mut rx), "{value} is a real anchor");
        assert!(!token.is_cancelled());
        let positioner = &state.xdg_positioners[&(CLIENT, POSITIONER)];
        assert_eq!(positioner.anchor as u32, value);
        assert_eq!(positioner.gravity as u32, value);
    }
}
