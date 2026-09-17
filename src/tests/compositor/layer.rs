//! Tests for where a layer surface lands and what room it leaves behind.
//!
//! The placement rules are the protocol's and they interact: anchoring decides
//! whether a size of zero means "nothing" or "fill the span", margins only
//! push against edges the surface actually touches, and an exclusive zone
//! takes a strip off the edge that surface is anchored to — which then moves
//! everything placed after it. Each of those is easy to get individually right
//! and collectively wrong.

use tokio_way_backends::outputs::OutputId;
use tokio_way_core::layer::{Rect, geometry, place, usable_area};
use tokio_way_core::state::{
    Anchor, CompositorState, LayerKind, LayerSurfaceState, LayerSurfaceStatePending,
};

const OUTPUT: OutputId = OutputId(1);
const SCREEN: Rect = Rect {
    x: 0,
    y: 0,
    width: 1000,
    height: 800,
};

fn wants(anchor: u32, size: (i32, i32)) -> LayerSurfaceStatePending {
    LayerSurfaceStatePending {
        size,
        anchor: Anchor(anchor),
        ..LayerSurfaceStatePending::default()
    }
}

#[test]
fn a_bar_anchored_across_the_top_fills_the_width() {
    // The commonest layer surface there is: anchored left, top and right, with
    // a height and a width of zero meaning "as wide as the anchor makes me".
    let bar = wants(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 30));
    assert_eq!(
        place(SCREEN, &bar),
        Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 30
        }
    );
}

#[test]
fn a_bar_anchored_across_the_bottom_sits_against_it() {
    let bar = wants(Anchor::LEFT | Anchor::BOTTOM | Anchor::RIGHT, (0, 30));
    assert_eq!(
        place(SCREEN, &bar),
        Rect {
            x: 0,
            y: 770,
            width: 1000,
            height: 30
        }
    );
}

#[test]
fn a_surface_anchored_to_nothing_is_centred() {
    // No edge to sit against, so the compositor puts it in the middle — which
    // is what a notification or an on-screen display wants.
    let toast = wants(0, (200, 100));
    assert_eq!(
        place(SCREEN, &toast),
        Rect {
            x: 400,
            y: 350,
            width: 200,
            height: 100
        }
    );
}

#[test]
fn a_size_of_zero_without_a_span_to_fill_gets_nothing() {
    // Zero means "fill the anchored span", and anchored to one edge there is
    // no span. Stretching it across the screen instead would put a wallpaper
    // where a client asked for a sliver.
    let odd = wants(Anchor::TOP, (0, 30));
    assert_eq!(place(SCREEN, &odd).width, 0);
}

#[test]
fn a_wallpaper_anchored_to_every_edge_covers_the_output() {
    let wallpaper = wants(Anchor::ALL, (0, 0));
    assert_eq!(place(SCREEN, &wallpaper), SCREEN);
}

#[test]
fn margins_push_only_against_edges_the_surface_touches() {
    // A margin on an edge the surface is not anchored to has nothing to push
    // against, so it must not shift the surface. Applying all four regardless
    // would drag a top-anchored bar down by its bottom margin.
    let bar = LayerSurfaceStatePending {
        size: (0, 30),
        anchor: Anchor(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT),
        // top, right, bottom, left
        margin: (10, 5, 999, 5),
        ..LayerSurfaceStatePending::default()
    };
    assert_eq!(
        place(SCREEN, &bar),
        Rect {
            x: 5,
            y: 10,
            width: 990,
            height: 30
        },
        "the bottom margin is ignored: the bar is not anchored there",
    );
}

/// An output with one layer surface on it, described by what it asked for.
fn state_with_layers(layers: &[(u32, LayerSurfaceStatePending)]) -> CompositorState {
    let mut state = crate::tests::test_state();
    let mut output = super::test_output(OUTPUT);
    output.geometry.physical_width = SCREEN.width;
    output.geometry.physical_height = SCREEN.height;
    state.outputs.push(output);
    for (index, (id, pending)) in layers.iter().enumerate() {
        state.layer_surfaces.insert(
            (1, *id),
            LayerSurfaceState {
                client_id: 1,
                wl_surface_id: 100 + u32::try_from(index).unwrap(),
                output: Some(OUTPUT),
                pending: pending.clone(),
                current: pending.clone(),
                namespace: String::from("test"),
                configured: true,
                pending_configures: std::collections::VecDeque::new(),
                highest_configure: 0,
                configured_size: None,
            },
        );
    }
    state
}

