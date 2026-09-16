//! `zwp_linux_buffer_params_v1` protocol handler.
//!
//! A buffer under construction: the client adds one descriptor per plane, then
//! asks for a `wl_buffer`. The object is single-use — after `create` or
//! `create_immed` the only legal request is `destroy`.
//!
//! The two creation requests differ in who names the `wl_buffer` and therefore
//! in what can still be refused. `create` lets the compositor answer `created`
//! with an id of its own, or `failed`, so a client that cannot use dma-buf
//! learns in time to fall back. `create_immed` has the client name the id up
//! front, so there is nothing to refuse: a buffer that turns out not to import
//! is registered anyway, as [`BufferKind::Failed`], and simply draws nothing.
//! That is what the protocol means by "the server creates an invalid
//! `wl_buffer`, marks it as failed" — tearing the object down instead would
//! leave the client owning an id the compositor has forgotten, and the next
//! request naming it would disconnect them.
//!
//! Errors split the same way throughout: a malformed request is a fatal
//! protocol error, and "the driver will not take this" is the non-fatal
//! `failed` event, because only the second is something a client can recover
//! from.

use super::super::state::{
    Buffer, BufferKind, ClientObjectId, CompositorState, MAX_DMABUF_PLANES, PendingImport,
    PendingPlane,
};
use super::ObjectType;
use super::wire_utils::{ArgReader, ArgWriter, build_message};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use tokio_way_backends::dma::{
    DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DmabufImage, DmabufPlane,
};
use tokio_way_backends::messages::BackendRequest;
use tokio_way_sock::WaylandRequestWithClientInfo;
use tracing::debug;

// Request opcodes
pub const DESTROY: u16 = 0;
/// Carries a file descriptor — see `super::request_fd_count`.
pub const ADD: u16 = 1;
pub const CREATE: u16 = 2;
pub const CREATE_IMMED: u16 = 3;

// Event opcodes
const CREATED: u16 = 0;
const FAILED: u16 = 1;

// zwp_linux_buffer_params_v1.error
/// The object has already been used to create a buffer.
const ERROR_ALREADY_USED: u32 = 0;
/// Plane index out of bounds.
const ERROR_PLANE_IDX: u32 = 1;
/// The plane was already set.
const ERROR_PLANE_SET: u32 = 2;
/// The plane set is missing planes.
const ERROR_INCOMPLETE: u32 = 3;
/// Format not supported.
const ERROR_INVALID_FORMAT: u32 = 4;
/// Invalid width or height.
const ERROR_INVALID_DIMENSIONS: u32 = 5;
/// Offset + stride exceed the descriptor.
const ERROR_OUT_OF_BOUNDS: u32 = 6;

/// Bytes per pixel assumed when checking a plane against its descriptor.
///
/// Every format this compositor can sample is 32-bit; the bound is only used to
/// prove a plane fits, and for anything wider it is conservative rather than
/// wrong.
const BYTES_PER_PIXEL: u64 = 4;

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo, fds: Vec<OwnedFd>) {
    match msg.message.op_code {
        DESTROY => handle_destroy(state, msg),
        ADD => handle_add(state, msg, fds),
        CREATE => handle_create(state, msg, None),
        CREATE_IMMED => handle_create_immed(state, msg),
        _ => super::unknown_request(state, msg, "zwp_linux_buffer_params_v1"),
    }
}

fn handle_destroy(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let key = (msg.client_id, msg.message.object_id);
    state.dmabuf_params.remove(&key);
    // An import still in flight has nowhere to report back to, and holding its
    // descriptors any longer would be a leak. Cancelling is legitimate: a
    // client may destroy the object to abandon a buffer it no longer wants.
    state
        .pending_dmabuf_imports
        .retain(|_, pending| (pending.client_id, pending.params_id) != key);

    if let Some(client) = state.clients.get(msg.client_id) {
        client.unregister(msg.message.object_id);
    }
}

