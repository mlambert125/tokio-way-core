//! Tests for describing a buffer, and for the line between the two ways of
//! refusing one.
//!
//! A malformed request is fatal and takes the client with it; "the driver will
//! not take this" is the `failed` event, which a client can recover from by
//! falling back to shm. Putting a case on the wrong side of that line either
//! kills clients that did nothing wrong or lets a bad request through.

use super::build_request;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_backends::dma::fourcc;
use tokio_way_backends::dma::{DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_INVALID, DmabufFormat};
use tokio_way_core::protocol::wire_utils::{ArgReader, ArgWriter};
use tokio_way_core::protocol::zwp_linux_buffer_params::{
    ADD, CREATE, CREATE_IMMED, DESTROY, handle, resolve_import,
};
use tokio_way_core::protocol::{ObjectType, wl_display};
use tokio_way_core::state::{BufferKind, CompositorState};
use tokio_way_sock::{WaylandEvent, WaylandRequestWithClientInfo};

const CLIENT: u32 = 1;
const PARAMS: u32 = 10;
const BUFFER: u32 = 11;
const SIDE: i32 = 64;
/// A linear layout, whose extent can be checked from the outside.
const LINEAR: u64 = 0;
/// Intel's Y-tiled with compression, whose extent cannot.
const TILED_CCS: u64 = 0x0100_0000_0000_0008;

/// A compositor that advertises one format, with a client holding a params
/// object ready to describe a buffer on.
fn state_with_params() -> (CompositorState, Receiver<WaylandEvent>) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(64);
    state.clients.create(CLIENT, tx, CancellationToken::new());
    state.dmabuf_formats = vec![DmabufFormat {
        fourcc: DRM_FORMAT_ARGB8888,
        modifiers: vec![LINEAR, TILED_CCS],
    }];
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(PARAMS, ObjectType::ZwpLinuxBufferParams)
        .unwrap();
    state.dmabuf_params.insert(
        (CLIENT, PARAMS),
        tokio_way_core::state::BufferParams::default(),
    );
    (state, rx)
}

/// A descriptor of `size` bytes, standing in for one a driver would give.
fn buffer_fd(size: i64) -> OwnedFd {
    let fd = unsafe { libc::memfd_create(c"params-test".as_ptr().cast(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    assert_eq!(unsafe { libc::ftruncate(fd, size) }, 0);
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// A descriptor big enough for a linear `SIDE` square.
fn whole_buffer_fd() -> OwnedFd {
    buffer_fd(i64::from(SIDE) * i64::from(SIDE) * 4)
}

fn add_plane(state: &mut CompositorState, index: u32, stride: u32, modifier: u64, fd: OwnedFd) {
    handle(
        state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: build_request(
                PARAMS,
                ADD,
                ArgWriter::new()
                    .u32(index)
                    .u32(0)
                    .u32(stride)
                    .u32(u32::try_from(modifier >> 32).unwrap())
                    .u32(u32::try_from(modifier & 0xffff_ffff).unwrap())
                    .build(),
            ),
        },
        // `handle_message` claims the fd off the client's queue before
        // dispatching; calling the handler directly hands it over the same way.
        vec![fd],
    );
}

/// One plane, sound in every way, so a test can change exactly one thing.
fn add_good_plane(state: &mut CompositorState) {
    add_plane(state, 0, SIDE.unsigned_abs() * 4, LINEAR, whole_buffer_fd());
}

fn create(state: &mut CompositorState, width: i32, height: i32, format: u32, flags: u32) {
    handle(
        state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: build_request(
                PARAMS,
                CREATE,
                ArgWriter::new()
                    .i32(width)
                    .i32(height)
                    .u32(format)
                    .u32(flags)
                    .build(),
            ),
        },
        Vec::new(),
    );
}

/// The `zwp_linux_buffer_params_v1.error` code the client was killed with, if
/// it was.
fn fatal_code(rx: &mut Receiver<WaylandEvent>) -> Option<u32> {
    while let Ok(msg) = rx.try_recv() {
        if msg.object_id == wl_display::OBJECT_ID && msg.op_code == wl_display::ERROR {
            let mut args = ArgReader::new(&msg.args);
            let (Some(object), Some(code)) = (args.u32(), args.u32()) else {
                continue;
            };
            assert_eq!(object, PARAMS, "the error names the params object");
            return Some(code);
        }
    }
    None
}

/// Whether the client was told, non-fatally, that its buffer could not be made.
fn was_failed(rx: &mut Receiver<WaylandEvent>) -> bool {
    let mut failed = false;
    while let Ok(msg) = rx.try_recv() {
        assert_ne!(
            (msg.object_id, msg.op_code),
            (wl_display::OBJECT_ID, wl_display::ERROR),
            "this should not have been fatal"
        );
        if msg.object_id == PARAMS && msg.op_code == 1 {
            failed = true;
        }
    }
    failed
}

#[test]
fn a_plane_index_past_the_last_is_fatal() {
    let (mut state, mut rx) = state_with_params();
    add_plane(&mut state, 4, 256, LINEAR, whole_buffer_fd());
    assert_eq!(fatal_code(&mut rx), Some(1));
}

#[test]
fn setting_the_same_plane_twice_is_fatal() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    add_good_plane(&mut state);
    assert_eq!(fatal_code(&mut rx), Some(2));
}

#[test]
fn creating_from_no_planes_is_fatal() {
    let (mut state, mut rx) = state_with_params();
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);
    assert_eq!(fatal_code(&mut rx), Some(3));
}

