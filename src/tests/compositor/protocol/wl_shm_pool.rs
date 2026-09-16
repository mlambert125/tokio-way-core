//! Tests for the geometry a client may describe a buffer with.
//!
//! A `wl_buffer` is a rectangle inside a mapping, and every number describing
//! it comes from the client: where it starts, how long its rows are, how many
//! there are, and what the pixels mean. The compositor reads those pixels
//! directly, so each of these checks is a bound on what a client can make it
//! read — and, just as importantly, an answer the client can act on instead of
//! a window that silently stays black.

use super::{CLIENT, deliver, was_sent_an_error};
use std::os::fd::IntoRawFd;
use tokio::sync::mpsc::{Receiver, channel};
use tokio_util::sync::CancellationToken;
use tokio_way_core::protocol::{ObjectType, wire_utils::ArgWriter, wl_shm};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::WaylandEvent;

const POOL: u32 = 30;
const BUFFER: u32 = 40;
const CREATE_BUFFER: u16 = 0;
const RESIZE: u16 = 2;

/// A 16x16 ARGB pool: big enough to carve a real buffer out of, small enough
/// that going past it takes only a few pixels.
const SIDE: i32 = 16;
const STRIDE: i32 = SIDE * 4;
const POOL_SIZE: u32 = (SIDE * STRIDE) as u32;

