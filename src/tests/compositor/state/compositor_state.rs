//! Tests for compositor state: region arithmetic, and the defences that
//! keep a client from taking the compositor down with its shm pool.

use std::os::unix::io::RawFd;
use tokio_way_backends::shm::PoolMapping;
use tokio_way_backends::shm::patched_pages;

/// A memfd of `size` bytes filled with `fill`. `sealable` decides whether
/// the compositor will be able to seal it against shrinking.
fn pool_file(size: u32, fill: u8, sealable: bool) -> RawFd {
    let flags = if sealable {
        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING
    } else {
        libc::MFD_CLOEXEC
    };
    let fd = unsafe { libc::memfd_create(c"pool-test".as_ptr().cast(), flags) };
    assert!(fd >= 0, "memfd_create failed");
    assert_eq!(unsafe { libc::ftruncate(fd, i64::from(size)) }, 0);
    let bytes = vec![fill; size as usize];
    let written = unsafe { libc::pwrite(fd, bytes.as_ptr().cast(), bytes.len(), 0) };
    assert_eq!(written, isize::try_from(size).unwrap());
    fd
}

#[test]
fn a_pool_larger_than_its_file_is_refused() {
    let fd = pool_file(4096, 0xab, false);
    // Mapping it would create pages with nothing behind them, which fault
    // on the first read. Better to refuse and tell the client.
    assert!(PoolMapping::new(fd, 8192).is_none());
    unsafe { libc::close(fd) };
}

#[test]
fn a_sealable_pool_is_sealed_against_shrinking() {
    let size = 8192;
    let fd = pool_file(size, 0xab, true);
    let mapping = PoolMapping::new(fd, size).expect("should map");

    // The seal is the real defence: the client now cannot shrink the file
    // at all, so the fault can never happen.
    assert_eq!(
        unsafe { libc::ftruncate(fd, 0) },
        -1,
        "a sealed pool must refuse to shrink"
    );

    drop(mapping);
    unsafe { libc::close(fd) };
}

#[test]
fn truncating_an_unsealable_pool_blanks_it_rather_than_killing_us() {
    let size = 16384;
    // Not sealable, so the client can still pull the file out from under
    // the mapping — the case the SIGBUS net exists for.
    let fd = pool_file(size, 0xab, false);
    let mapping = PoolMapping::new(fd, size).expect("should map");

    let tail = usize::try_from(size).unwrap() - 4;
    let before = unsafe { mapping.slice(tail, 4) }.expect("in bounds");
    assert_eq!(before, &[0xab; 4], "file contents should be visible");

    let patched_before = patched_pages();
    assert_eq!(unsafe { libc::ftruncate(fd, 0) }, 0, "should shrink");

    // Without the handler this read takes the whole process down with
    // SIGBUS. With it, the page is replaced by zeroes and the read retries.
    let after = unsafe { mapping.slice(tail, 4) }.expect("in bounds");
    assert_eq!(after, &[0x00; 4], "truncated pages should read as black");
    assert!(
        patched_pages() > patched_before,
        "the fault should have been counted"
    );

    drop(mapping);
    unsafe { libc::close(fd) };
}
use tokio_way_core::state::{RegionOp, RegionRect, region_contains};

fn r(op: RegionOp, x: i32, y: i32, width: i32, height: i32) -> RegionRect {
    RegionRect {
        op,
        x,
        y,
        width,
        height,
    }
}

#[test]
fn empty_region_accepts_nothing() {
    assert!(!region_contains(&[], 0, 0));
    assert!(!region_contains(&[], 50, 50));
}

#[test]
fn add_bounds_are_half_open() {
    let rects = [r(RegionOp::Add, 10, 10, 20, 20)];
    assert!(region_contains(&rects, 10, 10));
    assert!(region_contains(&rects, 29, 29));
    assert!(!region_contains(&rects, 30, 30));
    assert!(!region_contains(&rects, 9, 10));
}

#[test]
fn subtract_punches_a_hole() {
    let rects = [
        r(RegionOp::Add, 0, 0, 100, 100),
        r(RegionOp::Subtract, 40, 40, 20, 20),
    ];
    assert!(region_contains(&rects, 10, 10));
    assert!(!region_contains(&rects, 45, 45));
}

#[test]
fn later_add_reinstates_subtracted_area() {
    // The case two unordered lists would get wrong.
    let rects = [
        r(RegionOp::Add, 0, 0, 100, 100),
        r(RegionOp::Subtract, 40, 40, 20, 20),
        r(RegionOp::Add, 45, 45, 5, 5),
    ];
    assert!(!region_contains(&rects, 41, 41));
    assert!(region_contains(&rects, 46, 46));
}

#[test]
fn zero_sized_rect_contains_nothing() {
    assert!(!region_contains(&[r(RegionOp::Add, 5, 5, 0, 0)], 5, 5));
}

mod cursor {
    // Cursor positions here are exact: every value under test is either an
    // integer the constraint clamped to, or a sum of integers and halves that
    // `f64` represents without loss.
    #![allow(clippy::float_cmp)]

