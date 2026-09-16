//! Scene building.
//!
//! Turns compositor state into a `Scene`: a flat, back-to-front list of
//! textured quads in output pixel coordinates. Surface position, subsurface
//! offsets, `wp_viewport` cropping and scaling, and buffer scale all resolve
//! here into a source rectangle paired with a destination rectangle. Runs in
//! the compositor task, which owns the state; the backend turns the result
//! into GPU work in its own thread and GL context.
//!
//! No client pixels are copied. A texture points straight into the client's
//! shm mapping and holds that buffer's guard, which is what stops
//! `wl_buffer.release` going out while the backend is still reading. The
//! `SceneCache` therefore holds no pixels either — only enough about the last
//! frame to say what the backend already has, so damage can be expressed
//! against it.

use super::protocol::wire_utils::f64_to_i32;
use super::protocol::wl_shm::FORMAT_XRGB8888;
use super::state::{Buffer, BufferKind, ClientObjectId, CompositorState, DefaultCursor};
use crate::shell::SurfacePresentation;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_way_backends::dma::pixel_format;
use tokio_way_backends::outputs::{OUTPUT_MODE_CURRENT, Output, Scale, output_contains};
use tokio_way_backends::scene_graph::{
    ElementTransform, PixelFormat, Scene, SceneContent, SceneElement, SceneGroup, TextureId,
    TextureImage, TextureRect, TextureSource,
};
use tokio_way_backends::shm::{PoolMapping, UploadPixels};
use tracing::{debug, info};

/// Number of bytes for representing one pixel
const BYTES_PER_PIXEL: usize = 4;

/// Fallback mouse cursor image encoded as an ASCII string
const FALLBACK_CURSOR_BITMAP: [&[u8; 12]; 19] = [
    b"B...........",
    b"BB..........",
    b"BWB.........",
    b"BWWB........",
    b"BWWWB.......",
    b"BWWWWB......",
    b"BWWWWWB.....",
    b"BWWWWWWB....",
    b"BWWWWWWWB...",
    b"BWWWWWWWWB..",
    b"BWWWWWWWWWB.",
    b"BWWWWWWWWWWB",
    b"BWWWWWWBBBBB",
    b"BWWBWWWB....",
    b"BWBB.BWWB...",
    b"BB...BWWB...",
    b"B.....BWWB..",
    b"......BWWB..",
    b".......BB...",
];

/// The bell flash: translucent white over the whole output. The alpha is what
/// makes it a flash rather than a blank screen — the windows stay readable
/// underneath it.
const BELL_FLASH_COLOR: u32 = 0x60ff_ffff;

// The background an output clears to comes from `Settings::background_color`.

/// Black color for coloring the fallback cursor
const CURSOR_BLACK: u32 = 0xff00_0000;
/// White color for coloring the fallback cursor
const CURSOR_WHITE: u32 = 0xffff_ffff;
/// The width of the fallback cursor
const FALLBACK_CURSOR_WIDTH: i32 = 12;

/// A choice of how to render the cursor (hidden, client-provided surface, or compositor
/// default/theme.)
enum CursorChoice {
    /// The focused client asked for no cursor.
    Hidden,
    /// Draw the client's own cursor surface at the given hotspot.
    Surface(ClientObjectId, i32, i32),
    /// Draw the theme's cursor for a shape the client named through
    /// `wp_cursor_shape_v1`, looked up by these candidate names.
    Shape(&'static [&'static str]),
    /// The compositor picks: the theme's ordinary pointer, or the built-in one.
    Compositor,
}

/// Pixel copies of client buffers and cursors, kept across frames.
///
/// Lives with the compositor loop rather than in `CompositorState`: it is
/// derived from protocol state, never part of it, and nothing in the protocol
/// layer needs to know it exists. Protocol handlers signal a content change by
/// bumping `ShmBuffer::content_serial`; this compares serials and re-reads only
/// what actually moved.
/// What the cache remembers about a buffer between frames.
///
/// Metadata only. There are no pixels to keep now that images borrow the
/// client's mapping, and holding a `TextureImage` here would pin the client's
/// buffer forever and stall its release.
#[derive(Clone, Copy)]
struct CachedImage {
    /// A unique serial number for this cached image
    serial: u64,
    /// Width of the cached image
    width: i32,
    /// Height of the cached image
    height: i32,
}

/// A surface's last successfully composed content, kept so the surface can
/// go on being drawn when its committed buffer stops existing.
///
/// Toolkits destroy a buffer the moment it is obsolete — during an
/// interactive resize, often while it is still the surface's current one and
/// before the replacement commits. The protocol calls the contents
/// "undefined" from then until the next commit; blanking the window for that
/// gap flickers on every resize step, so the answer every compositor gives
/// is to keep showing what was there. The `TextureImage` is what makes that
/// sound: it holds the shm mapping's guard (or the dma-buf image) itself, so
/// the pixels it points at stay valid however thoroughly the client tears
/// its objects down.
struct RetainedSurface {
    image: Arc<TextureImage>,
    mapping: crate::state::BufferMapping,
    transform: tokio_way_backends::scene_graph::BufferTransform,
}