#[test]
fn a_gap_in_the_planes_is_fatal() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    // Plane 2 with no plane 1 describes nothing.
    add_plane(&mut state, 2, 256, LINEAR, whole_buffer_fd());
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);
    assert_eq!(fatal_code(&mut rx), Some(3));
}

#[test]
fn a_buffer_with_no_area_is_fatal() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    create(&mut state, 0, SIDE, DRM_FORMAT_ARGB8888, 0);
    assert_eq!(fatal_code(&mut rx), Some(5));
}

#[test]
fn planes_that_disagree_about_the_layout_are_fatal() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    add_plane(&mut state, 1, 256, TILED_CCS, whole_buffer_fd());
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);

    // One buffer has one layout, but the protocol carries a modifier per plane.
    assert_eq!(fatal_code(&mut rx), Some(4));
}

#[test]
fn using_the_params_object_twice_is_fatal() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);
    while rx.try_recv().is_ok() {}

    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);

    // Refused even though the first create is still out with the backend: the
    // object is spent the moment it is used, not once the answer comes back.
    assert_eq!(fatal_code(&mut rx), Some(0));
}

#[test]
fn adding_a_plane_after_creating_is_fatal() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);
    while rx.try_recv().is_ok() {}

    add_plane(&mut state, 1, 256, LINEAR, whole_buffer_fd());
    assert_eq!(fatal_code(&mut rx), Some(0));
}

#[test]
fn a_plane_running_past_its_descriptor_is_fatal() {
    let (mut state, mut rx) = state_with_params();
    // A descriptor a quarter of the size the stride and height claim.
    add_plane(
        &mut state,
        0,
        SIDE.unsigned_abs() * 4,
        LINEAR,
        buffer_fd(i64::from(SIDE) * i64::from(SIDE)),
    );
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);

    // The only bound on what a client can make the driver read.
    assert_eq!(fatal_code(&mut rx), Some(6));
}

#[test]
fn only_the_first_plane_is_measured() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    // What a Vulkan swapchain on Intel hardware actually sends: a second plane
    // holding compression metadata, at a large offset with a tiny stride and a
    // geometry all its own. Measuring it against the image's width and height
    // is meaningless, and doing so rejected every compressed buffer offered.
    add_plane(&mut state, 1, 512, LINEAR, buffer_fd(4096));
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);

    assert_eq!(
        fatal_code(&mut rx),
        None,
        "a metadata plane is not an image and must not be measured like one"
    );
}