    use tokio_way_backends::outputs::{
        OUTPUT_MODE_CURRENT, Output, OutputGeometry, OutputId, OutputMode, OutputSubpixel,
        OutputTransform, Scale,
    };
    use tokio_way_core::protocol::wire_utils::f64_to_i32;
    use tokio_way_core::state::CompositorState;

    fn output(id: u32, x: i32, y: i32, width: i32, height: i32) -> Output {
        Output {
            id: OutputId(id),
            geometry: OutputGeometry {
                x,
                y,
                physical_width: width,
                physical_height: height,
                subpixel: OutputSubpixel::None,
                make: String::new(),
                model: String::new(),
                transform: OutputTransform::Normal,
            },
            modes: vec![OutputMode {
                flags: OUTPUT_MODE_CURRENT,
                width,
                height,
                refresh_mhz: 60000,
            }],
            scale: Scale::ONE,
            name: String::new(),
            description: String::new(),
        }
    }

    fn at(state: &CompositorState) -> (f64, f64) {
        (state.cursor_x, state.cursor_y)
    }

    fn state_with(outputs: Vec<Output>) -> CompositorState {
        let mut state = crate::tests::test_state();
        state.outputs = outputs;
        state
    }

    #[test]
    fn a_position_on_an_output_is_left_alone() {
        let mut state = state_with(vec![output(1, 0, 0, 100, 100)]);
        state.move_cursor_to(40.0, 60.0);
        assert_eq!(at(&state), (40.0, 60.0));
    }

    #[test]
    fn a_position_off_the_edge_is_pulled_back_on() {
        let mut state = state_with(vec![output(1, 0, 0, 100, 100)]);
        state.move_cursor_to(500.0, -20.0);
        // Still on a real pixel of the output, not one past its far edge.
        assert_eq!(at(&state), (99.0, 0.0));
    }

    #[test]
    fn relative_motion_accumulates_and_stops_at_the_edge() {
        let mut state = state_with(vec![output(1, 0, 0, 100, 100)]);
        state.move_cursor_to(50.0, 50.0);
        state.move_cursor_by(10.5, -5.0);
        assert_eq!(at(&state), (60.5, 45.0));

        // A mouse reports movement forever; without a constraint the pointer
        // would keep going and never come back.
        for _ in 0..100 {
            state.move_cursor_by(50.0, 0.0);
        }
        assert_eq!(at(&state).0, 99.0);
    }

    #[test]
    fn the_pointer_crosses_between_adjacent_outputs() {
        let mut state = state_with(vec![output(1, 0, 0, 100, 100), output(2, 100, 0, 100, 100)]);
        state.move_cursor_to(90.0, 50.0);
        state.move_cursor_by(30.0, 0.0);
        // The gap between them is not a gap: both are inside the union.
        assert_eq!(at(&state), (120.0, 50.0));
    }

    #[test]
    fn the_pointer_cannot_sit_in_the_notch_between_mismatched_outputs() {
        // A tall output beside a short one leaves a corner that belongs to no
        // display. A bounding box would happily park the pointer there, where
        // it would be invisible and unclickable.
        let mut state = state_with(vec![output(1, 0, 0, 100, 200), output(2, 100, 0, 100, 100)]);
        state.move_cursor_to(150.0, 180.0);

        let (x, y) = at(&state);
        let landed_on = state.outputs.iter().find(|o| {
            tokio_way_backends::outputs::output_contains(o, f64_to_i32(x), f64_to_i32(y))
        });
        assert!(
            landed_on.is_some(),
            "cursor at {:?} is on no output",
            (x, y)
        );
        // Nearest, not directly above: the tall output's right edge is 51
        // across, while the short output's bottom edge is 81 up.
        assert_eq!(at(&state), (99.0, 180.0));
    }

    #[test]
    fn a_non_finite_position_is_refused() {
        // Every reader converts the position to `i32` unchecked, so NaN must
        // never be stored.
        let mut state = state_with(vec![output(1, 0, 0, 100, 100)]);
        state.move_cursor_to(40.0, 40.0);
        state.move_cursor_by(f64::NAN, 0.0);
        assert_eq!(at(&state), (40.0, 40.0));
    }
}

/// A disconnecting client must take its pools with it.
mod disconnect {
    use super::pool_file;

    const CLIENT: u32 = 1;
    const POOL: u32 = 100;
    const BUFFER: u32 = 101;

    #[test]
    fn a_disconnecting_clients_pools_are_freed() {
        let mut state = crate::tests::test_state();
        let size = 4096;
        assert!(state.register_shm_pool(CLIENT, POOL, pool_file(size, 0xcd, true), size));
        state.register_buffer(CLIENT, BUFFER, POOL, 0, 8, 8, 32, 0);
        assert!(state.shm_pools.contains_key(&(CLIENT, POOL)));

        state.remove_client_resources(CLIENT);

        // A pool is freed only once nothing references it, so its buffers have
        // to go first. Tearing down in the other order leaves the pool marked
        // dead and never collected — its mapping alive and its descriptor open
        // for the rest of the compositor's life, once per client that ever
        // connected.
        assert!(
            !state.shm_pools.contains_key(&(CLIENT, POOL)),
            "the pool outlived the client that owned it"
        );
        assert!(state.buffers.is_empty());
    }
}