fn reserving(anchor: u32, size: (i32, i32), zone: i32) -> LayerSurfaceStatePending {
    LayerSurfaceStatePending {
        size,
        anchor: Anchor(anchor),
        exclusive_zone: zone,
        ..LayerSurfaceStatePending::default()
    }
}

#[test]
fn nothing_reserved_leaves_the_whole_output() {
    let state = state_with_layers(&[]);
    assert_eq!(usable_area(&state, OUTPUT), SCREEN);
}

#[test]
fn a_top_bar_takes_a_strip_off_the_top() {
    // The reason a panel is not just a window drawn on top: a maximised window
    // that covered the bar would leave the user unable to reach either.
    let state = state_with_layers(&[(
        10,
        reserving(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 30), 30),
    )]);
    assert_eq!(
        usable_area(&state, OUTPUT),
        Rect {
            x: 0,
            y: 30,
            width: 1000,
            height: 770
        }
    );
}

#[test]
fn a_side_dock_takes_a_strip_off_its_own_edge() {
    let state = state_with_layers(&[(
        10,
        reserving(Anchor::TOP | Anchor::LEFT | Anchor::BOTTOM, (60, 0), 60),
    )]);
    assert_eq!(
        usable_area(&state, OUTPUT),
        Rect {
            x: 60,
            y: 0,
            width: 940,
            height: 800
        }
    );
}

#[test]
fn two_bars_on_the_same_edge_stack_rather_than_overlap() {
    let state = state_with_layers(&[
        (
            10,
            reserving(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 30), 30),
        ),
        (
            11,
            reserving(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 20), 20),
        ),
    ]);
    assert_eq!(
        usable_area(&state, OUTPUT).y,
        50,
        "both reservations come off, one after the other",
    );
}

#[test]
fn a_bar_whose_client_has_gone_stops_reserving_its_strip() {
    // A panel that exits tidily destroys its layer surface and the room comes
    // back with it. One that is killed — which is how a bar usually ends —
    // never sends that request, so the only thing that can forget it is the
    // disconnect path. Until it did, the strip stayed reserved for the life of
    // the compositor: maximised windows short by a bar's height, with no bar
    // there to explain why.
    let mut state = state_with_layers(&[(
        10,
        reserving(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 30), 30),
    )]);
    // The reverse map the protocol handler keeps alongside it, which is what
    // gives a plain `wl_surface` its layer role.
    state.surface_layer.insert((1, 100), 10);
    assert_ne!(usable_area(&state, OUTPUT), SCREEN);

    state.remove_client_resources(1);

    assert!(state.layer_surfaces.is_empty());
    assert!(
        state.surface_layer.is_empty(),
        "the role map goes with the surface it named",
    );
    assert_eq!(usable_area(&state, OUTPUT), SCREEN);
}

#[test]
fn a_surface_reserving_nothing_takes_nothing() {
    // A wallpaper covers the output and reserves none of it — otherwise there
    // would be no room for any window at all.
    let state = state_with_layers(&[(10, reserving(Anchor::ALL, (0, 0), 0))]);
    assert_eq!(usable_area(&state, OUTPUT), SCREEN);
}

#[test]
fn a_negative_exclusive_zone_reserves_nothing() {
    // Negative means "I want no reservation and I ignore everyone else's",
    // which is how a fullscreen overlay says it wants the whole output.
    let state = state_with_layers(&[(10, reserving(Anchor::ALL, (0, 0), -1))]);
    assert_eq!(usable_area(&state, OUTPUT), SCREEN);
}

#[test]
fn a_surface_anchored_to_one_edge_without_spanning_reserves_nothing() {
    // There is no single edge to take a strip from — reserving would carve a
    // bite out of the middle of the screen, which serves nobody.
    let state = state_with_layers(&[(10, reserving(Anchor::TOP, (100, 30), 30))]);
    assert_eq!(usable_area(&state, OUTPUT), SCREEN);
}

#[test]
fn layers_stack_background_bottom_top_overlay() {
    // The ordering is the drawing order, and it is the whole point of the
    // protocol: a wallpaper can never be over a window, a lock screen never
    // under one.
    assert!(LayerKind::Background < LayerKind::Bottom);
    assert!(LayerKind::Bottom < LayerKind::Top);
    assert!(LayerKind::Top < LayerKind::Overlay);
    assert!(!LayerKind::Background.is_above_windows());
    assert!(!LayerKind::Bottom.is_above_windows());
    assert!(LayerKind::Top.is_above_windows());
    assert!(LayerKind::Overlay.is_above_windows());
}

