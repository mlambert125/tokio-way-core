//! Tests for what a client is told it can send.
//!
//! The advertisement is the contract: a client allocates against it and has
//! nothing to fall back on if the compositor then refuses what it built. So
//! what goes out here, and when, is worth pinning down.

use super::build_request;
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_backends::dma::fourcc;
use tokio_way_backends::dma::{DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_INVALID, DmabufFormat};
use tokio_way_core::protocol::wire_utils::{ArgReader, ArgWriter};
use tokio_way_core::protocol::zwp_linux_dmabuf::{INTERFACE, VERSION, send_formats};
use tokio_way_core::protocol::{ObjectType, wl_registry};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::{WaylandEvent, WaylandRequestWithClientInfo};

const CLIENT: u32 = 1;
const REGISTRY: u32 = 2;
const DMABUF: u32 = 3;

/// A tiling modifier, with distinct halves so a hi/lo swap cannot pass.
const TILED: u64 = 0x0123_4567_89ab_cdef;

fn add_client(state: &mut CompositorState, client_id: u32) -> Receiver<WaylandEvent> {
    let (tx, rx) = channel(64);
    state
        .clients
        .create(client_id, tx, CancellationToken::new());
    rx
}

/// A compositor whose backend has reported it can import these formats.
fn state_with_formats(formats: Vec<DmabufFormat>) -> CompositorState {
    let mut state = crate::tests::test_state();
    state.dmabuf_formats = formats;
    if !state.dmabuf_formats.is_empty() {
        state.dmabuf_global_name = Some(state.next_global_number);
        state.next_global_number += 1;
    }
    state
}

/// Every `wl_registry.global` a client would receive on `get_registry`.
fn advertised(state: &mut CompositorState, rx: &mut Receiver<WaylandEvent>) -> Vec<String> {
    wl_registry::advertise_globals(state, CLIENT, REGISTRY);
    let mut interfaces = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        let mut args = ArgReader::new(&msg.args);
        let (Some(_name), Some(interface)) = (args.u32(), args.string()) else {
            continue;
        };
        interfaces.push(interface);
    }
    interfaces
}

#[test]
fn nothing_importable_means_nothing_advertised() {
    let mut state = state_with_formats(Vec::new());
    let mut rx = add_client(&mut state, CLIENT);

    // Offering dma-buf a compositor cannot draw is worse than offering none:
    // the client allocates for it and is then left with nothing.
    assert!(
        !advertised(&mut state, &mut rx)
            .iter()
            .any(|i| i == INTERFACE)
    );
}

#[test]
fn an_importable_format_is_advertised_to_later_clients() {
    let mut state = state_with_formats(vec![DmabufFormat {
        fourcc: DRM_FORMAT_ARGB8888,
        modifiers: vec![TILED],
    }]);
    let mut rx = add_client(&mut state, CLIENT);

    assert!(
        advertised(&mut state, &mut rx)
            .iter()
            .any(|i| i == INTERFACE)
    );
}

#[test]
fn support_arriving_late_reaches_clients_already_connected() {
    let mut state = state_with_formats(Vec::new());
    let mut rx = add_client(&mut state, CLIENT);
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object(REGISTRY, ObjectType::WlRegistry)
        .unwrap();
    while rx.try_recv().is_ok() {}

    // The backend cannot answer until it has a GL context, which is after some
    // clients have connected and enumerated.
    wl_registry::broadcast_global(&mut state, 10, INTERFACE, VERSION);

    let msg = rx
        .try_recv()
        .expect("the global should have been broadcast");
    let mut args = ArgReader::new(&msg.args);
    assert_eq!(args.u32(), Some(10));
    assert_eq!(args.string().as_deref(), Some(INTERFACE));
    assert_eq!(args.u32(), Some(VERSION));
}

