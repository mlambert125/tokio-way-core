//! End-to-end tests over a real Unix socket.
//!
//! These drive the compositor the way a client does: connect, bind globals,
//! send requests, read events. Everything else in this suite calls
//! `handle_message` directly and so never touches the socket subsystem at all
//! — not the framing, not the send task, not descriptor passing. Those are
//! most of what a real client's first second exercises.
//!
//! Slower than the rest and worth it. Each test starts a compositor of its
//! own, so they cannot interfere, and every wait is bounded so a fault is a
//! failure rather than a hung CI run.

mod harness;

use harness::{Client, Harness, args, memfd, raw};
use tokio_way_core::protocol::{GLOBALS, wire_utils::ArgReader};

// Object ids this side allocates. The client owns everything below
// SERVER_ID_BASE and is free to number it as it likes.
const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const CALLBACK: u32 = 3;

// wl_display
const GET_REGISTRY: u16 = 1;
const SYNC: u16 = 0;
const ERROR: u16 = 0;
const DELETE_ID: u16 = 1;

// wl_registry
const BIND: u16 = 0;
const GLOBAL: u16 = 0;

// Ids and sizes for the shm test.
/// `wl_shm.create_pool`, the one request in this file that carries a descriptor.
const CREATE_POOL: u16 = 0;

const SHM: u32 = 10;
const POOL: u32 = 11;
const BUFFER: u32 = 12;
const POOL_BYTES: u64 = 64 * 64 * 4;

/// One advertised global: its name, interface and version.
#[derive(Debug, PartialEq, Eq)]
struct Global {
    name: u32,
    interface: String,
    version: u32,
}

fn decode_global(args: &[u8]) -> Global {
    let mut reader = ArgReader::new(args);
    Global {
        name: reader.u32().unwrap(),
        interface: reader.string().unwrap(),
        version: reader.u32().unwrap(),
    }
}

/// Do what every client does first: ask for the registry and read what is on
/// it. Returns the globals, so a test can bind by name rather than by guessing
/// an index.
async fn handshake(client: &mut Client) -> Vec<Global> {
    client
        .send(DISPLAY, GET_REGISTRY, &args().u32(REGISTRY).build())
        .await;
    client
        .send(DISPLAY, SYNC, &args().u32(CALLBACK).build())
        .await;

    // The sync callback is the client's marker for "everything the registry
    // had to say has been said" — the same trick a real client uses, and the
    // reason wl_display.sync exists.
    let mut globals = Vec::new();
    loop {
        let event = client
            .wait_for("the registry listing or its sync", |e| {
                (e.object_id == REGISTRY && e.op_code == GLOBAL) || e.object_id == CALLBACK
            })
            .await;
        if event.object_id == CALLBACK {
            return globals;
        }
        globals.push(decode_global(&event.args));
    }
}

fn find<'a>(globals: &'a [Global], interface: &str) -> &'a Global {
    globals
        .iter()
        .find(|g| g.interface == interface)
        .unwrap_or_else(|| panic!("{interface} was never advertised"))
}

#[tokio::test]
async fn a_client_can_complete_the_handshake_every_client_starts_with() {
    let harness = Harness::start().await;
    let mut client = harness.connect().await;

    let globals = handshake(&mut client).await;

    // Every static global, at the version the compositor claims. A client that
    // binds a version the compositor does not have gets an object it cannot
    // use, so the advertised numbers are part of the contract.
    for expected in GLOBALS {
        let advertised = find(&globals, expected.interface);
        assert_eq!(
            advertised.version, expected.version,
            "{} is advertised at the wrong version",
            expected.interface,
        );
    }
    assert_eq!(
        globals.len(),
        GLOBALS.len(),
        "with no outputs and no dma-buf, the static globals are all there is: {globals:?}",
    );
}

#[tokio::test]
async fn a_sync_is_answered_and_its_id_given_back() {
    let harness = Harness::start().await;
    let mut client = harness.connect().await;

    client
        .send(DISPLAY, SYNC, &args().u32(CALLBACK).build())
        .await;

    // done(callback_data), then wl_display.delete_id naming the callback.
    // libwayland recycles ids eagerly, so a callback id never announced as
    // free is one the next object is given — and the compositor would then
    // reject it as already in use.
    client
        .wait_for("wl_callback.done", |e| e.object_id == CALLBACK)
        .await;
    let deleted = client
        .wait_for("wl_display.delete_id", |e| {
            e.object_id == DISPLAY && e.op_code == DELETE_ID
        })
        .await;
    assert_eq!(
        ArgReader::new(&deleted.args).u32(),
        Some(CALLBACK),
        "the id given back must be the callback's",
    );
}