#[test]
fn a_bar_is_not_pushed_down_by_its_own_exclusive_zone() {
    // The bug this file did not catch until a real bar showed it. Placement and
    // reservation were computed separately: `geometry` placed a surface inside
    // the area that had *already* been shrunk by every exclusive zone —
    // including its own — so a bar reserving 34 pixels at the top was drawn 34
    // pixels down, holding a gap open for itself. Both halves were tested here
    // and both were right; only their composition was wrong.
    let mut state = state_with_layers(&[(
        10,
        reserving(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 48), 34),
    )]);
    // The measurements noctalia's bar actually reported.
    state
        .layer_surfaces
        .get_mut(&(1, 10))
        .unwrap()
        .wl_surface_id = 100;

    let placed = geometry(&state, (1, 10)).expect("the bar should be placed");
    assert_eq!(
        placed.y, 0,
        "a bar anchored to the top with no margin belongs at the top",
    );
    assert_eq!(placed.height, 48);

    // And the room it reserved still comes off, for everything else.
    assert_eq!(usable_area(&state, OUTPUT).y, 34);
}

#[test]
fn a_second_bar_is_placed_below_the_first_ones_reservation() {
    // A reservation applies to everything placed after it, which is what makes
    // two bars on the same edge stack outward instead of overlapping.
    let state = state_with_layers(&[
        (
            10,
            reserving(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 30), 30),
        ),
        (
            11,
            reserving(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 20), 20),
        ),
    ]);

    assert_eq!(
        geometry(&state, (1, 10)).unwrap().y,
        0,
        "the first bar is at the top",
    );
    assert_eq!(
        geometry(&state, (1, 11)).unwrap().y,
        30,
        "and the second sits below what the first reserved",
    );
    assert_eq!(
        usable_area(&state, OUTPUT).y,
        50,
        "both come off in the end"
    );
}

#[test]
fn a_wallpaper_ignores_every_reservation() {
    // A negative exclusive zone means "I want none and I ignore everyone
    // else's", so a wallpaper still covers the whole output however many bars
    // are reserving space above it.
    let state = state_with_layers(&[
        (
            10,
            reserving(Anchor::LEFT | Anchor::TOP | Anchor::RIGHT, (0, 34), 34),
        ),
        (11, reserving(Anchor::ALL, (0, 0), -1)),
    ]);

    let wallpaper = geometry(&state, (1, 11)).unwrap();
    assert_eq!((wallpaper.x, wallpaper.y), (0, 0));
    assert_eq!(
        (wallpaper.width, wallpaper.height),
        (SCREEN.width, SCREEN.height)
    );
}

#[test]
fn a_layer_surface_is_never_put_in_the_window_stack() {
    // Where a layer surface draws is decided by its layer and nothing else.
    // Raising one into a workspace would draw it twice — once as a layer and
    // once as a window — and let Alt+Tab cycle onto the wallpaper.
    use tokio_way_core::state::KeyboardInteractivity;
    let mut state = state_with_layers(&[(10, reserving(Anchor::ALL, (0, 0), 0))]);
    state.create_surface(1, 100);
    state.surface_layer.insert((1, 100), 10);
    crate::tests::with_ws(&mut state, |w, s| w.workspaces.sync_outputs(&s.outputs));

    crate::tests::with_ws(&mut state, |w, s| w.raise_window(s, (1, 100)));

    assert!(
        state.shell.visible_stack(OUTPUT).is_empty(),
        "the wallpaper must not have joined the windows",
    );

    // And it takes the keyboard only if it asked to. A wallpaper that stole
    // focus on click would take input from whatever the user was typing into.
    assert!(!tokio_way_core::input::accepts_click_focus(
        &state,
        (1, 100)
    ));
    state
        .layer_surfaces
        .get_mut(&(1, 10))
        .unwrap()
        .current
        .keyboard_interactivity = KeyboardInteractivity::OnDemand;
    assert!(tokio_way_core::input::accepts_click_focus(&state, (1, 100)));

    // An ordinary window is unaffected either way.
    state.create_surface(1, 200);
    assert!(tokio_way_core::input::accepts_click_focus(&state, (1, 200)));
}