/// Bind the dma-buf global at a version and collect what comes back.
fn formats_sent_at_version(state: &mut CompositorState, version: u32) -> Vec<(u16, Vec<u32>)> {
    let (tx, mut rx) = channel(64);
    state.clients.create(CLIENT, tx, CancellationToken::new());
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object_with_version(DMABUF, ObjectType::ZwpLinuxDmabuf, version)
        .unwrap();

    send_formats(state, CLIENT, DMABUF);

    let mut events = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if msg.object_id != DMABUF {
            continue;
        }
        let mut args = ArgReader::new(&msg.args);
        let mut values = Vec::new();
        while let Some(value) = args.u32() {
            values.push(value);
        }
        events.push((msg.op_code, values));
    }
    events
}

#[test]
fn version_three_is_told_the_layout_and_version_one_only_the_format() {
    let formats = vec![DmabufFormat {
        fourcc: DRM_FORMAT_ARGB8888,
        modifiers: vec![TILED],
    }];

    let mut state = state_with_formats(formats.clone());
    let old = formats_sent_at_version(&mut state, 1);
    // A version 1 client has no way to be told about layouts, so it hears only
    // that the format works.
    assert!(old.iter().all(|(op, args)| *op == 0 && args.len() == 1));
    assert!(old.iter().any(|(_, args)| args[0] == DRM_FORMAT_ARGB8888));

    let mut state = state_with_formats(formats);
    let new = formats_sent_at_version(&mut state, 3);
    assert!(
        new.iter().all(|(op, args)| *op == 1 && args.len() == 3),
        "a version 3 client is sent modifier events, never format ones"
    );
}

#[test]
fn a_modifier_goes_out_high_half_first() {
    let mut state = state_with_formats(vec![DmabufFormat {
        fourcc: DRM_FORMAT_ARGB8888,
        modifiers: vec![TILED],
    }]);

    let events = formats_sent_at_version(&mut state, 3);
    let tiled = events
        .iter()
        .find(|(_, args)| args[1] != 0x00ff_ffff)
        .expect("the tiling modifier should have been advertised");

    // Halves the wrong way round is the classic way to advertise a layout
    // nobody has, so the two are chosen to be distinguishable.
    assert_eq!(tiled.1[1], 0x0123_4567, "high half first");
    assert_eq!(tiled.1[2], 0x89ab_cdef, "then low");
}

#[test]
fn a_format_with_no_named_modifiers_still_offers_the_implicit_one() {
    let mut state = state_with_formats(vec![DmabufFormat {
        fourcc: fourcc(*b"XR24"),
        modifiers: Vec::new(),
    }]);

    let events = formats_sent_at_version(&mut state, 3);

    // With nothing named, an implicit layout is all there is — and saying
    // nothing at all would leave a version 3 client believing the format is
    // unsupported entirely.
    assert_eq!(events.len(), 1);
    assert_eq!(
        (events[0].1[1], events[0].1[2]),
        (
            u32::try_from(DRM_FORMAT_MOD_INVALID >> 32).unwrap(),
            u32::try_from(DRM_FORMAT_MOD_INVALID & 0xffff_ffff).unwrap(),
        )
    );
}

#[test]
fn creating_params_registers_an_object_to_describe_a_buffer_on() {
    let mut state = state_with_formats(vec![DmabufFormat {
        fourcc: DRM_FORMAT_ARGB8888,
        modifiers: Vec::new(),
    }]);
    let _rx = add_client(&mut state, CLIENT);
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object(DMABUF, ObjectType::ZwpLinuxDmabuf)
        .unwrap();

    tokio_way_core::protocol::zwp_linux_dmabuf::handle(
        &mut state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: build_request(DMABUF, 1, ArgWriter::new().u32(40).build()),
        },
    );

    assert!(state.dmabuf_params.contains_key(&(CLIENT, 40)));
    assert_eq!(
        state.clients.get(CLIENT).unwrap().objects.get(&40),
        Some(&ObjectType::ZwpLinuxBufferParams)
    );
}