/// Cached textures
#[derive(Default)]
pub struct SceneCache {
    /// Buffers from the client
    buffers: HashMap<ClientObjectId, CachedImage>,
    /// Each surface's last composed content, by surface key — the fallback
    /// for a committed buffer destroyed before its replacement arrives.
    retained: HashMap<ClientObjectId, RetainedSurface>,
    /// Cursor textures
    cursors: HashMap<TextureId, Arc<TextureImage>>,
    /// Serials for cursor images, which have no protocol-side content serial.
    next_cursor_serial: u64,
    /// Which cursor the themed slot currently holds: a shape's names, or `None`
    /// for the ordinary pointer. See [`ensure_cursor_image`].
    cursor_shape: Option<&'static [&'static str]>,
}

impl SceneCache {
    /// Create a new instance of the scene cache with defaults
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the cache is holding no buffer copies. Used by tests.
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    /// Drop copies of buffers that no longer exist.
    ///
    /// Buffers are destroyed without the cache hearing about it, so entries are
    /// reaped against live state rather than by explicit removal.
    pub fn gc(&mut self, state: &CompositorState) {
        self.buffers
            .retain(|key, _| state.buffers.contains_key(key));
        // A retained entry lives as long as its surface is mapped, and is
        // replaced only by the next successful compose. Nothing shorter is
        // safe: a client can attach a buffer and destroy it before a single
        // compose sees it, and the only contents left to show for that gap
        // are whatever composed last — evicting on any earlier signal turns
        // some resize step somewhere into a blank frame. The cost is that
        // the entry holds its buffer's read guard, so a replaced buffer's
        // `wl_buffer.release` waits for the compose that overwrites the
        // entry — prompt for anything visible, and a surface not being
        // composed is not redrawing either.
        self.retained.retain(|key, _| {
            state
                .surfaces
                .get(key)
                .is_some_and(|surface| surface.buffer_id.is_some())
        });
    }
}

/// Build the scene for one output.
pub fn build(
    output_id: tokio_way_backends::outputs::OutputId,
    serial: u64,
    state: &CompositorState,
    cache: &mut SceneCache,
) -> Scene {
    let mut elements = Vec::new();
    let mut scale = Scale::ONE;

    // An output with no current mode has nothing to compose onto.
    if let Some(output) = state.outputs.iter().find(|o| o.id == output_id)
        && current_mode_size(output).is_some()
    {
        scale = output.effective_scale();
        // Windows are positioned globally but drawn in the coordinates of the
        // output they are on, so everything shifts by that output's origin.
        let (origin_x, origin_y) = (output.geometry.x, output.geometry.y);

        // Wallpapers and anything else that belongs under the windows.
        push_layers(
            state,
            cache,
            &mut elements,
            output_id,
            false,
            origin_x,
            origin_y,
        );

        // Only what the shell says is showing on this output is drawn, in
        // the order it says. Popups and subsurfaces come along with their
        // toplevel.
        for key in state.shell.visible_stack(output_id) {
            let Some(surface) = state.surfaces.get(&key) else {
                continue;
            };
            let (x, y) = surface.position;
            let presentation = state.shell.present(key);
            if presentation.is_identity() {
                push_surface_tree(state, cache, &mut elements, key, x - origin_x, y - origin_y);
            } else {
                // A fade or transform applies to the window as one thing, so
                // its whole tree composes into a group and the presentation
                // rides on the group's single quad — dimming element by
                // element would seam wherever they overlap.
                let mut tree = Vec::new();
                push_surface_tree(state, cache, &mut tree, key, x - origin_x, y - origin_y);
                if let Some(element) = presented_group(tree, &presentation) {
                    elements.push(element);
                }
            }
        }

        // Panels, bars and lock screens, over every window.
        push_layers(
            state,
            cache,
            &mut elements,
            output_id,
            true,
            origin_x,
            origin_y,
        );

        // Above every window and below the cursor: a drag icon is meant to be
        // the thing being carried, and the pointer stays on top of it. The
        // cursor itself is not here — it rides in the frame beside the scenes,
        // built by `build_cursor`, so pointer motion does not recompose this.
        push_drag_icon(state, cache, &mut elements, output, origin_x, origin_y);
        push_bell(state, &mut elements, output);
    }

    Scene {
        output_id,
        background: state.settings.background_color,
        serial,
        elements,
        scale,
        // Whole output until the pacer, which holds the previous scene, fills
        // these in with what actually changed. A scene built with nothing to
        // diff against keeps `damage_from` at `None`, which reads as "all of it".
        damage_from: None,
        damage: Vec::new(),
    }
}

