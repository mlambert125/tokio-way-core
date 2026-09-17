//! A randomised soak over request dispatch.
//!
//! Every other test here names a request and checks what it does. This one
//! names nothing: it throws pseudo-random messages at [`handle_message`] and
//! asserts only that the compositor survives them and stays self-consistent.
//! The two crashes this file was written after — a popup parented to itself,
//! and an out-of-range positioner enum — were both three requests deep and
//! both invisible to tests that only exercise requests a correct client would
//! send.
//!
//! This is cheap because of how the compositor is built. `handle_message` is
//! synchronous and takes `&mut CompositorState`, so a soak needs no runtime, no
//! socket and no clients — it is a loop over a pure-ish function. That is a
//! payoff of keeping the protocol layer a plain state machine, and it is worth
//! collecting.
//!
//! Deterministic on purpose. A seed that fails reproduces exactly, in CI and on
//! the machine you are debugging on, which a wall-clock-seeded generator would
//! not. `cargo-fuzz` would explore far further and belongs alongside this
//! rather than instead of it; what this buys is coverage that runs on every
//! `cargo test` without any tooling at all.

use super::CLIENT;
use tokio::sync::mpsc::channel;
use tokio_util::sync::CancellationToken;
use tokio_way_backends::outputs::OutputId;
use tokio_way_core::protocol::signature::{ArgType, request_at, requests};
use tokio_way_core::protocol::wire_utils::ArgWriter;
use tokio_way_core::protocol::{ObjectType, handle_message};
use tokio_way_core::scene::{SceneCache, build};
use tokio_way_core::state::CompositorState;
use tokio_way_sock::{WaylandRequest, WaylandRequestWithClientInfo};

/// Seeds to run. Each is an independent session from a fresh state.
const SEEDS: u64 = 400;
/// Requests per seed.
const REQUESTS: usize = 250;

/// xorshift64*, so the soak carries no dependency and no hidden state.
///
/// The quality of the generator is not the point — the point is that it is the
/// same sequence every time, on every machine.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Any non-zero state will do; xorshift is stuck at zero.
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap_or(0)
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len())]
    }
}

/// The ids present in a freshly built state, used to seed the generator before
/// anything has been created and as a fallback if the client is ever emptied.
///
/// The set the generator actually draws from is read back out of the client's
/// object map after every request — see [`live_ids`]. A static vocabulary was
/// the single biggest thing holding this soak back: it could reach
/// `wl_shm.create_pool` and make a pool, but the pool's id was one the
/// compositor had just handed out, so no later request could ever name it and
/// `create_buffer` was unreachable. Measured, not guessed: pools went from one
/// to many and buffers from none at all once the ids became dynamic.
const SEED_IDS: &[u32] = &[
    1,  // wl_display
    2,  // wl_registry
    3,  // wl_compositor
    4,  // wl_shm
    5,  // wl_seat
    6,  // xdg_wm_base
    7,  // wl_subcompositor
    8,  // wl_data_device_manager
    9,  // wp_viewporter
    10, // wl_surface
    11, // wl_surface
    12, // wl_surface
    20, // xdg_surface (on 10)
    21, // xdg_surface (on 11)
    22, // xdg_surface (on 12)
    30, // xdg_toplevel (on 20)
    40, // xdg_positioner
    50, // wl_shm_pool
    51, // wl_buffer
    60, // wl_pointer
    61, // wl_keyboard
    62, // wl_touch
    63, // wl_data_device
    70, // wl_region
    71, // wp_viewport
];

/// Every object the client currently holds, which is what the generator names.
///
/// Capped, and sorted so a seed replays identically: a `HashMap`'s iteration
/// order is not stable across runs, and a soak whose failures cannot be
/// reproduced is worth very little.
fn live_ids(state: &mut CompositorState) -> Vec<u32> {
    let mut ids: Vec<u32> = state
        .clients
        .get(CLIENT)
        .map(|client| client.objects.keys().copied().collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return SEED_IDS.to_vec();
    }
    ids.sort_unstable();
    ids.truncate(96);
    ids
}