/// Add one plane's descriptor.
///
/// Every error path returns before the descriptor is taken out of `fds`, so it
/// is closed by the `Vec` being dropped — the same discipline
/// `wl_shm::handle_create_pool` keeps, and for the same reason: nothing here
/// has to unwind descriptors by hand.
fn handle_add(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    mut fds: Vec<OwnedFd>,
) {
    let key = (msg.client_id, msg.message.object_id);
    let used = state.dmabuf_params.get(&key).is_some_and(|p| p.used);
    let occupied = |index: usize| {
        state
            .dmabuf_params
            .get(&key)
            .is_some_and(|p| p.planes.get(index).is_some_and(Option::is_some))
    };

    let Some(client) = state.clients.get(msg.client_id) else {
        tracing::warn!("Received message from unknown client {}", msg.client_id);
        return;
    };

    let mut args = ArgReader::new(&msg.message.args);
    let (Some(plane_idx), Some(offset), Some(stride), Some(modifier_hi), Some(modifier_lo)) =
        (args.u32(), args.u32(), args.u32(), args.u32(), args.u32())
    else {
        client.send_error(
            msg.message.object_id,
            0,
            "zwp_linux_buffer_params_v1.add: malformed args",
        );
        return;
    };

    if used {
        fatal(
            client,
            msg.message.object_id,
            ERROR_ALREADY_USED,
            "already used",
        );
        return;
    }
    let Ok(index) = usize::try_from(plane_idx) else {
        fatal(
            client,
            msg.message.object_id,
            ERROR_PLANE_IDX,
            "plane index out of bounds",
        );
        return;
    };
    if index >= MAX_DMABUF_PLANES {
        fatal(
            client,
            msg.message.object_id,
            ERROR_PLANE_IDX,
            "plane index out of bounds",
        );
        return;
    }
    if occupied(index) {
        fatal(
            client,
            msg.message.object_id,
            ERROR_PLANE_SET,
            "plane already set",
        );
        return;
    }
    if fds.is_empty() {
        client.send_error(
            msg.message.object_id,
            0,
            "zwp_linux_buffer_params_v1.add: missing fd",
        );
        return;
    }

    let modifier = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
    let plane = PendingPlane {
        plane: DmabufPlane {
            fd: Arc::new(fds.remove(0)),
            offset,
            stride,
        },
        modifier,
    };
    if let Some(params) = state.dmabuf_params.get_mut(&key)
        && let Some(slot) = params.planes.get_mut(index)
    {
        *slot = Some(plane);
    }
}

/// `create`: the compositor names the buffer, so it can still say no.
fn handle_create(
    state: &mut CompositorState,
    msg: &WaylandRequestWithClientInfo,
    immediate_id: Option<u32>,
) {
    let key = (msg.client_id, msg.message.object_id);
    let mut args = ArgReader::new(&msg.message.args);
    // `create_immed` has already taken the new_id off the front.
    let (Some(width), Some(height), Some(format), Some(flags)) =
        (args.i32(), args.i32(), args.u32(), args.u32())
    else {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                0,
                "zwp_linux_buffer_params_v1.create: malformed args",
            );
        }
        return;
    };

    let description = match describe(state, key, width, height, format, flags) {
        Ok(image) => image,
        Err(Refusal::Fatal(code, reason)) => {
            if let Some(client) = state.clients.get(msg.client_id) {
                fatal(client, msg.message.object_id, code, reason);
            }
            return;
        }
        Err(Refusal::Failed(reason)) => {
            debug!("dma-buf refused for client {}: {reason}", msg.client_id);
            refuse(state, key, immediate_id, width, height);
            return;
        }
    };

    // For `create_immed` the client already owns the id, so the buffer exists
    // from this moment whatever the driver later says.
    let immediate = immediate_id.map(|buffer_id| {
        let serial = register_dmabuf(state, msg.client_id, buffer_id, &description, width, height);
        (buffer_id, serial)
    });

    let token = state.next_import_token;
    state.next_import_token += 1;
    let request = BackendRequest::ImportDmabuf {
        token,
        image: description.clone(),
    };
    // Only the backend thread can answer, and if it cannot be reached the
    // client must be told now rather than left waiting for a verdict that
    // cannot arrive.
    let dispatched = state
        .backend_sender
        .as_ref()
        .is_some_and(|requests| requests.try_send(request).is_ok());
    if !dispatched {
        debug!(
            "no backend to verify a dma-buf import for client {}",
            msg.client_id
        );
        refuse(state, key, immediate_id, width, height);
        return;
    }

    state.pending_dmabuf_imports.insert(
        token,
        PendingImport {
            client_id: msg.client_id,
            params_id: msg.message.object_id,
            immediate,
            image: description,
            width,
            height,
        },
    );
}

/// `create_immed`: the client names the buffer, so nothing can be refused.
fn handle_create_immed(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    let mut args = ArgReader::new(&msg.message.args);
    // The new_id comes first here, unlike `create`.
    let Some(buffer_id) = args.new_id() else {
        if let Some(client) = state.clients.get(msg.client_id) {
            client.send_error(
                msg.message.object_id,
                0,
                "zwp_linux_buffer_params_v1.create_immed: malformed args",
            );
        }
        return;
    };
    let Some(client) = state.clients.get(msg.client_id) else {
        return;
    };
    if client.register(buffer_id, ObjectType::WlBuffer).is_err() {
        return;
    }

    // The remaining arguments are `create`'s, so the rest of the path is shared.
    let rest = WaylandRequestWithClientInfo {
        client_id: msg.client_id,
        message: tokio_way_sock::WaylandRequest {
            object_id: msg.message.object_id,
            op_code: msg.message.op_code,
            args: msg.message.args[4..].to_vec(),
        },
    };
    handle_create(state, &rest, Some(buffer_id));
}