/// The regions that differ between two scenes for one output, in logical
/// pixels, for a backend that can repaint less than the whole output.
///
/// Conservative by construction: it never reports less than changed, though it
/// may report more. Elements are matched by position in the back-to-front list,
/// which has no stable identity across frames — so a reorder or an insert
/// damages everything from that point back, which is safe (a wasted repaint),
/// never wrong (a stale pixel). The common cases stay tight: a window redrawing
/// its buffer changes one element in place, and the cursor moving changes only
/// the last one, so only those rectangles come back.
pub fn output_damage(previous: &Scene, current: &Scene) -> Vec<TextureRect> {
    let mut rects = Vec::new();
    let count = previous.elements.len().max(current.elements.len());
    for i in 0..count {
        let before = previous.elements.get(i);
        let after = current.elements.get(i);
        if let (Some(before), Some(after)) = (before, after)
            && same_element(before, after)
        {
            continue;
        }
        // Whatever this slot held and whatever it holds now both have to be
        // repainted: the old to erase it, the new to draw it. An element that
        // only changed its contents in place covers one rectangle, not two.
        let before_rect = before.map(|e| bounding_rect(e.dst));
        let after_rect = after.map(|e| bounding_rect(e.dst));
        if let Some(rect) = before_rect {
            rects.push(rect);
        }
        if let Some(rect) = after_rect
            && before_rect != Some(rect)
        {
            rects.push(rect);
        }
    }
    rects
}

/// Whether two elements would draw identically. `dst` and `alpha` are compared
/// by bit pattern rather than value: exact equality is what "unchanged" means,
/// and it sidesteps the float-comparison lint without changing the answer for
/// the finite values a scene ever holds.
fn same_element(a: &SceneElement, b: &SceneElement) -> bool {
    a.opaque == b.opaque
        && a.alpha.to_bits() == b.alpha.to_bits()
        && dst_bits(a.dst) == dst_bits(b.dst)
        && a.transform == b.transform
        && same_effect(a, b)
        && same_content(&a.content, &b.content)
}

/// Whether two elements carry the same shader effect: both none, or the same
/// shared effect. Distinct effect values are never "the same" — a uniform that
/// moved draws differently even under one source snippet.
fn same_effect(a: &SceneElement, b: &SceneElement) -> bool {
    match (&a.effect, &b.effect) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

fn same_content(a: &SceneContent, b: &SceneContent) -> bool {
    match (a, b) {
        (
            SceneContent::Texture {
                image: ia,
                src: sa,
                transform: ta,
            },
            SceneContent::Texture {
                image: ib,
                src: sb,
                transform: tb,
            },
        ) => {
            // Same buffer, same contents (serial), sampled the same way. The
            // serial is what moves when a client draws, so it is what tells an
            // otherwise-identical element apart from its predecessor.
            ia.id == ib.id && ia.serial == ib.serial && ta == tb && dst_bits(*sa) == dst_bits(*sb)
        }
        (SceneContent::Color(a), SceneContent::Color(b)) => a == b,
        _ => false,
    }
}

/// The four components of a destination or source rectangle as raw bits, for
/// exact equality.
fn dst_bits(r: (f64, f64, f64, f64)) -> [u64; 4] {
    [r.0.to_bits(), r.1.to_bits(), r.2.to_bits(), r.3.to_bits()]
}

/// The smallest integer rectangle covering a fractional one, rounded outward so
/// a partially-covered pixel is repainted rather than left stale.
fn bounding_rect(dst: (f64, f64, f64, f64)) -> TextureRect {
    let (x, y, w, h) = dst;
    let left = x.floor();
    let top = y.floor();
    let right = (x + w).ceil();
    let bottom = (y + h).ceil();
    TextureRect {
        x: f64_to_i32(left),
        y: f64_to_i32(top),
        width: f64_to_i32(right - left),
        height: f64_to_i32(bottom - top),
    }
}

/// Get the size (resolution) of the current mode of the provided output
fn current_mode_size(output: &Output) -> Option<(i32, i32)> {
    output
        .modes
        .iter()
        .find(|m| m.flags & OUTPUT_MODE_CURRENT != 0)
        .map(|m| (m.width, m.height))
}

/// Push the entire surface tree into `elements` to build
/// the scene graph to be passed to the backend
fn push_surface_tree(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_key) else {
        return;
    };
    let client_id = surface.client_id;

    push_surface(state, cache, elements, surface_key, offset_x, offset_y);

    for &child_id in &surface.children {
        let child_key = (client_id, child_id);
        let Some(child) = state.surfaces.get(&child_key) else {
            continue;
        };
        let (cx, cy) = child.subsurface_position;
        push_surface_tree(
            state,
            cache,
            elements,
            child_key,
            offset_x.saturating_add(cx),
            offset_y.saturating_add(cy),
        );
    }
}