// -- The data protocols ------------------------------------------------------

use tokio::sync::mpsc::channel;
use tokio_util::sync::CancellationToken;
use tokio_way_core::state::{
    CompositorState, DataDeviceBinding, DataInterface, DataOffer, DataSource, DataSourceRole,
    OfferKind,
};

/// A client holding a data device and a source offering one mime type.
fn client_with_a_source(state: &mut CompositorState, client_id: u32, source_id: u32) {
    let (tx, _rx) = channel(64);
    state
        .clients
        .create(client_id, tx, CancellationToken::new());
    state.data_devices.push(DataDeviceBinding {
        client_id,
        object_id: source_id + 100,
    });
    state.data_sources.insert(
        (client_id, source_id),
        DataSource {
            interface: DataInterface::WlData,
            mime_types: vec!["text/plain".to_string()],
            actions: 0,
            role: DataSourceRole::Selection,
            cancelled: false,
        },
    );
}

#[test]
fn a_disconnecting_client_takes_the_clipboard_it_owned_with_it() {
    let mut state = crate::tests::test_state();
    client_with_a_source(&mut state, 1, 40);
    state.selection = Some((1, 40));

    state.remove_client_resources(1);

    assert!(
        state.selection.is_none(),
        "a selection naming a source that has gone can produce nothing"
    );
    assert!(state.data_sources.is_empty());
    assert!(state.data_devices.is_empty());
}

#[test]
fn a_disconnecting_client_leaves_someone_elses_clipboard_alone() {
    let mut state = crate::tests::test_state();
    client_with_a_source(&mut state, 1, 40);
    client_with_a_source(&mut state, 2, 50);
    state.selection = Some((1, 40));

    state.remove_client_resources(2);

    assert_eq!(state.selection, Some((1, 40)));
}

#[test]
fn an_offer_outlives_the_source_that_went_away() {
    // The client that holds it still owns the id and will still send requests
    // on it, so the object stays and only its contents are forgotten.
    let mut state = crate::tests::test_state();
    client_with_a_source(&mut state, 1, 40);
    client_with_a_source(&mut state, 2, 50);
    state.selection = Some((1, 40));
    state.data_offers.insert(
        (2, 0xff00_0000),
        DataOffer {
            client_id: 2,
            source: Some((1, 40)),
            kind: OfferKind::Selection,
            accepted: None,
            actions: 0,
            preferred_action: 0,
            resolved_action: 0,
        },
    );

    state.remove_client_resources(1);

    let offer = state
        .data_offers
        .get(&(2, 0xff00_0000))
        .expect("the offer object survives its source");
    assert!(offer.source.is_none(), "but it has nothing left behind it");
}

#[test]
fn a_destroyed_surface_gives_its_role_back() {
    // Roles are permanent for the life of a surface, not of an id: the client
    // is told the id is free, and what it allocates next has no role.
    let mut state = crate::tests::test_state();
    state.create_surface(1, 10);
    state.dnd_icon_surfaces.insert((1, 10));
    state.cursor_role_surfaces.insert((1, 10));

    state.destroy_surface(1, 10);

    assert!(state.dnd_icon_surfaces.is_empty());
    assert!(state.cursor_role_surfaces.is_empty());
}

// -- The keyboard and its xkb options ---------------------------------------

/// Evdev's `KEY_CAPSLOCK`, plus the eight xkb adds to every evdev code.
const XKB_CAPSLOCK: u32 = 58 + 8;

#[test]
fn on_the_default_keymap_caps_lock_locks_a_modifier() {
    // The counterpart to the remap test below: if this ever stops holding,
    // that test is passing for the wrong reason.
    let mut keyboard = tokio_way_core::state::Keyboard::new(None);
    assert!(
        keyboard.keymap_string().is_some(),
        "the default keymap should compile"
    );
    let mods = keyboard.update_key(XKB_CAPSLOCK, true);
    assert_ne!(mods.locked, 0, "Caps Lock locks on the way down");
}

#[test]
fn xkb_options_reach_the_keymap() {
    // caps:escape makes the key Escape, and Escape locks nothing — observable
    // straight from the modifier masks, no client needed.
    let mut keyboard = tokio_way_core::state::Keyboard::new(Some("caps:escape"));
    assert!(
        keyboard.keymap_string().is_some(),
        "caps:escape should compile"
    );
    let mods = keyboard.update_key(XKB_CAPSLOCK, true);
    assert_eq!(mods.locked, 0, "the remapped key is Escape, not a lock");
}

#[test]
fn an_unknown_xkb_option_still_leaves_a_working_keyboard() {
    // xkb quietly ignores option names it does not know, and if a string ever
    // breaks compilation outright, `Keyboard::new` falls back to the default
    // keymap. Either way a typo must not cost the whole keyboard.
    let keyboard = tokio_way_core::state::Keyboard::new(Some("caps:no_such_option"));
    assert!(keyboard.keymap_string().is_some());
}