fn client_with_a_pool() -> (CompositorState, CancellationToken, Receiver<WaylandEvent>) {
    let mut state = crate::tests::test_state();
    let (tx, rx) = channel(64);
    let token = CancellationToken::new();
    state.clients.create(CLIENT, tx, token.clone());
    state
        .clients
        .get(CLIENT)
        .unwrap()
        .register(POOL, ObjectType::WlShmPool)
        .unwrap();

    let fd = unsafe { libc::memfd_create(c"shm-pool-test".as_ptr().cast(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    file.set_len(u64::from(POOL_SIZE)).unwrap();
    assert!(state.register_shm_pool(CLIENT, POOL, file.into_raw_fd(), POOL_SIZE));

    (state, token, rx)
}

fn create_buffer(
    state: &mut CompositorState,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
) {
    deliver(
        state,
        POOL,
        CREATE_BUFFER,
        ArgWriter::new()
            .u32(BUFFER)
            .i32(offset)
            .i32(width)
            .i32(height)
            .i32(stride)
            .u32(format)
            .build(),
    );
}

/// A buffer that exactly fills the pool, which is the ordinary case.
fn create_a_whole_pool_buffer(state: &mut CompositorState) {
    create_buffer(state, 0, SIDE, SIDE, STRIDE, wl_shm::FORMAT_ARGB8888);
}

#[test]
fn a_buffer_filling_its_pool_is_accepted() {
    let (mut state, token, mut rx) = client_with_a_pool();

    create_a_whole_pool_buffer(&mut state);

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert!(state.buffers.contains_key(&(CLIENT, BUFFER)));
}

#[test]
fn a_buffer_one_row_past_its_pool_is_refused() {
    let (mut state, token, mut rx) = client_with_a_pool();

    // One row more than the pool holds. Without the check this is a buffer the
    // renderer declines to draw with nothing said to the client.
    create_buffer(
        &mut state,
        0,
        SIDE,
        SIDE + 1,
        STRIDE,
        wl_shm::FORMAT_ARGB8888,
    );

    assert!(was_sent_an_error(&mut rx), "the client must be told why");
    assert!(token.is_cancelled());
    assert!(!state.buffers.contains_key(&(CLIENT, BUFFER)));
}

#[test]
fn an_offset_that_pushes_the_buffer_past_the_pool_is_refused() {
    let (mut state, token, mut rx) = client_with_a_pool();

    // The geometry fits on its own; it is the offset that does not.
    create_buffer(&mut state, 4, SIDE, SIDE, STRIDE, wl_shm::FORMAT_ARGB8888);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
}

#[test]
fn a_negative_offset_is_refused_rather_than_taken_as_positive() {
    let (mut state, token, mut rx) = client_with_a_pool();

    create_buffer(&mut state, -8, SIDE, SIDE, STRIDE, wl_shm::FORMAT_ARGB8888);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert!(!state.buffers.contains_key(&(CLIENT, BUFFER)));
}

#[test]
fn a_stride_shorter_than_a_row_is_refused() {
    let (mut state, token, mut rx) = client_with_a_pool();

    // Four bytes a pixel, so a 16px row needs 64 bytes however the client
    // counts it.
    create_buffer(
        &mut state,
        0,
        SIDE,
        SIDE,
        STRIDE - 4,
        wl_shm::FORMAT_ARGB8888,
    );

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
}

#[test]
fn a_buffer_with_no_area_is_refused() {
    for (width, height) in [(0, SIDE), (SIDE, 0), (-SIDE, SIDE), (SIDE, -SIDE)] {
        let (mut state, token, mut rx) = client_with_a_pool();
        create_buffer(
            &mut state,
            0,
            width,
            height,
            STRIDE,
            wl_shm::FORMAT_ARGB8888,
        );
        assert!(was_sent_an_error(&mut rx), "{width}x{height}");
        assert!(token.is_cancelled(), "{width}x{height}");
    }
}

#[test]
fn a_format_that_was_never_advertised_is_refused() {
    let (mut state, token, mut rx) = client_with_a_pool();

    // Only ARGB8888 and XRGB8888 go out in `wl_shm.format`. Anything else was
    // previously accepted and then drawn as if it were ARGB, which is a window
    // full of the wrong colours rather than an error.
    create_buffer(
        &mut state,
        0,
        SIDE,
        SIDE,
        STRIDE,
        0x3432_4258, /* BG24 */
    );

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert!(!state.buffers.contains_key(&(CLIENT, BUFFER)));
}

#[test]
fn both_advertised_formats_are_accepted() {
    for format in [wl_shm::FORMAT_ARGB8888, wl_shm::FORMAT_XRGB8888] {
        let (mut state, token, mut rx) = client_with_a_pool();
        create_buffer(&mut state, 0, SIDE, SIDE, STRIDE, format);
        assert!(!was_sent_an_error(&mut rx), "format {format}");
        assert!(!token.is_cancelled(), "format {format}");
    }
}

#[test]
fn a_huge_buffer_cannot_overflow_its_way_past_the_check() {
    let (mut state, token, mut rx) = client_with_a_pool();

    // `height * stride` is well past `i32` here. Computed in `i32` it wraps to
    // something small and the bounds check waves it through, which is the one
    // way this check fails open.
    create_buffer(
        &mut state,
        0,
        SIDE,
        i32::MAX,
        i32::MAX,
        wl_shm::FORMAT_ARGB8888,
    );

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert!(!state.buffers.contains_key(&(CLIENT, BUFFER)));
}

fn resize(state: &mut CompositorState, size: i32) {
    deliver(state, POOL, RESIZE, ArgWriter::new().i32(size).build());
}

#[test]
fn a_pool_may_grow() {
    let (mut state, token, mut rx) = client_with_a_pool();
    let bigger = i32::try_from(POOL_SIZE).unwrap() * 2;
    // The file has to be able to back it, the same as at creation.
    let pool_fd = state.shm_pools[&(CLIENT, POOL)].fd;
    assert_eq!(unsafe { libc::ftruncate(pool_fd, i64::from(bigger)) }, 0);

    resize(&mut state, bigger);

    assert!(!was_sent_an_error(&mut rx));
    assert!(!token.is_cancelled());
    assert_eq!(state.shm_pools[&(CLIENT, POOL)].size, bigger.unsigned_abs());
}

#[test]
fn a_pool_may_not_shrink() {
    let (mut state, token, mut rx) = client_with_a_pool();

    // Buffers already carved out of the pool keep their offsets, so shrinking
    // leaves some of them describing memory the pool no longer covers.
    resize(&mut state, i32::try_from(POOL_SIZE).unwrap() / 2);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert_eq!(
        state.shm_pools[&(CLIENT, POOL)].size,
        POOL_SIZE,
        "and the pool keeps the size it had",
    );
}

#[test]
fn a_negative_resize_is_refused_rather_than_taken_as_positive() {
    let (mut state, token, mut rx) = client_with_a_pool();

    // This used to go through `unsigned_abs`, so a client asking for -4096 got
    // a 4096-byte pool and no indication that anything was wrong.
    resize(&mut state, -4096);

    assert!(was_sent_an_error(&mut rx));
    assert!(token.is_cancelled());
    assert_eq!(state.shm_pools[&(CLIENT, POOL)].size, POOL_SIZE);
}