#[test]
fn a_layout_the_compositor_cannot_measure_is_left_to_the_driver() {
    let (mut state, mut rx) = state_with_params();
    // Under a tiling or compression modifier a row is not `stride` bytes after
    // the one above it, so the linear extent says nothing about whether the
    // buffer fits. The driver knows the real layout and checks it at import.
    add_plane(
        &mut state,
        0,
        SIDE.unsigned_abs() * 4,
        TILED_CCS,
        buffer_fd(i64::from(SIDE) * i64::from(SIDE) * 2),
    );
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);

    assert_eq!(fatal_code(&mut rx), None);
}

#[test]
fn a_format_that_was_never_advertised_is_refused_without_killing_the_client() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    create(&mut state, SIDE, SIDE, fourcc(*b"NV12"), 0);

    // Non-fatal on purpose: `failed` is what lets the client fall back to shm.
    assert!(was_failed(&mut rx));
}

#[test]
fn a_flag_we_cannot_honour_is_refused_without_killing_the_client() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    // y_invert: legal to ask for, and this renderer has no way to flip.
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 1);

    assert!(was_failed(&mut rx));
}

#[test]
fn with_no_backend_to_ask_the_client_is_told_now_rather_than_left_waiting() {
    let (mut state, mut rx) = state_with_params();
    add_good_plane(&mut state);
    // `backend_requests` is None here, as it is in any headless test. A client
    // blocked on `created`/`failed` must not be left waiting for a verdict
    // that cannot arrive.
    create(&mut state, SIDE, SIDE, DRM_FORMAT_ARGB8888, 0);

    assert!(was_failed(&mut rx));
    assert!(state.pending_dmabuf_imports.is_empty());
}

#[test]
fn destroying_the_params_cancels_an_import_still_in_flight() {
    let (mut state, _rx) = state_with_params();
    let image = Arc::new(tokio_way_backends::dma::DmabufImage {
        width: SIDE,
        height: SIDE,
        fourcc: DRM_FORMAT_ARGB8888,
        modifier: DRM_FORMAT_MOD_INVALID,
        planes: Vec::new(),
    });
    state.pending_dmabuf_imports.insert(
        7,
        tokio_way_core::state::PendingImport {
            client_id: CLIENT,
            params_id: PARAMS,
            immediate: None,
            image,
            width: SIDE,
            height: SIDE,
        },
    );

    handle(
        &mut state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: build_request(PARAMS, DESTROY, Vec::new()),
        },
        Vec::new(),
    );

    // Otherwise the verdict would name an object that no longer exists — and
    // the descriptors it holds would be pinned for the life of the connection.
    assert!(state.pending_dmabuf_imports.is_empty());
}

#[test]
fn a_buffer_the_driver_refuses_stays_an_object_the_client_still_owns() {
    let (mut state, mut rx) = state_with_params();
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(BUFFER, ObjectType::WlBuffer)
        .unwrap();
    let image = Arc::new(tokio_way_backends::dma::DmabufImage {
        width: SIDE,
        height: SIDE,
        fourcc: DRM_FORMAT_ARGB8888,
        modifier: DRM_FORMAT_MOD_INVALID,
        planes: Vec::new(),
    });
    state.buffers.insert(
        (CLIENT, BUFFER),
        tokio_way_core::state::Buffer {
            client_id: CLIENT,
            width: SIDE,
            height: SIDE,
            content_serial: 42,
            kind: BufferKind::Dmabuf(image.clone()),
        },
    );
    state.pending_dmabuf_imports.insert(
        7,
        tokio_way_core::state::PendingImport {
            client_id: CLIENT,
            params_id: PARAMS,
            immediate: Some((BUFFER, 42)),
            image,
            width: SIDE,
            height: SIDE,
        },
    );

    resolve_import(&mut state, 7, false);

    // `create_immed` named the id itself, so it will destroy that id later.
    // Forgetting the object would make that destroy an unknown-object error,
    // and disconnect a client for its driver's answer.
    assert_eq!(
        state.clients.get(CLIENT).unwrap().objects.get(&BUFFER),
        Some(&ObjectType::WlBuffer)
    );
    assert!(matches!(
        state.buffers[&(CLIENT, BUFFER)].kind,
        BufferKind::Failed
    ));
    assert!(was_failed(&mut rx));
}

