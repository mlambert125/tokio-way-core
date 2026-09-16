# tokio_way_core

The mechanism half of a Wayland compositor, as a tokio-based library:
protocol handling, input delivery, and scene building over
[`tokio_way_sock`](https://github.com/mlambert125/tokio_way_sock) and
[`tokio_way_backends`](https://github.com/mlambert125/tokio_way_backends).
Window-management policy stays with the caller, behind the `Shell` trait.

The line it draws: everything that behaves the same whatever the window
management lives here — the protocol handlers and their state, keyboard and
pointer delivery, grabs and drag-and-drop, the scene builder, and the event
loop in `run_compositor`. What a compositor built on it keeps for itself is
policy: which window is where, what has focus, what a keybinding does. The
reference implementation of that half is
[way-small](https://github.com/mlambert125/way-small), whose `WaySmallShell`
(workspaces, placement, click-to-focus, configurable key and mouse bindings)
is the honest example of writing one.

## Usage

Implement `Shell` for your window management, then hand it to the compositor
task alongside the socket and backend channels:

```rust,ignore
let compositor_handle = tokio::spawn(tokio_way_core::run_compositor(
    settings,                        // tokio_way_core::Settings
    Box::new(MyShell::new()),        // your Shell impl
    wayland_message_rx,              // from tokio_way_sock
    backend_message_rx,              // from a tokio_way_backends backend
    backend_request_tx,
    frame_tx,                        // watch::Sender<SceneGraph>
    cancel_token.clone(),
));
```

See way-small's `main.rs` for the full wiring: socket, backend, and
compositor each on their own task or thread, talking over channels and
stopping together on one `CancellationToken`.

The `Shell` seam's one discipline, documented on `tokio_way_core::shell`:
mutating hooks run with the shell detached from `CompositorState`, so a hook
must answer policy questions from its own fields rather than through
`state.shell`.
