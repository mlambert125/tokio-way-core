//! Checks the request signature table against the dispatcher.
//!
//! [`tokio_way_core::protocol::signature`] is a hand-written transcription
//! of the protocol, and a transcription that has drifted from the code it
//! describes is worse than none at all: the fd accounting is derived from it,
//! so a wrong entry desyncs a client's descriptor queue for the rest of its
//! connection. Neither side is trusted here — each is checked against the
//! other.
//!
//! Two directions, and both matter. An opcode the table lists must be one its
//! interface accepts, or the table claims a request that does not exist. An
//! opcode just past the end of the table must be refused, or the interface has
//! a request the table has not got.

use super::CLIENT;
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::signature::{ArgType, Request, request_at, requests};
use tokio_way_core::protocol::{ObjectType, handle_message, wire_utils::ArgWriter};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::{WaylandEvent, WaylandRequest, WaylandRequestWithClientInfo};

/// Every interface the dispatcher knows, which is every one the table must
/// cover. Kept as a list rather than derived because `ObjectType` has no
/// iteration and adding a variant should make somebody look here.
const EVERY_INTERFACE: &[ObjectType] = &[
    ObjectType::WlDisplay,
    ObjectType::WlRegistry,
    ObjectType::WlCallback,
    ObjectType::WlCompositor,
    ObjectType::WlShm,
    ObjectType::WlShmPool,
    ObjectType::WlBuffer,
    ObjectType::WlSurface,
    ObjectType::WlRegion,
    ObjectType::WlSeat,
    ObjectType::WlPointer,
    ObjectType::WlKeyboard,
    ObjectType::WlTouch,
    ObjectType::WlOutput,
    ObjectType::WlSubcompositor,
    ObjectType::WlSubsurface,
    ObjectType::WlDataDeviceManager,
    ObjectType::WlDataDevice,
    ObjectType::WlDataSource,
    ObjectType::WlDataOffer,
    ObjectType::ZwpPrimarySelectionDeviceManager,
    ObjectType::ZwpPrimarySelectionDevice,
    ObjectType::ZwpPrimarySelectionSource,
    ObjectType::ZwpPrimarySelectionOffer,
    ObjectType::ZwlrDataControlManager,
    ObjectType::ZwlrDataControlDevice,
    ObjectType::ZwlrDataControlSource,
    ObjectType::ZwlrDataControlOffer,
    ObjectType::WpFractionalScaleManager,
    ObjectType::WpFractionalScale,
    ObjectType::WpCursorShapeManager,
    ObjectType::WpCursorShapeDevice,
    ObjectType::WlFixes,
    ObjectType::XdgWmBase,
    ObjectType::XdgSurface,
    ObjectType::XdgToplevel,
    ObjectType::XdgPopup,
    ObjectType::XdgPositioner,
    ObjectType::XdgSystemBell,
    ObjectType::WpViewporter,
    ObjectType::WpViewport,
    ObjectType::WpPresentation,
    ObjectType::WpPresentationFeedback,
    ObjectType::ZwpLinuxDmabuf,
    ObjectType::ZwpLinuxBufferParams,
    ObjectType::ZxdgDecorationManager,
    ObjectType::ZxdgToplevelDecoration,
    ObjectType::ZwlrLayerShell,
    ObjectType::ZwlrLayerSurface,
];

/// The object under test, and a fresh id to hand any `new_id` argument.
const SUBJECT: u32 = 500;
const FRESH: u32 = 900;

fn state_with(object_type: ObjectType) -> (CompositorState, Receiver<WaylandEvent>) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(64);
    state.clients.create(CLIENT, tx, CancellationToken::new());
    // wl_display is registered by ClientState::new, so it is the one interface
    // whose object already exists at a known id.
    if object_type != ObjectType::WlDisplay {
        state
            .clients
            .get(CLIENT)
            .unwrap()
            .register_client_object_with_version(SUBJECT, object_type, 8)
            .unwrap();
    }
    (state, rx)
}

fn subject_id(object_type: ObjectType) -> u32 {
    if object_type == ObjectType::WlDisplay {
        tokio_way_core::protocol::wl_display::OBJECT_ID
    } else {
        SUBJECT
    }
}

/// Encode arguments that decode cleanly, so a request reaches its handler
/// rather than stopping at the malformed-arguments arm.
fn well_formed(request: &Request) -> Vec<u8> {
    let mut args = ArgWriter::new();
    for arg in request.args {
        args = match arg {
            ArgType::Int => args.i32(1),
            ArgType::Uint | ArgType::Object => args.u32(1),
            ArgType::Fixed => args.fixed(1.0),
            ArgType::NewId => args.u32(FRESH),
            ArgType::String => args.string("x"),
            ArgType::Array => args.array_u32(&[1]),
            // Out of band: it occupies no wire bytes.
            ArgType::Fd => args,
        };
    }
    args.build()
}