#[test]
fn a_verdict_for_a_buffer_id_since_reused_is_ignored() {
    let (mut state, _rx) = state_with_params();
    let image = Arc::new(tokio_way_backends::dma::DmabufImage {
        width: SIDE,
        height: SIDE,
        fourcc: DRM_FORMAT_ARGB8888,
        modifier: DRM_FORMAT_MOD_INVALID,
        planes: Vec::new(),
    });
    // The client destroyed the buffer the verdict is about and built another
    // under the same id, which is legal once the id has been released.
    state.buffers.insert(
        (CLIENT, BUFFER),
        tokio_way_core::state::Buffer {
            client_id: CLIENT,
            width: SIDE,
            height: SIDE,
            content_serial: 99,
            kind: BufferKind::Dmabuf(image.clone()),
        },
    );
    state.pending_dmabuf_imports.insert(
        7,
        tokio_way_core::state::PendingImport {
            client_id: CLIENT,
            params_id: PARAMS,
            immediate: Some((BUFFER, 42)),
            image,
            width: SIDE,
            height: SIDE,
        },
    );

    resolve_import(&mut state, 7, false);

    assert!(
        matches!(state.buffers[&(CLIENT, BUFFER)].kind, BufferKind::Dmabuf(_)),
        "the serial is what stops an old verdict landing on a new buffer"
    );
}

#[test]
fn a_created_buffer_is_named_from_the_compositors_half_of_the_id_space() {
    let (mut state, mut rx) = state_with_params();
    let image = Arc::new(tokio_way_backends::dma::DmabufImage {
        width: SIDE,
        height: SIDE,
        fourcc: DRM_FORMAT_ARGB8888,
        modifier: DRM_FORMAT_MOD_INVALID,
        planes: Vec::new(),
    });
    state.pending_dmabuf_imports.insert(
        7,
        tokio_way_core::state::PendingImport {
            client_id: CLIENT,
            params_id: PARAMS,
            immediate: None,
            image,
            width: SIDE,
            height: SIDE,
        },
    );

    resolve_import(&mut state, 7, true);

    let mut created = None;
    while let Ok(msg) = rx.try_recv() {
        if msg.object_id == PARAMS && msg.op_code == 0 {
            created = ArgReader::new(&msg.args).u32();
        }
    }
    let buffer_id = created.expect("the client should have been given a buffer");
    assert!(
        tokio_way_core::state::is_server_id(buffer_id),
        "a buffer the compositor names must not collide with the client's own ids"
    );
    assert!(state.buffers.contains_key(&(CLIENT, buffer_id)));
}

/// `create_immed` takes the same arguments as `create` behind a `new_id`.
#[test]
fn create_immed_registers_the_buffer_the_client_named() {
    let (mut state, _rx) = state_with_params();
    add_good_plane(&mut state);

    handle(
        &mut state,
        &WaylandRequestWithClientInfo {
            client_id: CLIENT,
            message: build_request(
                PARAMS,
                CREATE_IMMED,
                ArgWriter::new()
                    .u32(BUFFER)
                    .i32(SIDE)
                    .i32(SIDE)
                    .u32(DRM_FORMAT_ARGB8888)
                    .u32(0)
                    .build(),
            ),
        },
        Vec::new(),
    );

    // With no backend the import cannot be verified, so it lands as a buffer
    // that draws nothing — but it is a buffer, and the id the client named.
    assert_eq!(
        state.clients.get(CLIENT).unwrap().objects.get(&BUFFER),
        Some(&ObjectType::WlBuffer)
    );
    assert!(matches!(
        state.buffers[&(CLIENT, BUFFER)].kind,
        BufferKind::Failed
    ));
}