#[tokio::test]
async fn a_descriptor_travels_with_the_request_that_carries_it() {
    // The whole point of this file. Descriptors arrive out of band and are
    // paired with messages by counting, so nothing below the socket can test
    // it: `handle_message` is handed a queue somebody else filled.
    let harness = Harness::start().await;
    let mut client = harness.connect().await;
    let globals = handshake(&mut client).await;

    let shm = find(&globals, "wl_shm");
    client
        .send(
            REGISTRY,
            BIND,
            &args()
                .u32(shm.name)
                .string("wl_shm")
                .u32(shm.version)
                .u32(SHM)
                .build(),
        )
        .await;

    // wl_shm.create_pool(new_id, fd, size) — the fd is not in these bytes.
    let fd = memfd(POOL_BYTES);
    client
        .send_with_fds(
            SHM,
            0,
            &args()
                .u32(POOL)
                .i32(i32::try_from(POOL_BYTES).unwrap())
                .build(),
            &[raw(&fd)],
        )
        .await;

    // wl_shm_pool.create_buffer(new_id, offset, w, h, stride, format).
    // Succeeding proves the pool was mapped, which proves the descriptor
    // arrived and was paired with the right request.
    client
        .send(
            POOL,
            0,
            &args()
                .u32(BUFFER)
                .i32(0)
                .i32(64)
                .i32(64)
                .i32(64 * 4)
                .u32(0)
                .build(),
        )
        .await;
    client
        .send(DISPLAY, SYNC, &args().u32(CALLBACK).build())
        .await;

    let events = client.drain().await;
    let errors: Vec<_> = events
        .iter()
        .filter(|e| e.object_id == DISPLAY && e.op_code == ERROR)
        .collect();
    assert!(
        errors.is_empty(),
        "creating a pool and a buffer from a real descriptor must not error: {errors:?}",
    );
    // And positively: the sync came back, so the connection is still up. A
    // descriptor that never arrived would have failed the pool, and a failed
    // pool is fatal — the client would be gone rather than merely quiet.
    assert!(
        events.iter().any(|e| e.object_id == CALLBACK),
        "the connection must still be alive: {events:?}",
    );
}

#[tokio::test]
async fn a_descriptor_outlives_the_read_that_delivered_it() {
    // A `sendmsg` can carry a descriptor and stop partway through the request
    // that claims it. That read delivers a descriptor and no whole message, so
    // the queue it joins has to outlive the read — batching descriptors per
    // read is a transport detail, not a scope.
    let harness = Harness::start().await;
    let mut client = harness.connect().await;
    let globals = handshake(&mut client).await;

    let shm = find(&globals, "wl_shm");
    client
        .send(
            REGISTRY,
            BIND,
            &args()
                .u32(shm.name)
                .string("wl_shm")
                .u32(shm.version)
                .u32(SHM)
                .build(),
        )
        .await;

    // Let the compositor finish that read first. A read is not aligned to
    // `sendmsg` boundaries, so a bind still in the socket would be glued onto
    // the front of the next one and the descriptor would arrive alongside a
    // whole message after all — which is the case the test is not about.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // wl_shm.create_pool(new_id, fd, size), built by hand so it can be cut.
    let payload = args()
        .u32(POOL)
        .i32(i32::try_from(POOL_BYTES).unwrap())
        .build();
    let length = u32::try_from(payload.len() + 8).unwrap();
    let mut message = Vec::new();
    message.extend_from_slice(&SHM.to_le_bytes());
    message.extend_from_slice(&((length << 16) | u32::from(CREATE_POOL)).to_le_bytes());
    message.extend_from_slice(&payload);

    // The descriptor rides on a write that stops inside the header, so the
    // read it arrives in completes no message at all.
    let fd = memfd(POOL_BYTES);
    client.send_raw_with_fds(&message[..4], &[raw(&fd)]).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    client.send_raw(&message[4..]).await;

    // The pool only maps if the descriptor was still there when the request
    // finally arrived, and the buffer only builds if the pool mapped.
    client
        .send(
            POOL,
            0,
            &args()
                .u32(BUFFER)
                .i32(0)
                .i32(64)
                .i32(64)
                .i32(64 * 4)
                .u32(0)
                .build(),
        )
        .await;
    client
        .send(DISPLAY, SYNC, &args().u32(CALLBACK).build())
        .await;

    let events = client.drain().await;
    let errors: Vec<_> = events
        .iter()
        .filter(|e| e.object_id == DISPLAY && e.op_code == ERROR)
        .collect();
    assert!(
        errors.is_empty(),
        "a descriptor that arrived before its request must still be claimed: {errors:?}",
    );
    assert!(
        events.iter().any(|e| e.object_id == CALLBACK),
        "the connection must still be alive: {events:?}",
    );
}