/// A compositor with an output and one client holding a plausible object graph.
///
/// The graph matters more than its exact shape: a handler reached with nothing
/// to act on returns immediately, so a soak against an empty state tests the
/// early returns and nothing else.
fn populated_state() -> (CompositorState, CancellationToken) {
    let mut state = crate::tests::test_state();
    state.outputs.push(super::super::test_output(OutputId(1)));
    crate::tests::with_ws(&mut state, |w, s| w.workspaces.sync_outputs(&s.outputs));
    let token = register_client(&mut state);
    (state, token)
}

/// Create the client and its object graph. Called again whenever a request
/// gets the client disconnected, so one fatal error does not leave the rest of
/// the run dispatching into an empty state.
fn register_client(state: &mut CompositorState) -> CancellationToken {
    let (tx, _rx) = channel(4096);
    let token = CancellationToken::new();
    state.clients.create(CLIENT, tx, token.clone());

    let globals = [
        (2, ObjectType::WlRegistry),
        (3, ObjectType::WlCompositor),
        (4, ObjectType::WlShm),
        (5, ObjectType::WlSeat),
        (6, ObjectType::XdgWmBase),
        (7, ObjectType::WlSubcompositor),
        (8, ObjectType::WlDataDeviceManager),
        (80, ObjectType::ZwpPrimarySelectionDeviceManager),
        (81, ObjectType::ZwpPrimarySelectionDevice),
        (82, ObjectType::ZwlrDataControlManager),
        (83, ObjectType::ZwlrDataControlDevice),
        (84, ObjectType::WpFractionalScaleManager),
        (85, ObjectType::WpCursorShapeManager),
        (86, ObjectType::WpCursorShapeDevice),
        (9, ObjectType::WpViewporter),
        (60, ObjectType::WlPointer),
        (61, ObjectType::WlKeyboard),
        (62, ObjectType::WlTouch),
        (63, ObjectType::WlDataDevice),
        (70, ObjectType::WlRegion),
        (71, ObjectType::WpViewport),
    ];
    for (id, object_type) in globals {
        let _ = state
            .clients
            .get(CLIENT)
            .unwrap()
            .register_client_object_with_version(id, object_type, 8);
    }

    for surface in [10, 11, 12] {
        state.create_surface(CLIENT, surface);
        let _ = state
            .clients
            .get(CLIENT)
            .unwrap()
            .register_client_object(surface, ObjectType::WlSurface);
    }
    for (xdg, surface) in [(20, 10), (21, 11), (22, 12)] {
        state.create_xdg_surface(CLIENT, xdg, surface);
        let _ = state
            .clients
            .get(CLIENT)
            .unwrap()
            .register_client_object_with_version(xdg, ObjectType::XdgSurface, 5);
    }
    state.create_xdg_toplevel(CLIENT, 30, 20);
    let _ = state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object_with_version(30, ObjectType::XdgToplevel, 5);
    state.create_xdg_positioner(CLIENT, 40);
    let _ = state
        .clients
        .get(CLIENT)
        .unwrap()
        .register_client_object(40, ObjectType::XdgPositioner);
    state.create_region(CLIENT, 70);
    state.create_viewport(CLIENT, 71, 12);
    token
}

/// Size of the memfds the soak offers. Big enough that a pool declared over it
/// is usually accepted, so `create_buffer`'s geometry checks are reached with
/// something real behind them.
const FD_BYTES: u64 = 64 * 1024;

