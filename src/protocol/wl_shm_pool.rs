//! `wl_shm_pool` protocol handler.
//!
//! A pool represents an mmap-able region of shared memory. Clients create
//! buffers from pools (specifying offset, dimensions, stride, format),
//! resize pools, and destroy them when done.

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;
use super::wire_utils::ArgReader;
use super::{ObjectType, wl_shm};

// Request opcodes
const CREATE_BUFFER: u16 = 0;
const DESTROY: u16 = 1;
const RESIZE: u16 = 2;

// wl_shm.error
/// The pixel format is not one the compositor advertised.
const ERROR_INVALID_FORMAT: u32 = 0;
/// The geometry does not describe a buffer that fits its pool.
const ERROR_INVALID_STRIDE: u32 = 1;
/// The pool's file cannot back the pool the client asked for.
const ERROR_INVALID_FD: u32 = 2;

/// Bytes per pixel. Every format `wl_shm::send_formats` advertises is 32-bit,
/// which is what makes a single constant here honest rather than an
/// assumption — a wider format would have to widen this with it.
const BYTES_PER_PIXEL: i64 = 4;

/// Check a `create_buffer` against the pool it draws from.
///
/// A buffer is a rectangle inside a mapping, described entirely by the client:
/// where it starts, how long its rows are, and how many there are. None of
/// that has to be true, and the compositor reads the pixels out directly, so
/// every one of these is a bound on what a client can make it read.
///
/// Returns the `wl_shm.error` code and a message, or `Ok` if the geometry
/// describes a rectangle that fits.
///
/// Arithmetic is in `i64` throughout. The products here overflow `i32` for
/// perfectly ordinary values — a 4K buffer is already past it — and an
/// overflow in a bounds check is the check silently passing.
fn check_geometry(
    pool_size: u32,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
) -> Result<(), (u32, String)> {
    if format != wl_shm::FORMAT_ARGB8888 && format != wl_shm::FORMAT_XRGB8888 {
        return Err((
            ERROR_INVALID_FORMAT,
            format!("wl_shm_pool.create_buffer: format {format} was never advertised"),
        ));
    }
    if width <= 0 || height <= 0 {
        return Err((
            ERROR_INVALID_STRIDE,
            format!("wl_shm_pool.create_buffer: {width}x{height} is not a rectangle"),
        ));
    }
    if offset < 0 {
        return Err((
            ERROR_INVALID_STRIDE,
            format!("wl_shm_pool.create_buffer: offset {offset} is before the pool"),
        ));
    }
    let (offset, width, height, stride) = (
        i64::from(offset),
        i64::from(width),
        i64::from(height),
        i64::from(stride),
    );
    let row_bytes = width * BYTES_PER_PIXEL;
    if stride < row_bytes {
        return Err((
            ERROR_INVALID_STRIDE,
            format!("wl_shm_pool.create_buffer: stride {stride} is shorter than a {width}px row"),
        ));
    }
    // The last row needs no stride padding, so the extent stops at the end of
    // its pixels rather than a full stride later.
    let extent = offset + (height - 1) * stride + row_bytes;
    if extent > i64::from(pool_size) {
        return Err((
            ERROR_INVALID_STRIDE,
            format!(
                "wl_shm_pool.create_buffer: buffer ends at {extent}, past the {pool_size}-byte pool"
            ),
        ));
    }
    Ok(())
}

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    match msg.message.op_code {
        CREATE_BUFFER => handle_create_buffer(state, msg),
        DESTROY => handle_destroy(state, msg),
        RESIZE => handle_resize(state, msg),
        _ => super::reject_unknown_request(state, msg, "wl_shm_pool"),
    }
}

fn handle_create_buffer(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let pool_id = msg.message.object_id;
    let pool_size = state
        .shm_pools
        .get(&(msg.client_id, pool_id))
        .map(|pool| pool.size);

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // create_buffer args: new_id, int32 offset, int32 width, int32 height, int32 stride, uint32 format
    let (Some(buffer_id), Some(offset), Some(width), Some(height), Some(stride), Some(format)) = (
        args.new_id(),
        args.i32(),
        args.i32(),
        args.i32(),
        args.i32(),
        args.u32(),
    ) else {
        super::reject_malformed_request(state, msg, "wl_shm_pool");
        return;
    };

    debug!(
        "wl_shm_pool.create_buffer: buffer_id={} offset={} {}x{} stride={} format={}",
        buffer_id, offset, width, height, stride, format
    );

    // Checked here rather than left to the render path. Nothing downstream is
    // unsafe without it — `TextureImage::bytes` re-checks the extent against
    // the mapping and refuses to read past it — but the client is owed an
    // answer either way, and without one the whole failure it sees is a window
    // that stays black with a `debug!` line it cannot read. Every one of these
    // is a `wl_shm.error` the protocol defines for exactly this.
    let Some(pool_size) = pool_size else {
        tracing::warn!("wl_shm_pool.create_buffer on unknown pool {pool_id}");
        return;
    };
    if let Err((code, message)) = check_geometry(pool_size, offset, width, height, stride, format) {
        tracing::warn!("client {}: {message}", msg.client_id);
        client.send_error(pool_id, code, &message);
        return;
    }

    if client
        .register_client_object(buffer_id, ObjectType::WlBuffer)
        .is_err()
    {
        return;
    }
    state.register_buffer(
        msg.client_id,
        buffer_id,
        msg.message.object_id,
        offset,
        width,
        height,
        stride,
        format,
    );
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let pool_id = msg.message.object_id;
    debug!("wl_shm_pool.destroy: pool_id={}", pool_id);
    state.destroy_shm_pool(msg.client_id, pool_id);
    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(pool_id);
    } else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
    }
}

fn handle_resize(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    let Some(new_size) = args.i32() else {
        super::reject_malformed_request(state, msg, "wl_shm_pool");
        return;
    };
    let pool_id = msg.message.object_id;
    debug!(
        "wl_shm_pool.resize: pool_id={} new_size={}",
        pool_id, new_size
    );

    // A pool may only grow. The protocol says so outright, and the reason it
    // does is that buffers already carved out of the pool keep their offsets:
    // shrinking would leave some of them describing memory the pool no longer
    // covers, which is a set of windows that quietly stop drawing.
    //
    // `new_size` is signed on the wire, and a negative one used to be run
    // through `unsigned_abs` — turning a client's -4096 into a 4096-byte pool
    // and carrying on. Reinterpreting an argument is worse than refusing it:
    // the client believes something happened that did not.
    let current = state
        .shm_pools
        .get(&(msg.client_id, pool_id))
        .map(|pool| pool.size);
    let Some(current) = current else {
        tracing::warn!("wl_shm_pool.resize on unknown pool {pool_id}");
        return;
    };
    let grown = u32::try_from(new_size).ok().filter(|&size| size >= current);
    let Some(new_size) = grown else {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                pool_id,
                ERROR_INVALID_FD,
                &format!("wl_shm_pool.resize: {new_size} does not grow a pool of {current} bytes"),
            );
        }
        return;
    };

    if !state.resize_shm_pool(msg.client_id, pool_id, new_size)
        && let Some(client) = state.clients.get(msg.client_id)
    {
        client.send_error(
            pool_id,
            ERROR_INVALID_FD,
            "wl_shm_pool.resize: pool file is smaller than the new size",
        );
    }
}