/// Why a buffer will not be made.
enum Refusal {
    /// The request itself is malformed: fatal, with a `zwp_linux_buffer_params_v1.error`.
    Fatal(u32, &'static str),
    /// The request is well formed and cannot be honoured: the `failed` event,
    /// which is what lets a client fall back to shm.
    Failed(String),
}

/// Turn a described set of planes into something importable, or say why not.
///
/// Everything checkable is checked here, before any of it reaches the backend:
/// a fatal error must not be deferred behind a round trip.
fn describe(
    state: &mut CompositorState,
    key: ClientObjectId,
    width: i32,
    height: i32,
    format: u32,
    flags: u32,
) -> Result<Arc<DmabufImage>, Refusal> {
    let Some(params) = state.dmabuf_params.get_mut(&key) else {
        return Err(Refusal::Fatal(ERROR_ALREADY_USED, "no such params object"));
    };
    if params.used {
        return Err(Refusal::Fatal(ERROR_ALREADY_USED, "already used"));
    }
    // Set before anything can fail asynchronously, so a second create is
    // refused even while the first is still in flight.
    params.used = true;

    if width <= 0 || height <= 0 {
        return Err(Refusal::Fatal(
            ERROR_INVALID_DIMENSIONS,
            "width and height must be positive",
        ));
    }

    // Planes must run 0..n with no gaps: a set with a hole describes nothing.
    let count = params
        .planes
        .iter()
        .position(Option::is_none)
        .unwrap_or(MAX_DMABUF_PLANES);
    if count == 0 || params.planes[count..].iter().any(Option::is_some) {
        return Err(Refusal::Fatal(
            ERROR_INCOMPLETE,
            "planes must be set from zero with no gaps",
        ));
    }

    // One buffer has one layout, but the protocol carries a modifier per plane.
    let modifier = params.planes[0].as_ref().map_or(0, |p| p.modifier);
    if params.planes[..count]
        .iter()
        .flatten()
        .any(|p| p.modifier != modifier)
    {
        return Err(Refusal::Fatal(
            ERROR_INVALID_FORMAT,
            "planes disagree about the format modifier",
        ));
    }

    if let Some(plane) = params.planes[0].as_ref() {
        check_bounds(&plane.plane, width, height, modifier)?;
    }

    if flags != 0 {
        // y_invert needs a flip this renderer has no way to express, and
        // interlaced content needs a deinterlacer it does not have. Refusing is
        // what the protocol recommends over showing something wrong.
        return Err(Refusal::Failed(format!("unsupported flags {flags:#x}")));
    }
    if !state.dmabuf_formats.iter().any(|f| f.fourcc == format) {
        return Err(Refusal::Failed(format!(
            "format {} was never advertised",
            tokio_way_backends::dma::fourcc_name(format)
        )));
    }

    let planes = params.planes[..count]
        .iter()
        .flatten()
        .map(|p| DmabufPlane {
            fd: p.plane.fd.clone(),
            offset: p.plane.offset,
            stride: p.plane.stride,
        })
        .collect();
    Ok(Arc::new(DmabufImage {
        width,
        height,
        fourcc: format,
        modifier,
        planes,
    }))
}

/// Check that the first plane fits inside the descriptor it names.
///
/// The only bound on what a client can make the driver read: without it a
/// stride or offset naming memory past the end of the buffer is passed straight
/// through to EGL. A dma-buf answers `lseek(SEEK_END)` with its size, so this
/// costs one syscall.
///
/// Only for layouts whose extent can be worked out from the outside — linear,
/// or implicit and so presumed linear. Under a tiling or compression modifier
/// the bytes are not `stride` per row and the later planes are metadata with
/// their own geometry entirely: computing an extent for those says nothing
/// about the buffer, and an earlier version of this check rejected every
/// compressed buffer a Vulkan client offered. There the driver knows the real
/// layout and does its own bounds checking at import.
fn check_bounds(
    plane: &DmabufPlane,
    width: i32,
    height: i32,
    modifier: u64,
) -> Result<(), Refusal> {
    if modifier != DRM_FORMAT_MOD_INVALID && modifier != DRM_FORMAT_MOD_LINEAR {
        return Ok(());
    }
    // SAFETY: `fd` is a live descriptor owned by the plane, and `lseek` only
    // reads its offset table.
    let size = unsafe { libc::lseek(plane.fd.as_raw_fd(), 0, libc::SEEK_END) };
    if size <= 0 {
        // Not every descriptor answers; refusing one that cannot be checked
        // would reject buffers that work.
        return Ok(());
    }
    let rows = u64::from(height.unsigned_abs());
    let extent = u64::from(plane.offset)
        + u64::from(plane.stride) * (rows - 1)
        + u64::from(width.unsigned_abs()) * BYTES_PER_PIXEL;
    if extent > size.unsigned_abs() {
        debug!(
            "plane out of bounds: offset={} stride={} {}x{} extent={extent} fd_size={size}",
            plane.offset, plane.stride, width, height,
        );
        return Err(Refusal::Fatal(
            ERROR_OUT_OF_BOUNDS,
            "plane runs past the end of its descriptor",
        ));
    }
    Ok(())
}

/// Tell a client its buffer could not be made.
///
/// For `create` that is the `failed` event and nothing else. For `create_immed`
/// the client already holds the id, so the object stays registered and becomes
/// a buffer that draws nothing.
fn refuse(
    state: &mut CompositorState,
    key: ClientObjectId,
    immediate_id: Option<u32>,
    width: i32,
    height: i32,
) {
    if let Some(buffer_id) = immediate_id {
        let content_serial = state.next_content_serial();
        state.buffers.insert(
            (key.0, buffer_id),
            Buffer {
                client_id: key.0,
                width,
                height,
                content_serial,
                kind: BufferKind::Failed,
            },
        );
    }
    send_failed(state, key);
}

/// Send `failed`, if the params object is still there to send it on.
fn send_failed(state: &mut CompositorState, key: ClientObjectId) {
    if !state.dmabuf_params.contains_key(&key) {
        return;
    }
    if let Some(client) = state.clients.get(key.0) {
        let _ = client.send(build_message(key.1, FAILED, Vec::new()));
    }
}

/// Put an imported buffer in the registry, and give back the serial it got.
fn register_dmabuf(
    state: &mut CompositorState,
    client_id: u32,
    buffer_id: u32,
    image: &Arc<DmabufImage>,
    width: i32,
    height: i32,
) -> u64 {
    let content_serial = state.next_content_serial();
    state.releasing_buffers.remove(&(client_id, buffer_id));
    state.buffers.insert(
        (client_id, buffer_id),
        Buffer {
            client_id,
            width,
            height,
            content_serial,
            kind: BufferKind::Dmabuf(image.clone()),
        },
    );
    content_serial
}

/// Act on the backend's verdict for one import.
///
/// Everything it names may be gone by now — the client, the params object, the
/// buffer, or a buffer that has been destroyed and had its id reused — so each
/// is checked rather than assumed.
pub fn resolve_import(state: &mut CompositorState, token: u64, imported: bool) {
    let Some(pending) = state.pending_dmabuf_imports.remove(&token) else {
        // Cancelled: the params object or the client went away.
        return;
    };
    let key = (pending.client_id, pending.params_id);

    if let Some((buffer_id, serial)) = pending.immediate {
        // `create_immed`: the buffer exists either way. A verdict for a buffer
        // that has since been destroyed — possibly with its id already reused —
        // must not touch whatever holds that id now.
        let current = state.buffers.get(&(pending.client_id, buffer_id));
        if current.is_none_or(|b| b.content_serial != serial) {
            return;
        }
        if !imported {
            debug!("dma-buf {buffer_id} did not import; it will draw nothing");
            if let Some(buffer) = state.buffers.get_mut(&(pending.client_id, buffer_id)) {
                buffer.kind = BufferKind::Failed;
            }
            send_failed(state, key);
        }
        return;
    }

    if !imported {
        send_failed(state, key);
        return;
    }

    // `create`: the compositor names the buffer, and only now that there is
    // one to name.
    let Some(client) = state.clients.get(pending.client_id) else {
        return;
    };
    let Some(buffer_id) = client.allocate_id(ObjectType::WlBuffer) else {
        send_failed(state, key);
        return;
    };
    let args = ArgWriter::new().u32(buffer_id).build();
    let _ = client.send(build_message(pending.params_id, CREATED, args));
    register_dmabuf(
        state,
        pending.client_id,
        buffer_id,
        &pending.image,
        pending.width,
        pending.height,
    );
}

/// Send a `zwp_linux_buffer_params_v1.error`, which is fatal by protocol.
///
/// Logged as well as sent: this disconnects the client, and a client that
/// vanishes with no explanation on either side is the hardest kind of bug to
/// find from the outside.
fn fatal(client: &super::super::state::ClientState, object_id: u32, code: u32, reason: &str) {
    tracing::warn!("zwp_linux_buffer_params_v1 error {code} on object {object_id}: {reason}");
    client.send_error(object_id, code, reason);
    client.cancel_token.cancel();
}