/// Keep a descriptor available for the three requests that carry one.
///
/// Without this the whole shm path is unreachable: `wl_shm.create_pool` is
/// dropped for a missing fd before it ever runs, so no pool and no buffer is
/// ever built and none of the geometry validation is exercised. Measured
/// rather than assumed — an earlier version of this soak reached zero pools
/// and zero buffers across two hundred thousand requests.
fn top_up_descriptors(state: &mut CompositorState) {
    let Some(client) = state.clients.get(CLIENT) else {
        return;
    };
    if !client.fd_queue.is_empty() {
        return;
    }
    // SAFETY: `memfd_create` takes a name and flags and returns a new fd.
    let fd = unsafe { libc::memfd_create(c"soak".as_ptr().cast(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    file.set_len(FD_BYTES).unwrap();
    client.fd_queue.push_back(std::os::fd::OwnedFd::from(file));
}

/// Build a request the compositor can actually decode.
///
/// The signature table is what makes this possible. Guessing argument bytes
/// gets a generator as far as the malformed-arguments arm and no further —
/// measured, not assumed: before the table this soak could build a `wl_shm`
/// pool but never a `wl_buffer`, because `create_buffer` takes six arguments
/// that have to agree with each other and with the pool behind them, and
/// random words never do.
///
/// Every argument is still perturbed. A generator that only ever emitted
/// sensible values would be testing the happy path, which the rest of this
/// directory already does; the point here is well-*formed* rather than
/// well-*behaved* — a request that decodes and then asks for something absurd
/// is exactly what reaches the code that has to say no.
fn shaped_request(
    rng: &mut Rng,
    fresh: &mut u32,
    live: &[u32],
    object_type: ObjectType,
    op_code: u16,
) -> Vec<u8> {
    let Some(request) = request_at(object_type, op_code) else {
        return Vec::new();
    };
    let mut args = ArgWriter::new();
    for arg in request.args {
        args = match arg {
            ArgType::NewId => {
                *fresh += 1;
                args.u32(1000 + *fresh)
            }
            ArgType::Object => {
                // Usually something real, occasionally a null and
                // occasionally an id nobody has.
                match rng.below(8) {
                    0 => args.u32(0),
                    1 => args.u32(u32::try_from(rng.below(4096)).unwrap_or(0)),
                    _ => args.u32(rng.pick(live)),
                }
            }
            ArgType::Int => args.i32(interesting_signed(rng)),
            ArgType::Uint => args.u32(interesting_unsigned(rng)),
            ArgType::Fixed => args.fixed(interesting_signed(rng).into()),
            ArgType::String => args.string(rng.pick(&["", "x", "text/plain", "\u{1f600}"])),
            ArgType::Array => args.array_u32(&[rng.pick(live)]),
            // Out of band; the queue is topped up separately.
            ArgType::Fd => args,
        };
    }
    args.build()
}

/// Signed values worth sending: zero, negative, the extremes where arithmetic
/// overflows, and — most of the time — a size that could plausibly be real.
///
/// The plausible bucket helps, and is honest about how much. Dimensions drawn
/// independently from a wide range essentially never satisfy
/// `wl_shm_pool.create_buffer`, whose five numbers have to agree with each
/// other and with the pool behind them — a stride at least four times the
/// width, an extent inside the pool. Powers of two agree by accident more
/// often. Measured across a full run: `create_buffer` is dispatched around
/// five hundred times and passes its geometry checks once.
///
/// So the `wl_buffer` path is reachable rather than reached. Getting there
/// reliably needs a generator that knows a stride is *derived from* a width,
/// which is semantics no signature table carries — the shape of a request and
/// the relationships between its arguments are different kinds of knowledge.
/// Worth doing, and worth doing as its own thing.
const PLAUSIBLE: &[i32] = &[0, 1, 2, 4, 8, 16, 32, 64, 128, 256];

fn interesting_signed(rng: &mut Rng) -> i32 {
    match rng.below(10) {
        0 => 0,
        1 => -1,
        2 => i32::MIN,
        3 => i32::MAX,
        4 => -(i32::try_from(rng.below(4096)).unwrap_or(0)),
        5 => i32::try_from(rng.below(4096)).unwrap_or(0),
        // A size, or a multiple of one — which is what a stride is.
        _ => rng.pick(PLAUSIBLE) * i32::try_from(1 + rng.below(4)).unwrap_or(1),
    }
}

/// Unsigned values worth sending, weighted toward the small ones.
///
/// Most `uint` arguments in this protocol are an enum or a small count — a
/// pixel format, an anchor, a transform, a set of flags — and only a few are
/// genuinely a wide number. Drawing uniformly from the whole range means the
/// enum-valued ones are almost never valid, which quietly gates off whatever
/// sits behind them: `wl_shm_pool.create_buffer` checks its format before it
/// checks anything else, so a soak that rarely names a real format rarely
/// reaches the geometry checks at all.
fn interesting_unsigned(rng: &mut Rng) -> u32 {
    match rng.below(10) {
        0 => u32::MAX,
        1 => 1 << rng.below(32),
        2 => u32::try_from(rng.below(4096)).unwrap_or(0),
        // Small enough to be an enum value or a flag mask, which is what most
        // of them are.
        _ => u32::try_from(rng.below(10)).unwrap_or(0),
    }
}

/// Pick an object and an opcode, and build the request that goes with them.
///
/// Mostly a live object and an opcode its interface really has, which is what
/// gets past dispatch and into a handler. The rest of the time it is
/// deliberately wrong — an object nobody owns, or an opcode past the end of an
/// interface — because refusing those correctly is its own behaviour worth
/// exercising.
fn random_message(
    rng: &mut Rng,
    fresh: &mut u32,
    live: &[u32],
    state: &mut CompositorState,
) -> WaylandRequestWithClientInfo {
    let object_id = if rng.below(16) == 0 {
        u32::try_from(rng.below(4096)).unwrap_or(0)
    } else {
        rng.pick(live)
    };
    let object_type = state
        .clients
        .get(CLIENT)
        .and_then(|client| client.objects.get(&object_id).copied());

    let (op_code, args) = match object_type {
        Some(object_type) if rng.below(16) > 0 => {
            let count = requests(object_type).len();
            if count == 0 {
                (0, Vec::new())
            } else {
                let op_code = u16::try_from(rng.below(count)).unwrap_or(0);
                (
                    op_code,
                    shaped_request(rng, fresh, live, object_type, op_code),
                )
            }
        }
        // An opcode no interface has, or an object that does not exist.
        _ => (
            u16::try_from(rng.below(64)).unwrap_or(0),
            (0..rng.below(6))
                .map(|_| interesting_unsigned(rng))
                .flat_map(u32::to_le_bytes)
                .collect(),
        ),
    };

    WaylandRequestWithClientInfo {
        client_id: CLIENT,
        message: WaylandRequest {
            object_id,
            op_code,
            args,
        },
    }
}

/// No surface may be its own ancestor.
///
/// The invariant worth checking after every single request rather than at the
/// end of a run: composing and hit-testing both recurse the surface tree, so a
/// cycle is a stack overflow — which aborts the whole test binary and reports
/// nothing useful. Catching it here fails one assertion instead, and names the
/// seed and the request that did it.
fn tree_is_acyclic(state: &CompositorState) -> bool {
    state.surfaces.keys().all(|&(client_id, surface_id)| {
        state.surfaces[&(client_id, surface_id)]
            .parent
            .is_none_or(|parent| !state.is_ancestor(client_id, surface_id, parent))
    })
}

#[test]
fn a_soak_of_random_requests_leaves_the_compositor_standing() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let (mut state, mut token) = populated_state();
        let mut cache = SceneCache::new();
        let mut fresh = 0u32;

        for request in 0..REQUESTS {
            top_up_descriptors(&mut state);
            let live = live_ids(&mut state);
            let message = random_message(&mut rng, &mut fresh, &live, &mut state);
            let (object_id, op_code) = (message.message.object_id, message.message.op_code);
            handle_message(&mut state, &message);

            assert!(
                tree_is_acyclic(&state),
                "seed {seed}, request {request}: object {object_id} opcode {op_code} \
                 put a surface in its own ancestry",
            );

            // A fatal error cancels the client's token, and the compositor
            // loop — which there is none of here — is what would then tear the
            // client down. Doing that after *every* fatal request turned out
            // to be self-defeating: roughly three quarters of random requests
            // are fatal, because the compositor is correctly strict about
            // arguments it cannot decode, so the soak spent its whole run
            // rebuilding a client rather than exploring one. A cancelled
            // client still dispatches perfectly well here, so the state is
            // kept and the teardown is exercised on a schedule instead.
            if token.is_cancelled() && request % 50 == 0 {
                state.remove_client_resources(CLIENT);
                state.clients.remove(CLIENT);
                token = register_client(&mut state);
            }

            // Periodically walk everything the request may have changed. These
            // are the recursive and iterative passes over compositor state, and
            // they are where a malformed object graph stops being inert.
            if request % 25 == 0 {
                drop(build(OutputId(1), request as u64, &state, &mut cache));
                tokio_way_core::input::touch_down(&mut state, 0, 1, 40.0, 40.0);
                tokio_way_core::input::touch_up(&mut state, 0, 1);
                crate::tests::with_ws(&mut state, crate::tests::TestShell::rehome_toplevels);
                tokio_way_core::input::start_buffer_releases(&mut state);
                tokio_way_core::input::finish_buffer_releases(&mut state);
            }
        }
    }
}