/// Put a descriptor on the client's queue for the requests that claim one.
///
/// Without it the dispatcher refuses the request for a missing fd before any
/// handler runs, and the test would be checking the wrong thing.
fn provide_descriptors(state: &mut CompositorState, count: usize) {
    let Some(client) = state.clients.get(CLIENT) else {
        return;
    };
    for _ in 0..count {
        // SAFETY: `memfd_create` takes a name and flags and returns a new fd.
        let fd = unsafe { libc::memfd_create(c"sig".as_ptr().cast(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        client.fd_queue.push_back(unsafe {
            <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
        });
    }
}

/// Whether the client was told the interface has no such request.
///
/// The specific complaint matters. Plenty of these requests fail for other
/// reasons — an object that does not exist, a serial nobody issued — and those
/// are fine. The one answer that would mean the table is wrong is the
/// dispatcher saying the opcode is not a request at all.
fn was_told_no_such_request(rx: &mut Receiver<WaylandEvent>) -> bool {
    std::iter::from_fn(|| rx.try_recv().ok()).any(|m| {
        m.object_id == tokio_way_core::protocol::wl_display::OBJECT_ID
            && m.op_code == tokio_way_core::protocol::wl_display::ERROR
            // wl_display.error is (object, code, message); the message starts
            // at byte 8 and is a length-prefixed string.
            && String::from_utf8_lossy(&m.args).contains("has no request")
    })
}

fn deliver(state: &mut CompositorState, object_id: u32, op_code: u16, args: Vec<u8>) {
    handle_message(
        state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: WaylandRequest {
                object_id,
                op_code,
                args,
            },
        },
    );
}

#[test]
fn every_request_in_the_table_is_one_its_interface_accepts() {
    for &object_type in EVERY_INTERFACE {
        for (op_code, request) in requests(object_type).iter().enumerate() {
            let op_code = u16::try_from(op_code).unwrap();
            let (mut state, mut rx) = state_with(object_type);
            let descriptors = request.args.iter().filter(|a| **a == ArgType::Fd).count();
            provide_descriptors(&mut state, descriptors);

            deliver(
                &mut state,
                subject_id(object_type),
                op_code,
                well_formed(request),
            );

            assert!(
                !was_told_no_such_request(&mut rx),
                "{object_type:?} opcode {op_code} is in the table as {:?}, \
                 but the dispatcher says it has no such request",
                request.name,
            );
        }
    }
}

#[test]
fn the_first_opcode_past_the_table_is_refused() {
    for &object_type in EVERY_INTERFACE {
        let past = u16::try_from(requests(object_type).len()).unwrap();
        let (mut state, mut rx) = state_with(object_type);

        deliver(&mut state, subject_id(object_type), past, Vec::new());

        assert!(
            was_told_no_such_request(&mut rx),
            "{object_type:?} accepts opcode {past}, which is past the end of \
             its table — the table is missing a request",
        );
    }
}

#[test]
fn descriptor_counts_come_from_the_table() {
    // `request_fd_count` is private, so this checks the property it exists to
    // provide: a request declaring a descriptor consumes exactly one from the
    // client's queue, and a request declaring none consumes nothing.
    for &object_type in EVERY_INTERFACE {
        for (op_code, request) in requests(object_type).iter().enumerate() {
            let declared = request.args.iter().filter(|a| **a == ArgType::Fd).count();
            let (mut state, _rx) = state_with(object_type);
            provide_descriptors(&mut state, 2);

            deliver(
                &mut state,
                subject_id(object_type),
                u16::try_from(op_code).unwrap(),
                well_formed(request),
            );

            let left = state
                .clients
                .get(CLIENT)
                .map_or(0, |client| client.fd_queue.len());
            assert_eq!(
                2 - left,
                declared,
                "{object_type:?}.{} declares {declared} descriptor(s) but took {}",
                request.name,
                2 - left,
            );
        }
    }
}

#[test]
fn the_table_covers_every_interface_the_dispatcher_knows() {
    // A new ObjectType with no table entry would otherwise be an empty request
    // list, which reads as "this interface has no requests" and silently makes
    // every one of its opcodes an error.
    let empty: Vec<ObjectType> = EVERY_INTERFACE
        .iter()
        .copied()
        .filter(|&object_type| requests(object_type).is_empty())
        .collect();
    assert_eq!(
        empty,
        vec![ObjectType::WlCallback, ObjectType::WpPresentationFeedback],
        "only wl_callback and wp_presentation_feedback genuinely have no requests",
    );
}

#[test]
fn no_request_creates_more_than_one_object() {
    // What a generator can rely on: a request has at most one `new_id`, so
    // there is exactly one id to invent and the rest of the arguments name
    // things that already exist.
    //
    // Where that id sits is *not* something to rely on. It leads for every
    // constructor except two — `wl_registry.bind`, which puts it after the
    // interface and version it is binding, and `wp_presentation.feedback`,
    // which puts it after the surface being asked about. Both are the
    // protocol's own doing, and a generator that assumed otherwise would
    // quietly send the wrong word.
    let mut trailing = Vec::new();
    for &object_type in EVERY_INTERFACE {
        for request in requests(object_type) {
            let positions: Vec<usize> = request
                .args
                .iter()
                .enumerate()
                .filter(|(_, a)| **a == ArgType::NewId)
                .map(|(i, _)| i)
                .collect();
            assert!(
                positions.len() <= 1,
                "{object_type:?}.{} creates {} objects",
                request.name,
                positions.len(),
            );
            if positions.first().is_some_and(|&at| at > 0) {
                trailing.push((object_type, request.name));
            }
        }
    }
    assert_eq!(
        trailing,
        vec![
            (ObjectType::WlRegistry, "bind"),
            (ObjectType::WpPresentation, "feedback"),
        ],
    );
}

#[test]
fn request_at_stops_at_the_end_of_an_interface() {
    assert!(request_at(ObjectType::WlBuffer, 0).is_some());
    assert!(request_at(ObjectType::WlBuffer, 1).is_none());
    assert!(request_at(ObjectType::WlCallback, 0).is_none());
}