/// Add one surface's buffer to the scene, if it has one.
///
/// The source crop and destination size come from the same mapping the damage
/// path runs backwards, expressed here as a single quad instead of a per-pixel
/// sampling loop. Clipping to the output is left to the GPU.
fn push_surface(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    surface_key: ClientObjectId,
    offset_x: i32,
    offset_y: i32,
) {
    let Some(surface) = state.surfaces.get(&surface_key) else {
        return;
    };
    // No buffer attached means the client chose to show nothing; that is not
    // the destroyed-buffer gap below, and nothing is drawn.
    let Some(buffer_id) = surface.buffer_id else {
        return;
    };

    let current = state
        .surface_buffer_mapping(surface_key)
        .and_then(|mapping| {
            let image = ensure_image(state, cache, (surface.client_id, buffer_id))?;
            Some((image, mapping, surface.buffer_transform))
        });
    let (image, mapping, transform) = if let Some((image, mapping, transform)) = current {
        cache.retained.insert(
            surface_key,
            RetainedSurface {
                image: Arc::clone(&image),
                mapping,
                transform,
            },
        );
        (image, mapping, transform)
    } else {
        // The committed buffer no longer exists — destroyed while attached,
        // which toolkits do mid-resize before the replacement commits. The
        // contents are "undefined" until then, and the useful reading of
        // that is: what was there last, not a blank frame.
        let Some(retained) = cache.retained.get(&surface_key) else {
            debug!("push_surface {surface_key:?}: buffer {buffer_id} gone, nothing retained");
            return;
        };
        (
            Arc::clone(&retained.image),
            retained.mapping,
            retained.transform,
        )
    };

    elements.push(SceneElement {
        content: SceneContent::Texture {
            image,
            src: mapping.src,
            transform,
        },
        dst: dst_rect(offset_x, offset_y, mapping.dest_width, mapping.dest_height),
        transform: ElementTransform::IDENTITY,
        effect: None,
        alpha: 1.0,
        opaque: surface_is_opaque(surface, mapping.dest_width, mapping.dest_height),
    });
}

/// Widen an integer rectangle into the fractional one a scene carries.
///
/// Everything the protocol lays out is on the integer grid; the fractional
/// space exists for what the compositor itself moves — an animation.
fn dst_rect(x: i32, y: i32, width: i32, height: i32) -> (f64, f64, f64, f64) {
    (
        f64::from(x),
        f64::from(y),
        f64::from(width),
        f64::from(height),
    )
}

/// Build the texture for a client buffer, however its memory is held.
///
/// The one place the two kinds of buffer part company: an upload has to be
/// read, bounds-checked and diffed against what the backend already has, while
/// an imported one is handed over as a description and sampled where it lies.
fn ensure_image(
    state: &CompositorState,
    cache: &mut SceneCache,
    key: ClientObjectId,
) -> Option<Arc<TextureImage>> {
    match &state.buffers.get(&key)?.kind {
        BufferKind::Shm(_) => ensure_buffer_image(state, cache, key),
        BufferKind::Dmabuf(image) => Some(imported_image(key, state.buffers.get(&key)?, image)),
        // Described, accepted, and then refused by the driver. It draws
        // nothing rather than taking the client down for its driver's answer.
        BufferKind::Failed => None,
    }
}

/// Describe an already-imported buffer to the backend.
///
/// Nothing is read and nothing is cached: the texture the backend builds from
/// this samples the client's own memory, so a client drawing into it changes
/// what is on screen without anything passing through here. That is also why
/// the serial never moves — see [`crate::state::Buffer`].
fn imported_image(
    key: ClientObjectId,
    buffer: &Buffer,
    image: &Arc<tokio_way_backends::dma::DmabufImage>,
) -> Arc<TextureImage> {
    Arc::new(TextureImage {
        id: TextureId::Buffer(key.0, key.1),
        serial: buffer.content_serial,
        width: buffer.width,
        height: buffer.height,
        format: pixel_format(image.fourcc),
        source: TextureSource::Dmabuf {
            image: image.clone(),
            // Plain `wl_buffer` commits get implicit sync: the kernel's
            // ordering is trusted, which is what these fields' absence means.
            acquire: None,
            release: None,
        },
    })
}