#[tokio::test]
async fn a_client_is_told_why_it_was_disconnected() {
    // The compositor queues a wl_display.error and cancels the client's token
    // on the very next line. The send task has to write the message before it
    // notices the cancellation, or the client learns nothing and sees a bare
    // EOF. Nothing below the socket can check this: it is a race between two
    // tasks, and the whole failure mode is a message that never gets written.
    let harness = Harness::start().await;
    let mut client = harness.connect().await;

    // wl_display has two requests. Opcode 99 is not one of them, and being
    // wrong about which interface an object is makes every later request on
    // the connection suspect — so it is fatal.
    client.send(DISPLAY, 99, &[]).await;

    let error = client
        .wait_for("wl_display.error", |e| {
            e.object_id == DISPLAY && e.op_code == ERROR
        })
        .await;

    let mut reader = ArgReader::new(&error.args);
    assert_eq!(reader.u32(), Some(DISPLAY), "the object at fault");
    let _code = reader.u32().unwrap();
    let message = reader.string().unwrap();
    assert!(
        message.contains("has no request"),
        "the error should say what was wrong, got {message:?}",
    );

    assert!(
        client.wait_for_close().await,
        "a wl_display.error is fatal, so the connection must then close",
    );
}

#[tokio::test]
async fn one_clients_fatal_error_does_not_touch_another() {
    // Every client shares one compositor task, and a fatal error cancels a
    // token that is a child of the global one. Cancelling the wrong token, or
    // the parent, would take every client down together — which is the kind of
    // thing only a second connection can notice.
    let harness = Harness::start().await;
    let mut victim = harness.connect().await;
    let mut bystander = harness.connect().await;

    let globals = handshake(&mut bystander).await;
    assert!(!globals.is_empty());

    victim.send(DISPLAY, 99, &[]).await;
    assert!(victim.wait_for_close().await, "the offender is dropped");

    // The bystander must still be served.
    bystander
        .send(DISPLAY, SYNC, &args().u32(CALLBACK + 1).build())
        .await;
    bystander
        .wait_for("the bystander's sync", |e| e.object_id == CALLBACK + 1)
        .await;
}

#[tokio::test]
async fn a_client_that_hangs_up_is_cleaned_up_without_disturbing_anyone() {
    let harness = Harness::start().await;
    let mut first = harness.connect().await;
    handshake(&mut first).await;

    {
        // Connect, build some state the compositor has to unwind — a surface,
        // an xdg_surface, a toplevel — and then vanish mid-conversation.
        let mut leaver = harness.connect().await;
        let globals = handshake(&mut leaver).await;
        let compositor = find(&globals, "wl_compositor");
        leaver
            .send(
                REGISTRY,
                BIND,
                &args()
                    .u32(compositor.name)
                    .string("wl_compositor")
                    .u32(compositor.version)
                    .u32(20)
                    .build(),
            )
            .await;
        // wl_compositor.create_surface(new_id)
        leaver.send(20, 0, &args().u32(21).build()).await;
        leaver.drain().await;
    }

    // The survivor must still be answered, which it cannot be if the
    // disconnect took the compositor task down with it.
    first
        .send(DISPLAY, SYNC, &args().u32(CALLBACK + 2).build())
        .await;
    first
        .wait_for("the survivor's sync", |e| e.object_id == CALLBACK + 2)
        .await;
}

#[tokio::test]
async fn requests_split_across_writes_are_still_framed() {
    // A stream splits wherever the kernel likes, and the compositor reassembles
    // by length. Sending one request as a burst of many, byte by byte, is the
    // adversarial version of what happens naturally under load.
    let harness = Harness::start().await;
    let mut client = harness.connect().await;

    // wl_display.get_registry, one byte per write.
    let payload = args().u32(REGISTRY).build();
    let length = u32::try_from(payload.len() + 8).unwrap();
    let mut message = Vec::new();
    message.extend_from_slice(&DISPLAY.to_le_bytes());
    message.extend_from_slice(&((length << 16) | u32::from(GET_REGISTRY)).to_le_bytes());
    message.extend_from_slice(&payload);

    for byte in message {
        client.send_raw(&[byte]).await;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    client
        .wait_for("the registry listing", |e| {
            e.object_id == REGISTRY && e.op_code == GLOBAL
        })
        .await;
}

#[tokio::test]
async fn several_requests_in_one_write_are_all_handled() {
    // The other direction: a client's startup burst arrives as one read, and
    // every message in it has to be found.
    let harness = Harness::start().await;
    let mut client = harness.connect().await;

    let mut burst = Vec::new();
    for (object_id, op_code, payload) in [
        (DISPLAY, GET_REGISTRY, args().u32(REGISTRY).build()),
        (DISPLAY, SYNC, args().u32(CALLBACK).build()),
        (DISPLAY, SYNC, args().u32(CALLBACK + 1).build()),
    ] {
        let length = u32::try_from(payload.len() + 8).unwrap();
        burst.extend_from_slice(&object_id.to_le_bytes());
        burst.extend_from_slice(&((length << 16) | u32::from(op_code)).to_le_bytes());
        burst.extend_from_slice(&payload);
    }
    client.send_raw(&burst).await;

    client
        .wait_for("the first sync", |e| e.object_id == CALLBACK)
        .await;
    client
        .wait_for("the second sync", |e| e.object_id == CALLBACK + 1)
        .await;
}
