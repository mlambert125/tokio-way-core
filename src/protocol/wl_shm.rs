//! `wl_shm` protocol handler.
//!
//! Manages shared memory for buffer allocation. Clients bind `wl_shm` to
//! discover supported pixel formats, then call `create_pool` with a file
//! descriptor to an mmap-able region. Pools are used to allocate `wl_buffers`.

use std::os::fd::{IntoRawFd, OwnedFd};

use tracing::debug;

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::ClientState;
use super::super::state::CompositorState;
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};

// Request opcodes
pub const CREATE_POOL: u16 = 0;

// Event opcodes
const FORMAT: u16 = 0;

// Pixel format constants (from wayland-protocol's wl_shm.xml)
pub const FORMAT_ARGB8888: u32 = 0;
pub const FORMAT_XRGB8888: u32 = 1;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo, fds: Vec<OwnedFd>) {
    match msg.message.op_code {
        CREATE_POOL => handle_create_pool(state, msg, fds),
        _ => super::reject_unknown_request(state, msg, "wl_shm"),
    }
}

/// Validation happens before the fd is taken out of `fds`, so every error path
/// simply drops it and the descriptor is closed. Nothing here has to unwind fds
/// by hand, and no path can leave the client's fd queue misaligned.
fn handle_create_pool(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    mut fds: Vec<OwnedFd>,
) {
    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    // create_pool args: new_id, fd (passed out-of-band), int32 size
    let (Some(pool_id), Some(size)) = (args.new_id(), args.i32()) else {
        client.send_error(
            msg.message.object_id,
            0,
            "wl_shm.create_pool: malformed args",
        );
        return;
    };

    if size <= 0 {
        client.send_error(msg.message.object_id, 0, "wl_shm.create_pool: invalid size");
        return;
    }

    // Guaranteed present by `request_fd_count`, which drops any client that
    // omits it, but do not panic if that ever stops being true.
    if fds.is_empty() {
        client.send_error(msg.message.object_id, 0, "wl_shm.create_pool: missing fd");
        return;
    }

    // Register before taking the descriptor out of `fds`. Every failure up to
    // this point leaves it owned by the `Vec<OwnedFd>`, so returning closes it;
    // once it becomes a `RawFd` below, nothing would.
    if client
        .register_client_object(pool_id, ObjectType::WlShmPool)
        .is_err()
    {
        return;
    }

    let fd = fds.remove(0).into_raw_fd();
    debug!(
        "wl_shm.create_pool: pool_id={} fd={} size={}",
        pool_id, fd, size
    );
    // Ownership of the raw fd passes to the pool, which closes it in
    // `try_cleanup_pool`.
    if !state.register_shm_pool(msg.client_id, pool_id, fd, size.unsigned_abs())
        && let Some(client) = state.clients.get(msg.client_id)
    {
        // WL_SHM_ERROR_INVALID_FD = 2
        client.send_error(
            msg.message.object_id,
            2,
            "wl_shm.create_pool: fd is not a readable file of at least the given size",
        );
    }
}

/// Send `wl_shm.format` events for all supported pixel formats.
pub fn send_formats(client: &mut ClientState, shm_id: u32) {
    for &fmt in &[FORMAT_ARGB8888, FORMAT_XRGB8888] {
        let args = ArgWriter::new().u32(fmt).build();
        if client.send(build_message(shm_id, FORMAT, args)).is_err() {
            return;
        }
    }
}