/// Build the texture for a client buffer, borrowing the client's mapping.
///
/// No pixels are copied: the image points into the shm mapping and holds the
/// buffer's guard, which is what keeps `wl_buffer.release` from being sent
/// while the backend is still reading. The cache is consulted only to work out
/// what the backend already has, so damage can be expressed against it.
fn ensure_buffer_image(
    state: &CompositorState,
    cache: &mut SceneCache,
    key: ClientObjectId,
) -> Option<Arc<TextureImage>> {
    let buffer = state.buffers.get(&key)?;
    // Only an upload comes through here. A dma-buf is imported instead, and
    // none of the mapping, stride or damage reasoning below applies to it.
    let shm = buffer.shm()?;
    if buffer.width <= 0 || buffer.height <= 0 {
        return None;
    }
    // The buffer's own handle on its memory, which is the mapping that will
    // actually be read. Not the pool's current one: a pool id is recycled the
    // moment its object is destroyed, so by now `shm_pools` may hold an
    // entirely different pool under this buffer's `pool_id` — and bounds-
    // checking against one mapping while reading another is how a check comes
    // to mean nothing.
    let Some(guard) = state.buffer_guards.get(&key) else {
        debug!("Buffer {key:?} has no mapping to read");
        return None;
    };
    let mapping = guard.mapping();

    let width = buffer.width.unsigned_abs() as usize;
    let height = buffer.height.unsigned_abs() as usize;
    let stride = shm.stride.unsigned_abs() as usize;
    let offset = shm.offset.unsigned_abs() as usize;
    let row_bytes = width * BYTES_PER_PIXEL;
    if stride < row_bytes {
        debug!("Buffer stride {stride} is shorter than its rows for {key:?}");
        return None;
    }

    // The end offset of the buffer data in the pool. The last row needs no
    // stride padding, so this is less than `offset + height * stride`. It must
    // be within the mapping or every read past it is out of bounds.
    let extent = offset + (height - 1) * stride + row_bytes;
    if extent > mapping.size() {
        debug!(
            "Buffer exceeds pool mapping: end={extent} mapping={} buffer={key:?}",
            mapping.size()
        );
        return None;
    }

    let previous = cache.buffers.get(&key).copied();

    // Damage describes a change *from* the image the backend already holds, so
    // it is only usable if there is one and it is the same shape. A resized
    // buffer shares nothing with its predecessor even where rectangles overlap.
    let unchanged = previous.is_some_and(|p| p.serial == buffer.content_serial);
    let damage = match previous {
        Some(p) if !unchanged && p.width == buffer.width && p.height == buffer.height => {
            shm.damage.clone().unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let previous_serial = (!unchanged).then_some(previous.map(|p| p.serial)).flatten();

    // GL addresses rows in whole pixels, so a stride that is not a multiple of
    // four cannot be described to it. Repacking is the only way to draw such a
    // buffer at all; no real toolkit produces one.
    let pixels = if stride.is_multiple_of(BYTES_PER_PIXEL) {
        UploadPixels::Mapped {
            guard: guard.clone(),
            offset,
            stride,
        }
    } else {
        UploadPixels::Owned(repack_rows(mapping, offset, stride, row_bytes, height)?)
    };

    let image = Arc::new(TextureImage {
        id: TextureId::Buffer(key.0, key.1),
        serial: buffer.content_serial,
        width: buffer.width,
        height: buffer.height,
        format: if shm.format == FORMAT_XRGB8888 {
            PixelFormat::Xrgb8888
        } else {
            PixelFormat::Argb8888
        },
        source: TextureSource::Upload {
            pixels,
            previous_serial,
            damage,
        },
    });
    cache.buffers.insert(
        key,
        CachedImage {
            serial: buffer.content_serial,
            width: buffer.width,
            height: buffer.height,
        },
    );
    Some(image)
}

/// Copy a buffer's rows out of the mapping, tightly packed.
///
/// Only for layouts GL cannot read in place; the normal path takes no copy.
fn repack_rows(
    mapping: &PoolMapping,
    offset: usize,
    stride: usize,
    row_bytes: usize,
    height: usize,
) -> Option<Box<[u8]>> {
    let mut out = vec![0u8; height * row_bytes];
    for y in 0..height {
        // SAFETY: the caller checked the buffer's extent against the mapping,
        // and the client may not write to a committed buffer before release.
        let src = unsafe { mapping.slice(offset + y * stride, row_bytes)? };
        out[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(src);
    }
    Some(out.into_boxed_slice())
}

/// Draw one band of layer surfaces.
///
/// Called twice per output, once for the surfaces under the windows and once
/// for those above. Splitting the stack around the windows is the whole
/// purpose of the protocol: a wallpaper is not a window drawn first, it is a
/// thing that can never be above one.
///
/// A layer surface that has not acknowledged a configure is skipped. Until it
/// has, its size is something the compositor proposed and the client has not
/// agreed to, so anything it drew is at the wrong size by definition.
/// One window's surface tree as a single quad carrying its shell-given
/// presentation: the elements compose into a [`SceneGroup`] at full opacity,
/// and the alpha, transform and effect apply to the group.
///
/// `None` for an empty tree — a window whose surfaces have no buffers yet is
/// drawn nowhere, presentation or not. The group's quad is the tree's
/// bounding box, so a transform's element-local pivot is in window
/// coordinates and a centre pivot is `width / 2.0, height / 2.0` of the box.
fn presented_group(
    tree: Vec<SceneElement>,
    presentation: &SurfacePresentation,
) -> Option<SceneElement> {
    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for element in &tree {
        let (x, y, w, h) = element.dst;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
    }
    if tree.is_empty() || !(min_x.is_finite() && min_y.is_finite()) {
        return None;
    }

    // Rebased into canvas coordinates: the group composes in its own space
    // with (0, 0) at the box's top-left.
    let elements = tree
        .into_iter()
        .map(|mut element| {
            element.dst.0 -= min_x;
            element.dst.1 -= min_y;
            element
        })
        .collect();

    Some(SceneElement {
        content: SceneContent::Group(Arc::new(SceneGroup {
            size: (max_x - min_x, max_y - min_y),
            // Transparent, so the group is only its own elements.
            background: 0,
            elements,
        })),
        dst: (min_x, min_y, max_x - min_x, max_y - min_y),
        transform: presentation.transform,
        effect: presentation.effect.clone(),
        alpha: presentation.alpha.clamp(0.0, 1.0),
        // A presentation exists to blend; and even at alpha 1.0 with only a
        // transform, the quad's corners no longer promise full coverage.
        opaque: false,
    })
}

fn push_layers(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    output_id: tokio_way_backends::outputs::OutputId,
    above_windows: bool,
    origin_x: i32,
    origin_y: i32,
) {
    for key in super::layer::keys_in_band(state, output_id, above_windows) {
        let Some(layer_key) = state.layer_surface_of(key) else {
            continue;
        };
        if !state.layer_surfaces[&layer_key].configured {
            continue;
        }
        let Some(geometry) = super::layer::geometry(state, layer_key) else {
            continue;
        };
        push_surface_tree(
            state,
            cache,
            elements,
            key,
            geometry.x - origin_x,
            geometry.y - origin_y,
        );
    }
}

/// Add the icon of a drag in progress to the scene.
///
/// The icon follows the pointer, offset by whatever the client attached it
/// with. That offset is the only means a client has to position its icon — a
/// toolkit centres one under the cursor by attaching at a negative dx and dy —
/// which is why [`crate::state::Surface::offset`] is
/// tracked at all.
///
/// Nothing has to be unwound when the drag ends: this reads `state.drag`, so
/// clearing the drag stops drawing the icon on the next frame.
fn push_drag_icon(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    output: &Output,
    origin_x: i32,
    origin_y: i32,
) {
    let Some(icon) = state.drag.as_ref().and_then(|drag| drag.icon) else {
        return;
    };
    let cx = f64_to_i32(state.cursor_x);
    let cy = f64_to_i32(state.cursor_y);
    // The pointer is over one output at a time, and so is what it is carrying.
    if !output_contains(output, cx, cy) {
        return;
    }
    let offset = state.surfaces.get(&icon).map_or((0, 0), |s| s.offset);

    // The whole tree: an icon surface can never itself be a subsurface, but it
    // may have them.
    push_surface_tree(
        state,
        cache,
        elements,
        icon,
        cx - origin_x + offset.0,
        cy - origin_y + offset.1,
    );
}

/// Build the pointer cursor, positioned in the coordinates of the output it is
/// over.
///
/// Kept out of the scene so pointer motion neither recomposes a scene nor
/// perturbs its serial or damage, and so a backend with a cursor plane can move
/// it on its own — see [`tokio_way_backends::scene_graph::SceneGraph`]. The elements come out in the
/// same logical space and scale as that output's scene, ready to composite on
/// top of it.
pub fn build_cursor(
    state: &CompositorState,
    cache: &mut SceneCache,
) -> tokio_way_backends::scene_graph::Cursor {
    let cx = f64_to_i32(state.cursor_x);
    let cy = f64_to_i32(state.cursor_y);
    // The pointer is over one output at a time; if it is over none there is
    // nothing to draw.
    let Some(output) = state.outputs.iter().find(|o| output_contains(o, cx, cy)) else {
        return tokio_way_backends::scene_graph::Cursor::default();
    };
    let mut elements = Vec::new();
    push_cursor(
        state,
        cache,
        &mut elements,
        output,
        output.geometry.x,
        output.geometry.y,
    );
    tokio_way_backends::scene_graph::Cursor {
        output: Some(output.id),
        elements,
        // The pacer stamps the serial; it owns the counter.
        serial: 0,
    }
}

/// Add the pointer cursor to `elements`, in `output`'s coordinates.
///
/// A client that has set its own cursor surface gets that; a client that asked
/// for a hidden cursor gets nothing. Otherwise the compositor draws the theme
/// cursor, or its built-in one if no theme loaded.
fn push_cursor(
    state: &CompositorState,
    cache: &mut SceneCache,
    elements: &mut Vec<SceneElement>,
    output: &Output,
    origin_x: i32,
    origin_y: i32,
) {
    let cx = f64_to_i32(state.cursor_x);
    let cy = f64_to_i32(state.cursor_y);
    // The pointer is over one output at a time; the others draw no cursor.
    if !output_contains(output, cx, cy) {
        return;
    }
    let (cx, cy) = (cx - origin_x, cy - origin_y);

    let shape = match client_cursor(state) {
        CursorChoice::Hidden => return,
        CursorChoice::Surface(surface_key, hotspot_x, hotspot_y) => {
            push_surface(
                state,
                cache,
                elements,
                surface_key,
                cx - hotspot_x,
                cy - hotspot_y,
            );
            return;
        }
        // A shape whose image is not loaded falls back to the ordinary pointer.
        // `set_shape` only records shapes it managed to load, so this is the
        // theme changing underneath a running compositor rather than anything a
        // client did.
        CursorChoice::Shape(names) => {
            Some(names).filter(|names| state.cursor_shape_images.contains_key(*names))
        }
        CursorChoice::Compositor => None,
    };

    let themed = match shape {
        Some(names) => state.cursor_shape_images.get(names),
        None => state.default_cursor.as_ref(),
    };
    let (id, hotspot_x, hotspot_y) = match themed {
        Some(cursor) => (TextureId::DefaultCursor, cursor.hotspot_x, cursor.hotspot_y),
        None => (TextureId::FallbackCursor, 0, 0),
    };
    let Some(texture) = ensure_cursor_image(state, cache, id, shape) else {
        return;
    };
    let (w, h) = (texture.width, texture.height);
    elements.push(SceneElement {
        content: SceneContent::Texture {
            image: texture,
            src: (0.0, 0.0, f64::from(w), f64::from(h)),
            transform: tokio_way_backends::scene_graph::BufferTransform::Normal,
        },
        dst: dst_rect(cx - hotspot_x, cy - hotspot_y, w, h),
        transform: ElementTransform::IDENTITY,
        effect: None,
        alpha: 1.0,
        // A cursor is mostly transparent, whatever else it is.
        opaque: false,
    });
}

/// Look at compositor state and decide on a mouse cursor to display
fn client_cursor(state: &CompositorState) -> CursorChoice {
    let Some((pointer_client, _)) = state.pointer_surface else {
        return CursorChoice::Compositor;
    };
    // A named shape first: setting one clears any cursor surface and setting a
    // surface clears the shape, so at most one of the two is ever present, and
    // the order here is only for the reader.
    if let Some(&names) = state.cursor_shapes.get(&pointer_client) {
        return CursorChoice::Shape(names);
    }
    match state.cursor_surfaces.get(&pointer_client) {
        Some(None) => CursorChoice::Hidden,
        Some(&Some((surface_id, hotspot_x, hotspot_y))) => {
            let surface_key = (pointer_client, surface_id);
            // No buffer attached yet — fall back rather than show nothing.
            if state
                .surfaces
                .get(&surface_key)
                .and_then(|s| s.buffer_id)
                .is_none()
            {
                return CursorChoice::Compositor;
            }
            CursorChoice::Surface(surface_key, hotspot_x, hotspot_y)
        }
        None => CursorChoice::Compositor,
    }
}

/// Return the compositor's own cursor image, building it on first use.
fn ensure_cursor_image(
    state: &CompositorState,
    cache: &mut SceneCache,
    id: TextureId,
    shape: Option<&'static [&'static str]>,
) -> Option<Arc<TextureImage>> {
    // One texture slot holds whichever themed cursor is wanted now. `TextureId`
    // is the backend's enum and has no room for a variant per shape, and there
    // is no call for one: a pointer shows one cursor at a time, so a change of
    // shape drops the slot and rebuilds it — and the fresh serial is what tells
    // the backend to upload again. A cursor is a few kilobytes and shapes change
    // when the pointer crosses a window edge, not per frame.
    if id == TextureId::DefaultCursor && cache.cursor_shape != shape {
        cache.cursors.remove(&TextureId::DefaultCursor);
        cache.cursor_shape = shape;
    }

    if let Some(image) = cache.cursors.get(&id) {
        return Some(image.clone());
    }

    let (width, height, argb) = match id {
        TextureId::DefaultCursor => {
            let cursor = match shape {
                Some(names) => state.cursor_shape_images.get(names)?,
                None => state.default_cursor.as_ref()?,
            };
            (cursor.width, cursor.height, cursor.pixels.clone())
        }
        TextureId::FallbackCursor => fallback_cursor_pixels(),
        TextureId::Buffer(..) => return None,
    };

    cache.next_cursor_serial += 1;
    let image = Arc::new(TextureImage {
        id,
        serial: cache.next_cursor_serial,
        width,
        height,
        format: PixelFormat::Argb8888,
        source: TextureSource::Upload {
            pixels: UploadPixels::Owned(argb_to_bytes(&argb)),
            // Cursor images never change once built, so there is nothing to
            // patch and nothing to patch against.
            previous_serial: None,
            damage: Vec::new(),
        },
    });
    cache.cursors.insert(id, image.clone());
    Some(image)
}

/// Rasterise the built-in cursor bitmap into premultiplied ARGB pixels.
fn fallback_cursor_pixels() -> (i32, i32, Vec<u32>) {
    let height = i32::try_from(FALLBACK_CURSOR_BITMAP.len()).unwrap_or(0);
    let mut pixels =
        Vec::with_capacity(FALLBACK_CURSOR_BITMAP.len() * FALLBACK_CURSOR_WIDTH as usize);
    for row in FALLBACK_CURSOR_BITMAP {
        for &ch in row {
            pixels.push(match ch {
                b'B' => CURSOR_BLACK,
                b'W' => CURSOR_WHITE,
                // Fully transparent, and premultiplied, so it blends to nothing.
                _ => 0,
            });
        }
    }
    (FALLBACK_CURSOR_WIDTH, height, pixels)
}

/// Flatten `0xAARRGGBB` words into the little-endian `[B, G, R, A]` byte order
/// that shm buffers already use, so both take the same upload path.
fn argb_to_bytes(pixels: &[u32]) -> Box<[u8]> {
    let mut out = Vec::with_capacity(pixels.len() * BYTES_PER_PIXEL);
    for &p in pixels {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out.into_boxed_slice()
}

/// The cursor theme and size the environment asks for.
///
/// `XCURSOR_THEME` and `XCURSOR_SIZE` are the only configuration a cursor theme
/// has ever had, and every toolkit reads the same two, so a compositor that
/// invented its own would be the one thing on the desktop showing a different
/// cursor.
fn theme_settings() -> (String, u32) {
    let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_string());
    let size = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(24);
    (theme, size)
}

/// Load one cursor from the system theme, trying each name in turn.
///
/// Several names per cursor because themes disagree about what things are
/// called — see the table in
/// [`super::protocol::wp_cursor_shape_device`]. `None` means the theme has none
/// of them, which is the theme's business and not something to fail over.
pub fn load_theme_cursor(names: &[&str]) -> Option<DefaultCursor> {
    let (theme_name, target_size) = theme_settings();
    let theme = xcursor::CursorTheme::load(&theme_name);

    let (name, cursor_path) = names
        .iter()
        .find_map(|name| theme.load_icon(name).map(|path| (*name, path)))?;
    let content = std::fs::read(&cursor_path).ok()?;
    let images = xcursor::parser::parse_xcursor(&content)?;

    // Pick the image closest to the requested size.
    let image = images
        .iter()
        .min_by_key(|img| (img.size.cast_signed() - target_size.cast_signed()).unsigned_abs())?;

    // The xcursor file stores pixels as little-endian 32-bit ARGB (premultiplied alpha).
    // The crate's `pixels_rgba` is the raw file bytes: [B, G, R, A] per pixel on LE systems.
    // Reading as LE u32 gives us 0xAARRGGBB directly, matching our texture format.
    let pixels: Vec<u32> = image
        .pixels_rgba
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    info!(
        "Loaded cursor '{}' from theme '{}': {}x{} hotspot=({},{}) from {:?}",
        name, theme_name, image.width, image.height, image.xhot, image.yhot, cursor_path
    );

    Some(DefaultCursor {
        pixels,
        width: image.width.cast_signed(),
        height: image.height.cast_signed(),
        hotspot_x: image.xhot.cast_signed(),
        hotspot_y: image.yhot.cast_signed(),
    })
}

/// The names the ordinary pointer goes by — the cursor shown when no client has
/// asked for anything else. The same two the `default` shape uses.
pub const DEFAULT_CURSOR_NAMES: &[&str] = &["default", "left_ptr"];

/// Load the cursor shown when nothing has asked for another.
pub fn load_default_cursor() -> Option<DefaultCursor> {
    load_theme_cursor(DEFAULT_CURSOR_NAMES)
}

/// Whether a client has promised its whole surface is opaque.
///
/// Only a region covering the surface outright counts. A partial promise is
/// real information, but acting on it would mean splitting the quad along the
/// region's edges, and one quad per window is what makes this renderer simple —
/// so the useful case, a window that says all of it is opaque, is the one taken.
fn surface_is_opaque(surface: &crate::state::Surface, width: i32, height: i32) -> bool {
    let Some(region) = &surface.opaque_region else {
        return false;
    };
    region.iter().any(|rect| {
        rect.op == crate::state::RegionOp::Add
            && rect.x <= 0
            && rect.y <= 0
            && rect.x.saturating_add(rect.width) >= width
            && rect.y.saturating_add(rect.height) >= height
    })
}

/// Flash an output that a client has rung the bell on.
///
/// A translucent quad over the whole display, drawn above every window so it
/// cannot be missed, and below the cursor so the pointer stays visible while it
/// is up. It is deliberately not a full-brightness blank: the alert is the
/// change, and a screen the user cannot read through is worse than one they can.
fn push_bell(state: &CompositorState, elements: &mut Vec<SceneElement>, output: &Output) {
    if !state.bell_until.contains_key(&output.id) {
        return;
    }
    let (width, height) = output.logical_size();
    elements.push(SceneElement {
        content: SceneContent::Color(BELL_FLASH_COLOR),
        dst: dst_rect(0, 0, width, height),
        transform: ElementTransform::IDENTITY,
        effect: None,
        alpha: 1.0,
        opaque: false,
    });
}
