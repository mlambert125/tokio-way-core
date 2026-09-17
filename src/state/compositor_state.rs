//! Global compositor state.

use super::client_state::Clients;
use crate::Settings;
use crate::protocol::wire_utils::f64_to_i32;
use crate::protocol::wl_data_device_manager::{
    DND_ACTION_ASK, DND_ACTION_COPY, DND_ACTION_MOVE, DND_ACTION_NONE,
};
use crate::protocol::wl_pointer;
use crate::protocol::{cancel_source, wl_data_device, wl_data_offer, wl_data_source};
use crate::protocol::{zwlr_data_control_device, zwp_primary_selection_device};
use crate::shell::{NoShell, Shell};
use enumflags2::{BitFlags, bitflags};
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};
use strum::FromRepr;
use tokio_way_backends::dma::{DmabufImage, DmabufPlane};
use tokio_way_backends::outputs::{Output, OutputId, Scale, cursor_bounds, output_contains};
use tokio_way_backends::scene_graph::{BufferTransform, TextureRect};
use tokio_way_backends::shm::{BufferGuard, PoolMapping};

/// The furthest a surface may sit from its parent, in either direction
pub const MAX_SURFACE_OFFSET: i32 = 1 << 20;

/// The most planes a buffer can have, per `zwp_linux_buffer_params_v1`
pub const MAX_DMABUF_PLANES: usize = 4;

/// How long the visual bell stays on screen.
const BELL_DURATION: Duration = Duration::from_millis(120);

/// How many input serials are remembered per client for [`CompositorState::recent_input_serials`].
const RECENT_INPUT_SERIALS: usize = 32;

/// A client object-id tuple holding a `client_id` paired with an object id.  This pair
/// is needed because object ids are only unique per client, so the `client_id` is paired
/// to make it unique
pub type ClientObjectId = (u32, u32);

/// A shared memory pool
#[derive(Debug)]
pub struct ShmPool {
    /// Client owning the pool
    pub client_id: u32,
    /// The file descriptor pointing to the shared memory
    pub fd: RawFd,
    /// The size of the shared memory
    pub size: u32,
    /// A live mmap of the shared memory. `None` if the mapping failed
    pub mapping: Option<Arc<PoolMapping>>,
    /// True after `wl_shm_pool.destroy` - the pool will be freed once no buffers reference it
    pub dead: bool,
}

/// A client `wl_buffer`, whatever kind of memory is behind it
#[derive(Debug)]
pub struct Buffer {
    /// Client owning the buffer
    pub client_id: u32,
    /// Width of this buffer in pixels
    pub width: i32,
    /// Height of this buffer in pixels
    pub height: i32,
    /// Identifies the current contents of this buffer
    pub content_serial: u64,
    /// Where the pixels live
    pub kind: BufferKind,
}

impl Buffer {
    /// The shm details, for pool bookkeeping (None if it's not Shm)
    pub fn shm(&self) -> Option<&ShmBuffer> {
        match &self.kind {
            BufferKind::Shm(shm) => Some(shm),
            BufferKind::Dmabuf(_) | BufferKind::Failed => None,
        }
    }

    /// Mutable shm details, for pool bookkeeping (None if it's not Shm)
    pub fn shm_mut(&mut self) -> Option<&mut ShmBuffer> {
        match &mut self.kind {
            BufferKind::Shm(shm) => Some(shm),
            BufferKind::Dmabuf(_) | BufferKind::Failed => None,
        }
    }
}

/// What kind of memory is behind a `wl_buffer`
#[derive(Debug)]
pub enum BufferKind {
    /// Client shm memory the compositor maps and the backend uploads from
    Shm(ShmBuffer),
    /// A GPU buffer the backend imports and samples in place.
    /// Arc so that we can keep track of who is using it.
    #[allow(dead_code)]
    Dmabuf(Arc<tokio_way_backends::dma::DmabufImage>),
    /// A buffer the compositor could not make good on.  It's invalid, but we
    /// have to present nothing/black because its already negotiated.
    #[allow(dead_code)]
    Failed,
}

/// An individual buffer in some `ShmPool`
#[derive(Debug)]
#[allow(dead_code)]
pub struct ShmBuffer {
    /// Pool Id that this buffer points into
    pub pool_id: u32,
    /// Offset into the pool where this buffer begins
    pub offset: i32,
    /// Actual byte length of each row in this buffer (includes padding, etc.)
    pub stride: i32,
    /// Format of this buffer, in `wl_shm`'s numbering rather than the DRM fourcc a dma-buf carries
    pub format: u32,
    /// What changed since the damage was last consumed, in buffer pixels
    pub damage: Option<Vec<TextureRect>>,
}

/// A pending surface (building, but not yet flipped)
#[derive(Debug, Default, Clone)]
pub struct SurfacePending {
    /// Is the buffer attached
    pub buffer_attached: bool,
    /// The buffer
    pub buffer_id: Option<u32>,
    /// `wl_surface.damage` rectangles, in surface-local coordinates.
    pub damage_surface: Vec<TextureRect>,
    /// `wl_surface.damage_buffer` rectangles, already in buffer coordinates.
    pub damage_buffer: Vec<TextureRect>,
    /// `wl_surface.frame` callbacks requested since the last commit, oldest first.
    pub frame_callbacks: Vec<u32>,
    /// Presentation feedback ids
    pub presentation_feedbacks: Vec<u32>,
    /// Pending input region
    pub input_region: PendingRegion,
    /// Pending opaque region
    pub opaque_region: PendingRegion,
    /// Pending buffer scale
    pub buffer_scale: Option<i32>,
    /// Pending buffer transform
    pub buffer_transform: Option<BufferTransform>,
    /// Pending offset
    pub offset: (i32, i32),
}

impl SurfacePending {
    /// Fold a newer commit's state into this one
    pub fn merge(&mut self, newer: Self) {
        if newer.buffer_attached {
            self.buffer_attached = true;
            self.buffer_id = newer.buffer_id;
        }
        self.damage_surface.extend(newer.damage_surface);
        self.damage_buffer.extend(newer.damage_buffer);
        self.frame_callbacks.extend(newer.frame_callbacks);
        self.presentation_feedbacks
            .extend(newer.presentation_feedbacks);
        if !matches!(newer.input_region, PendingRegion::Unchanged) {
            self.input_region = newer.input_region;
        }
        if !matches!(newer.opaque_region, PendingRegion::Unchanged) {
            self.opaque_region = newer.opaque_region;
        }
        if newer.buffer_scale.is_some() {
            self.buffer_scale = newer.buffer_scale;
        }
        if newer.buffer_transform.is_some() {
            self.buffer_transform = newer.buffer_transform;
        }
        self.offset = (
            self.offset.0.saturating_add(newer.offset.0),
            self.offset.1.saturating_add(newer.offset.1),
        );
    }
}

#[derive(Debug)]
pub struct Surface {
    /// The client that owns the surface
    pub client_id: u32,
    /// The id of the buffer for this surface
    pub buffer_id: Option<u32>,
    /// Committed `wl_surface.frame` callbacks awaiting a frame, oldest first
    pub frame_callbacks: Vec<u32>,
    /// Committed presentation feedback ids
    pub presentation_feedbacks: Vec<u32>,
    /// The surface back buffer before flip
    pub pending: SurfacePending,
    /// The parent surface id (for a subsurface or popup, etc.)
    pub parent: Option<u32>,
    /// The direct children surfaces
    pub children: Vec<u32>,
    /// Relative position if this is a subsurface or popup
    pub subsurface_position: (i32, i32),
    /// Whether this surface got its parent from `wl_subcompositor`
    pub is_subsurface: bool,
    /// The commit mode from `wl_subsurface.set_sync`/`set_desync`
    pub subsurface_sync: bool,
    /// Temporary state if this is a subsurface and committed while synchronised,
    /// waiting for the parent to actually commit
    pub cached: Option<SurfacePending>,
    /// Position of this surface
    pub position: (i32, i32),
    /// Input region for this surface
    pub input_region: Option<Vec<RegionRect>>,
    /// Opaque region for this surface
    pub opaque_region: Option<Vec<RegionRect>>,
    /// Buffer pixels to surface-local coorinate
    pub buffer_scale: i32,
    /// How the client has already transformed its buffer
    pub buffer_transform: BufferTransform,
    /// Where the surface's contents sit relative to where they would otherwise,
    /// from `wl_surface.attach`'s `dx`/`dy` and `wl_surface.offset`. (Read only
    /// by the drag icon.)
    pub offset: (i32, i32),
    /// Outputs the client has been told this surface is on
    pub entered_outputs: HashSet<OutputId>,
    /// Outputs actually showing this surface
    pub visible_on: HashSet<OutputId>,
}

/// One plane of a buffer a client is describing.
#[derive(Debug)]
pub struct PendingPlane {
    /// The descriptor and its layout.
    pub plane: DmabufPlane,
    /// The modifier the client gave for *this* plane. The protocol carries one
    /// per plane while a buffer has a single layout, so they must all agree.
    pub modifier: u64,
}

/// A `zwp_linux_buffer_params_v1`: a buffer being described one plane at a time.
#[derive(Debug, Default)]
pub struct BufferParams {
    /// Planes by index
    pub planes: [Option<PendingPlane>; MAX_DMABUF_PLANES],
    /// Set by `create`/`create_immed`. The object is single-use, so everything
    /// after that is a protocol error, which is why this is set before the import
    /// is dispatched rather than once it lands.
    pub used: bool,
}

/// A pending import. A dma-buf import the backend has been asked about and has not answered.
#[derive(Debug)]
pub struct PendingImport {
    /// Client that asked
    pub client_id: u32,
    /// The params object to answer on, if it still exists
    pub params_id: u32,
    /// For `create_immed`, the buffer already registered and the serial it was registered with.
    /// A client may destroy that id and reuse it before the verdict lands, and the serial
    /// is what tells the two buffers apart.
    pub immediate: Option<(u32, u64)>,
    /// The buffer being imported, kept alive until the verdict
    pub image: Arc<DmabufImage>,
    /// Width, for registering the buffer once the verdict is good
    pub width: i32,
    /// Height
    pub height: i32,
}

/// Layer shell layer
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, FromRepr)]
#[repr(u32)]
pub enum LayerKind {
    /// Under everything. Wallpapers
    #[default]
    Background = 0,
    /// Under ordinary windows, over the wallpaper. Widgets that windows cover.
    Bottom = 1,
    /// Over ordinary windows. Panels and bars.
    Top = 2,
    /// Over everything. Lock screens, notifications, even fullscreen apps.
    Overlay = 3,
}

impl LayerKind {
    /// Whether this layer draws over ordinary windows.
    pub fn is_above_windows(self) -> bool {
        matches!(self, Self::Top | Self::Overlay)
    }
}

/// Which edges a layer surface is anchored to, as `zwlr_layer_surface_v1` numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Anchor(pub u32);

impl Anchor {
    /// Top
    pub const TOP: u32 = 1;
    /// Bottom
    pub const BOTTOM: u32 = 2;
    /// Left
    pub const LEFT: u32 = 4;
    /// Right
    pub const RIGHT: u32 = 8;
    /// All of the
    pub const ALL: u32 = 0b1111;

    /// Is this anchored to the top
    pub fn top(self) -> bool {
        self.0 & Self::TOP != 0
    }
    /// Is this anchored to the bottom
    pub fn bottom(self) -> bool {
        self.0 & Self::BOTTOM != 0
    }
    /// Is this anchored to the left
    pub fn left(self) -> bool {
        self.0 & Self::LEFT != 0
    }
    /// Is this anchored to the right
    pub fn right(self) -> bool {
        self.0 & Self::RIGHT != 0
    }
    /// Anchored to both sides of the horizontal axis
    pub fn spans_horizontally(self) -> bool {
        self.left() && self.right()
    }
    /// Anchored to both sides of the vertical axis
    pub fn spans_vertically(self) -> bool {
        self.top() && self.bottom()
    }
}

/// Whether a layer surface wants the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, FromRepr)]
#[repr(u32)]
pub enum KeyboardInteractivity {
    /// Never focused. A wallpaper, a bar that is only clicked.
    #[default]
    None = 0,
    /// Takes the keyboard for as long as it exists, over any window. (e.g. lock screen)
    Exclusive = 1,
    /// Focused when the user clicks it, like an normal window.
    OnDemand = 2,
}

/// The double-buffered state of a layer surface.
#[derive(Debug, Clone, Default)]
pub struct LayerSurfaceStatePending {
    /// The size asked for. Zero on an axis means "as big as the anchor makes me"
    pub size: (i32, i32),
    /// The anchor(s)
    pub anchor: Anchor,
    /// How much room to reserve for this surface along the edge it is anchored
    /// to, so windows do not go under it. Negative means the surface wants no
    /// reservation *and* to ignore everyone else's.
    pub exclusive_zone: i32,
    /// Gaps outside the surface, in the order the protocol sends them.
    pub margin: (i32, i32, i32, i32),
    /// Keyboard interactivity
    pub keyboard_interactivity: KeyboardInteractivity,
    /// The kind of layer
    pub layer: LayerKind,
}

/// A `zwlr_layer_surface_v1`: a panel, bar, wallpaper or lock screen
#[derive(Debug)]
pub struct LayerSurfaceState {
    pub client_id: u32,
    /// The `wl_surface` this gives a role to
    pub wl_surface_id: u32,
    /// The output it belongs to. `None` means the client let the compositor choose
    pub output: Option<OutputId>,
    /// What the client has asked for and not yet committed
    pub pending: LayerSurfaceStatePending,
    /// What is currently active
    pub current: LayerSurfaceStatePending,
    /// The client's own name for it, for a compositor with per-namespace
    /// policy. Kept because the protocol says it is immutable and clients use
    /// it to identify themselves in logs.
    pub namespace: String,
    /// Whether a configure has been acknowledged. Don't show it before this.
    pub configured: bool,
    /// Serials sent and not yet acknowledged
    pub pending_configures: VecDeque<u32>,
    /// Highest configure
    pub highest_configure: u32,
    /// The geometry last configured, so an unchanged one sends nothing
    pub configured_size: Option<(i32, i32)>,
}

/// A `zxdg_toplevel_decoration_v1` object, bound to one `xdg_toplevel`
///
/// Carries no mode of its own. The answer to that comes straight from
/// config (see [`crate::protocol::zxdg_toplevel_decoration::send_configure`]),
/// so all this holds is enough to find the object again - to refuse a second
/// one on the same toplevel, and to forget it if the toplevel goes first.
#[derive(Debug)]
pub struct DecorationState {
    /// The client owning the decoration state
    pub client_id: u32,
    /// The `xdg_toplevel` that this decoration applies to
    pub toplevel_id: u32,
}

/// Viewport state for a surface to crop/scale a surface
#[derive(Debug)]
pub struct ViewportState {
    /// The client owning the viewport state
    pub client_id: u32,
    /// The surface that this applies to
    pub surface_id: u32,
    /// The source rectangle
    pub source: Option<(f64, f64, f64, f64)>,
    /// The destination rectangle to map onto
    pub destination: Option<(i32, i32)>,
    /// Pending version of the source rectangle
    pub pending_source: Option<(f64, f64, f64, f64)>,
    /// Pending version of the destination rectangle
    pub pending_destination: Option<(i32, i32)>,
}

/// How a surface's buffer maps onto the screen.
#[derive(Debug, Clone, Copy)]
pub struct BufferMapping {
    /// Source rectangle in buffer pixels: (x, y, width, height).
    pub src: (f64, f64, f64, f64),
    /// Destination width
    pub dest_width: i32,
    /// Destination height
    pub dest_height: i32,
}

/// Which edges of a window an interactive resize is dragging.
///
/// The bit values are `xdg_toplevel.resize_edge`, passed by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeEdges(pub u32);

impl ResizeEdges {
    /// The top edge
    pub const TOP: u32 = 1;
    /// The bottom edge
    pub const BOTTOM: u32 = 2;
    /// The left edge
    pub const LEFT: u32 = 4;
    /// The right edge
    pub const RIGHT: u32 = 8;

    /// Is top one of the resize edges
    pub fn top(self) -> bool {
        self.0 & Self::TOP != 0
    }
    /// Is bottom one of the resize edges
    pub fn bottom(self) -> bool {
        self.0 & Self::BOTTOM != 0
    }
    /// Is left one of the resize edges
    pub fn left(self) -> bool {
        self.0 & Self::LEFT != 0
    }
    /// Is right one of the resize edges
    pub fn right(self) -> bool {
        self.0 & Self::RIGHT != 0
    }
}

/// What an interactive grab is doing to the window it holds with state
#[derive(Debug, Clone, Copy)]
pub enum GrabKind {
    /// Moving the window
    Move { offset_x: i32, offset_y: i32 },
    /// Resizing the window
    Resize {
        /// Edges grabbed
        edges: ResizeEdges,
        /// The starting pointer position
        start_pointer: (f64, f64),
        /// The initial size before the grab/resize
        start_size: (i32, i32),
        /// Last size sent to the client, so an unchanged size sends nothing.
        last_sent: (i32, i32),
    },
}

/// The fixed edges of a window mid-resize, and the size it was last asked to become
#[derive(Debug, Clone, Copy)]
pub struct ResizeAnchor {
    /// The toplevel's `wl_surface` key
    pub surface: ClientObjectId,
    /// Which edges the drag moves - the opposite ones are the anchor
    pub edges: ResizeEdges,
    /// Global x of the right edge when the grab started
    pub right: i32,
    /// Global y of the bottom edge when the grab started
    pub bottom: i32,
}

/// An interactive move or resize the compositor is driving
///
/// While one is held the compositor owns the pointer: motion and buttons drive
/// the grab instead of reaching the client, which is what "grab" means.
#[derive(Debug, Clone, Copy)]
pub struct PointerGrab {
    /// The toplevel's `wl_surface`
    pub surface: ClientObjectId,
    /// The `xdg_toplevel` object, needed to configure a resize
    pub toplevel: ClientObjectId,
    /// What kind of grab (move or resize)
    pub kind: GrabKind,
}

/// Whether a rectangle adds to or subtracts from a region
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionOp {
    /// Add
    Add,
    /// Subtract
    Subtract,
}

/// One add/subtract rectangle from a `wl_region`
#[derive(Debug, Clone, Copy)]
pub struct RegionRect {
    /// Is this rectangle part of the region (add) or a hole (subtract)
    pub op: RegionOp,
    /// x coordinate of the top-left corner
    pub x: i32,
    /// y coordinate of the top-left corner
    pub y: i32,
    /// The width of the rectangle
    pub width: i32,
    /// The height of the rectangle
    pub height: i32,
}

impl RegionRect {
    /// Check if this rectangle contains the point
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }
}

/// A region waiting to be applied at the next commit
#[derive(Debug, Default, Clone)]
pub enum PendingRegion {
    /// The client has not set an input region since the last commit
    #[default]
    Unchanged,
    /// Default (everything for input regions, nothing for opaque)
    Infinite,
    /// A defined region made up of several add/sub rectangles
    Rects(Vec<RegionRect>),
}

/// A region
#[derive(Debug, Default)]
pub struct Region {
    /// The client id that owns the region
    pub client_id: u32,
    /// Add/subtract rectangles
    pub rects: Vec<RegionRect>,
}

/// A role for an Xdg wrapped surface
#[derive(Debug)]
pub enum XdgRole {
    /// A top-level window
    Toplevel(u32),
    /// A popup menu
    #[allow(dead_code)]
    Popup(u32),
}

/// How many unacknowledged configures a surface may accumulate
const MAX_PENDING_CONFIGURES: usize = 256;

/// The state of a xdg wrapped surface
#[derive(Debug)]
#[allow(dead_code)]
pub struct XdgSurfaceState {
    /// Owning client
    pub client_id: u32,
    /// The surface id that is wrapped
    pub wl_surface_id: u32,
    /// The xdg role
    pub role: Option<XdgRole>,
    /// Whether the client has ever acknowledged a configure
    pub configured: bool,
    /// Geometry of the top-level wrapper
    pub geometry: Option<(i32, i32, i32, i32)>,
    /// Pending `xdg_surface.configure` serials
    pub pending_configures: VecDeque<u32>,
    /// The highest serial ever sent to this surface
    pub highest_configure: u32,
}

/// The state of an xdg top-level window
#[derive(Debug)]
#[allow(dead_code)]
pub struct XdgToplevelState {
    /// Owning client
    pub client_id: u32,
    /// Wrapped surface Id
    pub xdg_surface_id: u32,
    // Title of the top-level window
    pub title: Option<String>,
    /// App Id String for this window
    pub app_id: Option<String>,
    /// Smallest size the client says it can work at, from `set_min_size`
    pub min_size: (i32, i32),
    /// Largest size the client says it wants, from `set_max_size`
    pub max_size: (i32, i32),
    /// Is it maximized
    pub maximized: bool,
    /// Is it full-screened
    pub fullscreen: bool,
    /// Where the window was before it was maximized or made fullscreen
    pub restore: Option<(i32, i32, i32, i32)>,
    /// The toplevel this one hangs off, from `set_parent`.  (E.g. a dialog)
    pub parent: Option<u32>,
    /// The bounds last sent as `configure_bounds`
    pub sent_bounds: Option<(i32, i32)>,
}

/// Xdg popup state
#[derive(Debug)]
pub struct XdgPopupState {
    /// Owning client
    pub client_id: u32,
    /// Wrapped surface
    pub xdg_surface_id: u32,
    /// Owning xdg wrapped top window or other popup
    pub parent_xdg_surface_id: u32,
    /// The positioner id it was created with
    pub positioner: u32,
    /// X position for the popup
    pub x: i32,
    /// Y position for the popup
    pub y: i32,
    // Width
    pub width: i32,
    /// Height
    pub height: i32,
}

#[derive(Debug, Default, Clone, Copy, FromRepr)]
#[repr(u32)]
pub enum XdgPositionerAnchor {
    #[default]
    None = 0,
    Top = 1,
    Bottom = 2,
    Left = 3,
    Right = 4,
    TopLeft = 5,
    BottomLeft = 6,
    TopRight = 7,
    BottomRight = 8,
}

#[derive(Debug, Default, Clone, Copy, FromRepr)]
#[repr(u32)]
pub enum XdgPositionerGravity {
    #[default]
    None = 0,
    Top = 1,
    Bottom = 2,
    Left = 3,
    Right = 4,
    TopLeft = 5,
    BottomLeft = 6,
    TopRight = 7,
    BottomRight = 8,
}

#[derive(Debug, Clone, Copy)]
#[bitflags]
#[repr(u32)]
pub enum XdgPositionerConstraintAdjustment {
    SlideX = 1,
    SlideY = 2,
    FlipX = 4,
    FlipY = 8,
    ResizeX = 16,
    ResizeY = 32,
}

#[derive(Debug, Default, Clone)]
pub struct XdgPositionerState {
    pub client_id: u32,
    pub width: i32,
    pub height: i32,
    pub anchor_rect: (i32, i32, i32, i32),
    pub anchor: XdgPositionerAnchor,
    pub gravity: XdgPositionerGravity,
    pub offset: (i32, i32),
    pub constraint_adjustment: BitFlags<XdgPositionerConstraintAdjustment>,
    /// From `set_reactive`: the popup wants to be re-placed whenever what it is
    /// anchored to moves, rather than only when the client asks.
    pub reactive: bool,
    /// From `set_parent_size`: the size the client believes its parent window
    /// will be. Constraining happens against this when it is given, which lets
    /// a client that is about to resize get a popup placed for the size it is
    /// heading to rather than the one it is leaving.
    pub parent_size: Option<(i32, i32)>,
}

#[derive(Debug, Clone)]
pub struct PointerBinding {
    pub client_id: u32,
    pub object_id: u32,
}

#[derive(Debug, Clone)]
pub struct KeyboardBinding {
    pub client_id: u32,
    pub object_id: u32,
}

#[derive(Debug, Clone)]
pub struct TouchBinding {
    pub client_id: u32,
    pub object_id: u32,
}

/// The four masks `wl_keyboard.modifiers` carries.
///
/// Grouped rather than passed as four loose `u32`s because they are only ever
/// read and written together, and because three of the four are easy to
/// transpose at a call site that takes them positionally.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ModifierState {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

/// The compositor's keyboard: the keymap every client is handed, and the xkb
/// state that turns a key press into the modifier masks clients are told about.
///
/// This belongs to the compositor, not the backend. A keymap and the modifier
/// state derived from it have to agree, and only one of them can be
/// authoritative — a backend serialising modifiers against its host's keymap
/// while the compositor gives clients a keymap of its own is two answers to one
/// question, and they drift the moment the host's layout is not the plain one.
/// So a backend reports only which physical key moved, and this works out what
/// it means. On hardware there is no host keymap to consult in any case.
pub struct Keyboard {
    /// The keymap as text, generated once and sent to every client. `None` if
    /// none could be compiled, in which case clients are sent no keymap and the
    /// modifiers stay clear — the same as a backend that reported nothing.
    keymap_string: Option<String>,
    /// Live xkb state, fed every key so its modifier serialisation stays
    /// current. `None` travels with a `None` keymap.
    state: Option<xkbcommon::xkb::State>,
    /// Kept only to keep the C keymap alive behind `state`; never read again.
    #[allow(dead_code)]
    keymap: Option<xkbcommon::xkb::Keymap>,
    /// Kept only to keep the C context alive behind the keymap.
    #[allow(dead_code)]
    context: xkbcommon::xkb::Context,
}

// SAFETY: the xkb objects wrap raw pointers and so are not `Send` on their own,
// but they are owned solely by the compositor task and never shared or aliased.
// The multi-threaded runtime may move that task between worker threads at an
// await point, but never runs it on two at once, so there is no concurrent
// access. xkb objects have no thread affinity — only a no-aliasing requirement,
// which single ownership already satisfies.
unsafe impl Send for Keyboard {}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Keyboard {
    /// Compile the keymap and start tracking state against it.
    ///
    /// `xkb_options` is the config file's `xkb_options`: extra xkb options,
    /// comma-separated, of which `caps:escape` is the classic. An option name
    /// xkb does not recognise it quietly ignores on its own; should the string
    /// break compilation outright, this falls back to the plain default keymap
    /// rather than to no keymap at all — a broken option string should cost
    /// the options, not the whole keyboard.
    pub fn new(xkb_options: Option<&str>) -> Self {
        use xkbcommon::xkb;
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let compile = |options: Option<String>| {
            xkb::Keymap::new_from_names(
                &context,
                "",
                "",
                "",
                "",
                options,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        };
        let mut keymap = compile(xkb_options.map(str::to_owned));
        if keymap.is_none() && xkb_options.is_some() {
            tracing::warn!(
                "xkb_options {:?} did not compile; using the default keymap without them",
                xkb_options.unwrap_or_default()
            );
            keymap = compile(None);
        }
        let keymap_string = keymap
            .as_ref()
            .map(|km| km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
            .filter(|s| !s.is_empty());
        let state = keymap.as_ref().map(xkb::State::new);
        Self {
            keymap_string,
            state,
            keymap,
            context,
        }
    }

    /// The keymap text handed to clients, if one compiled.
    pub fn keymap_string(&self) -> Option<&str> {
        self.keymap_string.as_deref()
    }

    /// Feed one key transition to the xkb state and read back the four masks.
    ///
    /// `xkb_keycode` is the evdev code plus eight, which is what xkb expects.
    /// With no keymap this is a no-op returning cleared modifiers, which is what
    /// a keyboard with nothing to map should report.
    pub fn update_key(&mut self, xkb_keycode: u32, pressed: bool) -> ModifierState {
        use xkbcommon::xkb;
        let Some(state) = self.state.as_mut() else {
            return ModifierState::default();
        };
        let direction = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        state.update_key(xkb::Keycode::new(xkb_keycode), direction);
        ModifierState {
            depressed: state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            latched: state.serialize_mods(xkb::STATE_MODS_LATCHED),
            locked: state.serialize_mods(xkb::STATE_MODS_LOCKED),
            group: state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        }
    }
}

#[derive(Debug, Default)]
pub struct SeatState {
    pub has_pointer: bool,
    pub has_keyboard: bool,
    pub has_touch: bool,
}

/// What a `wl_data_source` has been spent on.
///
/// A source is single-use by protocol: offering the same one as a selection and
/// then as a drag would leave two unrelated transfers sharing one set of mime
/// types and one `cancelled`. The second use is `used_source`, which is why this
/// is recorded rather than inferred from whether the selection happens to name
/// it — a source cancelled and replaced is still used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSourceRole {
    Unused,
    Selection,
    Drag,
}

/// Which interface a source or an offer speaks.
///
/// Three of them carry content between clients now, and a selection can be
/// owned through any of them: `wl_data_source` for the clipboard,
/// `zwp_primary_selection_source_v1` for the middle-click one, and
/// `zwlr_data_control_source_v1` for either, from a client that is not in the
/// focus model at all.
///
/// One map holds all three rather than one map each, because the alternative is
/// the same distinction spread wider: an offer drawing from any of them, a
/// selection naming any of them, and every lookup asking which map to try. The
/// tag is needed either way — the `send` and `cancelled` opcodes differ between
/// `wl_data_source` and the other two — so it is better kept in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataInterface {
    /// `wl_data_source` / `wl_data_offer` — the clipboard, and drag and drop.
    WlData,
    /// `zwp_primary_selection_*_v1` — the middle-click clipboard.
    PrimarySelection,
    /// `zwlr_data_control_*_v1` — what a clipboard manager binds.
    DataControl,
}

/// A source: content one client is offering to another.
#[derive(Debug)]
pub struct DataSource {
    /// Which interface this source speaks, and so how it is told to send and
    /// told it has been cancelled.
    pub interface: DataInterface,
    /// Mime types in the order the client offered them, which is the order of
    /// its own preference and is passed on to the receiver unchanged.
    pub mime_types: Vec<String>,
    /// The `dnd_action` mask from `set_actions`.
    ///
    /// Zero means the client never called it. That is not the same as a client
    /// too old to have the request, which is taken to mean `copy` — so the
    /// default is applied where the negotiation runs rather than stored here,
    /// and the two cases stay tellable apart.
    pub actions: u32,
    /// What this source has been spent on, if anything.
    pub role: DataSourceRole,
    /// Whether the compositor has told this source it is cancelled.
    ///
    /// Every interface says a cancelled source is finished with and should be
    /// destroyed; `zwlr_data_control_source_v1` is the one that makes adding a
    /// mime type to one an error, so the fact has to be recorded rather than
    /// inferred. It cannot be inferred, either: a cancelled source stays in the
    /// map until its client destroys it, because the client still owns the id
    /// and may still send requests on it.
    pub cancelled: bool,
}

/// Which of the three lives an offer is leading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferKind {
    /// A clipboard offer, handed out on focus change.
    Selection,
    /// The offer for a drag still under the pointer.
    Drag,
    /// A drag offer that has been dropped on. The pointer is free again, but
    /// the offer outlives the drag so the target can still read from it and
    /// say when it is done.
    Dropped,
}

/// An offer: the compositor's handle on a source, held by the receiver.
///
/// The id is the compositor's rather than the client's — see
/// [`super::client_state::ClientState::allocate_id`] — because the protocol has the
/// compositor name it in `wl_data_device.data_offer`, and there is no round
/// trip in which the client could name it instead.
/// Offers of all three interfaces share this one type and one map, and unlike
/// [`DataSource`] they need no tag saying which they are. Two things make that
/// work: a request arriving on an offer is routed by the object's type, which
/// the client's own object map already records, and the only event an offer ever
/// carries — `offer`, naming a mime type — is the same opcode and the same
/// payload on all three. A source is not so lucky: its `send` and `cancelled`
/// sit at different opcodes on `wl_data_source` than on the other two, and
/// nothing about the source itself says which it is.
#[derive(Debug)]
pub struct DataOffer {
    /// The client this offer was given to. The key already says this, since the
    /// object id lives in that client's id space; it is kept because teardown
    /// walks values rather than keys.
    pub client_id: u32,
    /// The source behind this offer, or `None` once that source has gone.
    ///
    /// An offer outlives its source, and it outlives the compositor caring
    /// about it. The client still owns the id and will still send requests on
    /// it, and those have to be answered — what is lost is anything to answer
    /// them with, so a `receive` closes the pipe rather than hanging.
    pub source: Option<ClientObjectId>,
    /// Which half of the protocol this offer belongs to. The three take
    /// different request paths: `finish` and the action events are drag-only,
    /// and `finish` is legal only once the drop has happened.
    pub kind: OfferKind,
    /// The mime type last passed to `accept`, relayed to the source as
    /// `wl_data_source.target`. `None` means the receiver will take nothing.
    pub accepted: Option<String>,
    /// The `dnd_action` mask from `set_actions`, and the one action within it
    /// the receiver would rather have.
    pub actions: u32,
    pub preferred_action: u32,
    /// The action last settled between the two sides, so an `action` event goes
    /// out only when it changes.
    pub resolved_action: u32,
}

/// A `wl_data_device`.
///
/// A flat list, keyed the way `wl_pointer` and `wl_keyboard` bindings are:
/// every delivery is a fan-out over all of them, and `Clients::get` borrows
/// mutably, so the list has to be cloned out before the sends begin.
#[derive(Debug, Clone)]
pub struct DataDeviceBinding {
    pub client_id: u32,
    pub object_id: u32,
}

/// A `zwp_primary_selection_device_v1`, kept the way [`DataDeviceBinding`] is
/// and for the same reason: every delivery is a fan-out over all of them.
#[derive(Debug, Clone)]
pub struct PrimaryDeviceBinding {
    pub client_id: u32,
    pub object_id: u32,
}

/// A `wp_fractional_scale_v1`: one surface's standing question about what scale
/// to draw at, and the last answer it was given.
///
/// Keyed by the object rather than by the surface, because `destroy` arrives on
/// the object and the protocol's one rule — at most one per surface — is cheap
/// to check the other way round over a map this small.
#[derive(Debug, Clone)]
pub struct FractionalScaleBinding {
    /// The `wl_surface` this is about, in the same client's id space.
    pub surface_id: u32,
    /// The scale last sent, in 120ths, so an unchanged one is not repeated. A
    /// client acts on every `preferred_scale` it receives by re-rendering, so
    /// saying the same thing twice costs it a frame.
    pub sent: Option<u32>,
}

/// A `zwlr_data_control_device_v1`. The same shape again, and the one told
/// about every selection rather than only the focused client's.
#[derive(Debug, Clone)]
pub struct DataControlDeviceBinding {
    pub client_id: u32,
    pub object_id: u32,
}

/// A drag the compositor is carrying between clients.
///
/// Deliberately not a third [`GrabKind`]. A move or resize acts on a window: it
/// writes a position and sends a configure. A drag writes no geometry at all
/// and delivers protocol to a *third* client, so both of [`PointerGrab`]'s
/// fields would be meaningless for it and the two would share not one line of
/// how they are driven. They cannot both be held at once — an interactive grab
/// swallows the button press, so the client never receives a serial to quote at
/// `start_drag` — and that is enforced where each begins rather than assumed.
#[derive(Debug, Clone)]
pub struct Drag {
    /// The content being dragged, or `None` for a client dragging within itself
    /// with nothing to hand anyone else. That case is why the origin client is
    /// stored separately rather than read off the source.
    pub source: Option<ClientObjectId>,
    /// The client that started the drag, which owns the pointer until it ends.
    pub origin_client: u32,
    /// The `wl_surface` the drag started from.
    pub origin: ClientObjectId,
    /// The drag icon, if the client gave one. Follows the pointer.
    pub icon: Option<ClientObjectId>,
    /// The surface under the pointer. `None` between surfaces, and once a
    /// target has gone away.
    pub focus: Option<ClientObjectId>,
    /// The offers the focus's client was given, one per data device it holds.
    ///
    /// A list rather than a single offer because `enter` is a per-device event:
    /// a client with two devices is entered twice and needs an offer for each,
    /// and it may then negotiate through either of them. Empty for a drag with
    /// no source, and for a target that has destroyed what it was given.
    pub focus_offers: Vec<ClientObjectId>,
}

pub struct DefaultCursor {
    pub pixels: Vec<u32>,
    pub width: i32,
    pub height: i32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

// The compositor's whole state lives here; a handful of independent flags among
// its many fields is expected, not a sign one should be an enum.
#[allow(clippy::struct_excessive_bools)]
pub struct CompositorState {
    /// The mechanism's knobs, fixed at construction.
    pub settings: Settings,
    /// The window-management policy. Living here is what lets a protocol
    /// handler deep in the call tree ask it a question; the discipline that
    /// comes with that is documented on [`crate::shell`].
    pub shell: Box<dyn Shell>,
    pub clients: Clients,
    pub shm_pools: HashMap<ClientObjectId, ShmPool>,
    pub buffers: HashMap<ClientObjectId, Buffer>,
    pub surfaces: HashMap<ClientObjectId, Surface>,
    pub regions: HashMap<ClientObjectId, Region>,
    pub xdg_surfaces: HashMap<ClientObjectId, XdgSurfaceState>,
    pub xdg_toplevels: HashMap<ClientObjectId, XdgToplevelState>,
    pub xdg_popups: HashMap<ClientObjectId, XdgPopupState>,
    pub xdg_positioners: HashMap<ClientObjectId, XdgPositionerState>,
    pub seat: SeatState,
    pub outputs: Vec<Output>,
    /// Maps `OutputId` -> `wl_registry` global name for dynamic output globals.
    pub output_global_names: HashMap<OutputId, u32>,
    /// Maps (`client_id`, `wl_output_object_id`) -> which output the binding refers to.
    pub output_bindings: HashMap<ClientObjectId, OutputId>,
    /// Counter for assigning unique `wl_registry` global names to new outputs.
    pub next_global_number: u32,
    pub pointers: Vec<PointerBinding>,
    pub keyboards: Vec<KeyboardBinding>,
    pub touches: Vec<TouchBinding>,
    /// Fingers currently down, and the surface each landed on.
    ///
    /// A touch point belongs to the surface it *started* on for its whole
    /// life, however far it then travels — dragging a finger off a window does
    /// not hand the gesture to whatever is underneath, which is what makes a
    /// swipe that leaves the window still reach the client that owns it.
    pub touch_points: HashMap<i32, ClientObjectId>,
    pub focused_surface: Option<ClientObjectId>,
    /// The specific surface (possibly a subsurface) currently under the pointer.
    /// Used for delivering pointer enter/leave/motion/button to the correct surface.
    pub pointer_surface: Option<ClientObjectId>,
    pub cursor_x: f64,
    pub cursor_y: f64,
    /// Currently pressed evdev keycodes (for `wl_keyboard.enter` keys array).
    pub pressed_keys: HashSet<u32>,
    /// The xkb modifier state the backend last reported.
    ///
    /// Kept because `wl_keyboard.modifiers` is owed at moments when no key is
    /// being pressed: right after an `enter`, and when a client binds a
    /// keyboard for a surface that already has focus. A client told nothing at
    /// those points believes no modifier is held, so focusing a window with
    /// Ctrl down makes its next click a plain click.
    pub modifiers: ModifierState,
    /// The keymap clients are given and the xkb state modifiers are read from.
    /// The compositor owns both so they cannot disagree — see [`Keyboard`].
    pub keyboard: Keyboard,
    /// The cursor has moved or changed appearance and its frame must be rebuilt.
    ///
    /// Separate from `dirty` because the cursor lives outside the scene: pointer
    /// motion is the most frequent event there is, and marking it here rather
    /// than as scene staleness is what keeps it from recomposing a scene and
    /// re-deriving its damage on every mouse event.
    pub cursor_dirty: bool,
    /// Maps `wl_subsurface` (`client_id`, `object_id`) -> the `wl_surface` object id it controls.
    pub subsurface_map: HashMap<ClientObjectId, u32>,
    /// `wp_viewport` objects keyed by (`client_id`, `viewport_object_id`).
    pub viewports: HashMap<ClientObjectId, ViewportState>,
    /// Reverse map: (`client_id`, `surface_id`) -> `viewport_object_id`.
    pub surface_viewport: HashMap<ClientObjectId, u32>,
    /// `zwlr_layer_surface_v1` objects, keyed by the layer surface object.
    pub layer_surfaces: HashMap<ClientObjectId, LayerSurfaceState>,
    /// Reverse map: (`client_id`, `wl_surface_id`) -> the layer surface object
    /// giving it its role. What tells a `wl_surface` apart from an ordinary
    /// one when all that is in hand is the surface.
    pub surface_layer: HashMap<ClientObjectId, u32>,
    /// `zxdg_toplevel_decoration_v1` objects keyed by (`client_id`, `decoration_object_id`).
    pub decorations: HashMap<ClientObjectId, DecorationState>,
    /// Reverse map: (`client_id`, `toplevel_id`) -> `decoration_object_id`, so a
    /// second `get_toplevel_decoration` on the same toplevel can be refused.
    pub toplevel_decoration: HashMap<ClientObjectId, u32>,
    /// Buffers to release on the next render (old buffers replaced by commit).
    pub buffers_pending_release: Vec<ClientObjectId>,
    /// dma-buf formats the backend can import, from
    /// [`tokio_way_backends::messages::BackendMessage::DmabufSupport`].
    ///
    /// Empty until the backend has answered, and empty for good if it cannot
    /// import at all — which is what `zwp_linux_dmabuf_v1` will be advertised
    /// on. A compositor that offers dma-buf it cannot draw is worse than one
    /// that offers none: the client allocates for it and then has nothing to
    /// fall back to.
    pub dmabuf_formats: Vec<tokio_way_backends::dma::DmabufFormat>,
    /// The `wl_registry` name `zwp_linux_dmabuf_v1` was advertised under, once
    /// there is something to advertise. `None` means clients have not been
    /// offered dma-buf and cannot reach any of it.
    pub dmabuf_global_name: Option<u32>,
    /// Live `zwp_linux_buffer_params_v1` objects, keyed like every other.
    pub dmabuf_params: HashMap<ClientObjectId, BufferParams>,
    /// Imports the backend has been asked about and not yet answered.
    pub pending_dmabuf_imports: HashMap<u64, PendingImport>,
    /// Source of import tokens. Never reused, so a verdict cannot be matched to
    /// a later import.
    pub next_import_token: u64,
    /// Where to send work only the backend thread can do. `None` in tests,
    /// where there is no backend: a client's `create` is then answered `failed`
    /// rather than left waiting for a verdict that cannot come.
    pub backend_sender:
        Option<tokio::sync::mpsc::Sender<tokio_way_backends::messages::BackendRequest>>,
    /// Whether visual state has changed and a re-render is needed.
    pub dirty: bool,
    /// Whether the shell has asked the whole compositor to shut down — an
    /// `exit` keybind, typically. A flag rather than a channel because the
    /// shell's hooks already hand state changes back this way (`dirty`,
    /// above), and the run loop reads it once per pass, which is prompt
    /// enough for a command a human just typed.
    pub exit_requested: bool,
    /// Per-client cursor: `client_id` -> `Some((surface_id, hotspot_x, hotspot_y))` or `None` (hidden).
    pub cursor_surfaces: HashMap<u32, Option<(u32, i32, i32)>>,
    /// Most recent `wl_pointer.enter` serial per client, for `set_cursor` validation.
    pub pointer_enter_serial: HashMap<u32, u32>,
    /// Surfaces with the cursor role (permanent, prevents other role assignment).
    pub cursor_role_surfaces: HashSet<ClientObjectId>,
    /// Pre-loaded cursor from the system cursor theme, used when no client cursor is set.
    pub default_cursor: Option<DefaultCursor>,
    /// Stack of popup (`client_id`, `xdg_popup_id`) that have called `grab`, newest on top.
    /// When the user clicks outside the topmost grabbed popup, it is dismissed.
    pub grabbed_popups: Vec<ClientObjectId>,
    /// The interactive move or resize in progress, if any.
    pub pointer_grab: Option<PointerGrab>,
    /// The fixed edges of the window a resize grab is (or was just) driving.
    /// Set by [`Self::start_resize_grab`], read as commits apply — see
    /// [`Self::apply_resize_anchor`].
    pub resize_anchor: Option<ResizeAnchor>,
    /// Mouse buttons currently held, as evdev codes.
    pub pressed_buttons: HashSet<u32>,
    /// Serial of each client's most recent `wl_pointer.button` press, which a
    /// client must quote when it asks to start a move or resize.
    pub last_button_serial: HashMap<u32, u32>,
    /// Source of `ShmBuffer::content_serial`. Only ever incremented, so no two
    /// buffer contents in the lifetime of the compositor share a serial.
    pub next_content_serial: u64,
    /// The compositor's own handle on each live buffer's memory. Anything
    /// reading a buffer holds a clone, so a count of one means nobody is.
    pub buffer_guards: HashMap<ClientObjectId, Arc<BufferGuard>>,
    /// Buffers the compositor has finished with, waiting on their readers
    /// before `wl_buffer.release` can be sent.
    pub releasing_buffers: HashSet<ClientObjectId>,
    /// Content clients are offering, keyed by the source object — of whichever
    /// of the three interfaces, which [`DataSource::interface`] records.
    pub data_sources: HashMap<ClientObjectId, DataSource>,
    /// Offers the compositor has handed out, keyed by the receiving client and
    /// the id allocated in that client's half of the compositor's id space.
    pub data_offers: HashMap<ClientObjectId, DataOffer>,
    /// Every live `wl_data_device`.
    pub data_devices: Vec<DataDeviceBinding>,
    /// The source that owns the clipboard, if any.
    ///
    /// Names the source rather than copying its mime types: there is one
    /// clipboard, and a second copy of the list could only drift from the
    /// source that owns it.
    pub selection: Option<ClientObjectId>,
    /// Every live `zwp_primary_selection_device_v1`.
    pub primary_devices: Vec<PrimaryDeviceBinding>,
    /// Every live `zwlr_data_control_device_v1`.
    pub data_control_devices: Vec<DataControlDeviceBinding>,
    /// Every live `wp_fractional_scale_v1`, keyed by the object itself.
    pub fractional_scales: HashMap<ClientObjectId, FractionalScaleBinding>,
    /// Cursor images loaded from the theme for `wp_cursor_shape_v1`, keyed by
    /// the list of xcursor names that found them.
    ///
    /// Read once each and kept: a theme lookup is file IO, and the alternative
    /// is doing it while composing a frame. Keyed by the whole candidate list
    /// rather than the name that matched, because that list is what a caller
    /// has — and what it means is "the cursor for this shape", which is the
    /// question being cached.
    pub cursor_shape_images: HashMap<&'static [&'static str], DefaultCursor>,
    /// Which shape each client has asked for, as the names to look it up by.
    ///
    /// Beside `cursor_surfaces` rather than inside it: naming a shape and
    /// attaching a surface are two answers to one question, so setting either
    /// clears the other and the cursor is whichever was set last.
    pub cursor_shapes: HashMap<u32, &'static [&'static str]>,
    /// The source that owns the primary selection, if any.
    ///
    /// Separate from [`Self::selection`] rather than a second role on it: the
    /// two are set by different gestures, read through different objects, and
    /// neither disturbs the other.
    pub primary_selection: Option<ClientObjectId>,
    /// The drag in progress, if any. One, because there is one pointer.
    pub drag: Option<Drag>,
    /// Surfaces with the drag-icon role. Permanent, like the cursor role.
    pub dnd_icon_surfaces: HashSet<ClientObjectId>,
    /// Whether a continuous scroll is in progress on each axis.
    ///
    /// Only the axes that actually moved are stopped when the fingers lift: an
    /// `axis_stop` for an axis that never scrolled describes something that
    /// never happened.
    pub scrolling_vertical: bool,
    pub scrolling_horizontal: bool,
    /// Outputs currently showing the visual bell, and when each stops.
    ///
    /// There is no audio anywhere in this compositor, so `xdg_system_bell.ring`
    /// is answered the way a terminal answers it with the speaker muted: a
    /// brief flash of the display the surface is on.
    pub bell_until: HashMap<OutputId, Instant>,
    /// The last few serials of input events delivered to each client.
    ///
    /// A client quoting a serial is saying "the user did this in my window",
    /// and the serial it quotes is the one on the event it is handling — which
    /// is not necessarily the newest, because a client may read a batch of
    /// events and act on the first of them. Keeping only the newest refuses a
    /// client that did nothing wrong; keeping a short history costs a few words
    /// per client and does not.
    ///
    /// Deliberately separate from [`Self::last_button_serial`], which is a
    /// stricter rule for a different purpose: an interactive move must follow
    /// the press that is *currently held*, and loosening that to serve the
    /// clipboard would weaken move and resize by a side effect.
    pub recent_input_serials: HashMap<u32, VecDeque<u32>>,
}

impl CompositorState {
    /// A state driven by a real shell, with the mechanism's settings applied
    /// — the keymap the settings ask for is compiled here, before any client
    /// can bind a keyboard and be handed the default one.
    pub fn with_shell_and_settings(settings: Settings, shell: Box<dyn Shell>) -> Self {
        let mut state = Self::new();
        state.keyboard = Keyboard::new(settings.xkb_options.as_deref());
        state.settings = settings;
        state.shell = shell;
        state
    }

    /// Run a shell hook: detach the shell, hand it the state, put it back.
    ///
    /// While the closure runs, `self.shell` is a [`NoShell`] — the price of
    /// the shell living inside the state it needs mutable access to. A hook
    /// must therefore answer policy questions from its own fields rather
    /// than through anything that consults `state.shell`; the contract is
    /// spelled out on [`crate::shell`].
    pub fn with_shell<R>(&mut self, f: impl FnOnce(&mut dyn Shell, &mut Self) -> R) -> R {
        let mut shell = std::mem::replace(&mut self.shell, Box::new(NoShell));
        let result = f(shell.as_mut(), self);
        self.shell = shell;
        result
    }

    /// A state with no policy behind it: [`NoShell`] answers every question
    /// with nothing. What tests that never map a window want, and the base
    /// [`Self::with_shell_and_settings`] builds on.
    pub fn new() -> Self {
        Self {
            settings: Settings::default(),
            shell: Box::new(NoShell),
            clients: Clients::new(),
            shm_pools: HashMap::new(),
            buffers: HashMap::new(),
            surfaces: HashMap::new(),
            regions: HashMap::new(),
            xdg_surfaces: HashMap::new(),
            xdg_toplevels: HashMap::new(),
            xdg_popups: HashMap::new(),
            xdg_positioners: HashMap::new(),
            seat: SeatState::default(),
            outputs: Vec::new(),
            output_global_names: HashMap::new(),
            output_bindings: HashMap::new(),
            next_global_number: u32::try_from(crate::protocol::GLOBALS.len()).unwrap_or(1),
            pointers: Vec::new(),
            keyboards: Vec::new(),
            touches: Vec::new(),
            touch_points: HashMap::new(),
            focused_surface: None,
            pointer_surface: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            pressed_keys: HashSet::new(),
            modifiers: ModifierState::default(),
            // Rebuilt from `xkb_options` once the config lands on the state;
            // see `run_compositor`.
            keyboard: Keyboard::new(None),
            // Built on the first publish, so the theme cursor shows before the
            // pointer has moved.
            cursor_dirty: true,
            subsurface_map: HashMap::new(),
            viewports: HashMap::new(),
            surface_viewport: HashMap::new(),
            layer_surfaces: HashMap::new(),
            surface_layer: HashMap::new(),
            decorations: HashMap::new(),
            toplevel_decoration: HashMap::new(),
            buffers_pending_release: Vec::new(),
            dmabuf_formats: Vec::new(),
            dmabuf_global_name: None,
            dmabuf_params: HashMap::new(),
            pending_dmabuf_imports: HashMap::new(),
            next_import_token: 0,
            backend_sender: None,
            dirty: true,
            exit_requested: false,
            cursor_surfaces: HashMap::new(),
            pointer_enter_serial: HashMap::new(),
            cursor_role_surfaces: HashSet::new(),
            default_cursor: None,
            grabbed_popups: Vec::new(),
            pointer_grab: None,
            resize_anchor: None,
            pressed_buttons: HashSet::new(),
            last_button_serial: HashMap::new(),
            next_content_serial: 0,
            buffer_guards: HashMap::new(),
            releasing_buffers: HashSet::new(),
            data_sources: HashMap::new(),
            data_offers: HashMap::new(),
            data_devices: Vec::new(),
            selection: None,
            primary_devices: Vec::new(),
            data_control_devices: Vec::new(),
            fractional_scales: HashMap::new(),
            cursor_shape_images: HashMap::new(),
            cursor_shapes: HashMap::new(),
            primary_selection: None,
            drag: None,
            dnd_icon_surfaces: HashSet::new(),
            recent_input_serials: HashMap::new(),
            scrolling_vertical: false,
            scrolling_horizontal: false,
            bell_until: HashMap::new(),
        }
    }

    /// Flash an output, for [`crate::protocol::xdg_system_bell`].
    pub fn ring_bell(&mut self, output_id: OutputId) {
        self.bell_until
            .insert(output_id, Instant::now() + BELL_DURATION);
        self.dirty = true;
    }

    /// Drop bells that have finished. Returns true if anything changed, which
    /// is what puts the flash back off the screen.
    pub fn expire_bells(&mut self) -> bool {
        let now = Instant::now();
        let before = self.bell_until.len();
        self.bell_until.retain(|_, &mut until| until > now);
        let changed = self.bell_until.len() != before;
        if changed {
            self.dirty = true;
        }
        changed
    }

    /// Note a serial the compositor has just sent a client on an input event.
    ///
    /// Only input events are recorded. A serial from a configure or a frame
    /// callback is not evidence the user did anything, and honouring one would
    /// let a client take the clipboard whenever it liked.
    pub fn record_input_serial(&mut self, client_id: u32, serial: u32) {
        let serials = self.recent_input_serials.entry(client_id).or_default();
        serials.push_back(serial);
        while serials.len() > RECENT_INPUT_SERIALS {
            serials.pop_front();
        }
    }

    /// Whether a serial a client quoted is one it was recently given.
    pub fn is_recent_input_serial(&self, client_id: u32, serial: u32) -> bool {
        self.recent_input_serials
            .get(&client_id)
            .is_some_and(|serials| serials.contains(&serial))
    }

    /// Load the theme cursor for a shape, if it is not loaded already.
    ///
    /// Returns whether there is an image for it now — false means the theme has
    /// no cursor under any of the names, which leaves the caller to keep
    /// whatever cursor was showing.
    pub fn ensure_cursor_shape(&mut self, names: &'static [&'static str]) -> bool {
        if self.cursor_shape_images.contains_key(&names) {
            return true;
        }
        let Some(cursor) = crate::scene::load_theme_cursor(names) else {
            return false;
        };
        self.cursor_shape_images.insert(names, cursor);
        true
    }

    /// The scale this surface should draw itself at, for
    /// `wp_fractional_scale_v1`.
    ///
    /// The highest scale among the outputs it is actually on. A surface
    /// straddling a 1× and a 2× display has to be drawn at 2× for one of them,
    /// and a buffer larger than needed scales down without artefacts while one
    /// too small cannot be scaled up at all.
    ///
    /// A surface on no output — which is every surface before its first commit,
    /// and that is exactly when a client asks — falls back to the output a
    /// window would open on. That is a guess, but it is the right guess far more
    /// often than 1× is, and being wrong costs one re-render rather than a
    /// blurry first frame that stays.
    pub fn preferred_scale(&self, surface: ClientObjectId) -> Scale {
        let on_outputs = self
            .surfaces
            .get(&surface)
            .into_iter()
            .flat_map(|s| s.visible_on.iter())
            .filter_map(|id| self.outputs.iter().find(|o| o.id == *id))
            .map(|o| o.scale)
            .max_by_key(|scale| scale.as_120ths());

        on_outputs
            .or_else(|| {
                self.output_for_new_window()
                    .and_then(|id| self.outputs.iter().find(|o| o.id == id))
                    .map(|o| o.scale)
            })
            .unwrap_or_default()
    }

    /// Returns false if the pool could not be mapped, which is a client error:
    /// the file is unreadable or smaller than the pool it declared.
    pub fn register_shm_pool(
        &mut self,
        client_id: u32,
        pool_id: u32,
        fd: RawFd,
        size: u32,
    ) -> bool {
        // An id here is usually a *dead* pool: one the client destroyed while
        // buffers still referenced it, so it stayed in the map, and whose id
        // `unregister` then announced as free. Reusing it is then perfectly
        // correct and clients really do — a real desktop shell hits this
        // within seconds of starting, which is how the warning that used to be
        // here was found calling it a protocol error.
        //
        // A *live* pool being displaced would be one, but
        // `wl_shm::handle_create_pool` rejects that before it reaches here.
        // Guarded anyway, because a plain insert would drop the displaced pool
        // without unmapping or closing it.
        if let Some(old) = self.shm_pools.remove(&(client_id, pool_id)) {
            if old.dead {
                tracing::debug!("wl_shm pool {pool_id} reused after its buffers outlived it");
            } else {
                tracing::warn!(
                    "wl_shm pool {} re-registered for client {} while still live",
                    pool_id,
                    client_id,
                );
            }
            // Dropping the pool drops its `Arc`; the mapping itself survives
            // until whatever is still reading it lets go.
            drop(old.mapping);
            unsafe { libc::close(old.fd) };
        }

        let mapping = PoolMapping::new(fd, size).map(Arc::new);
        let mapped = mapping.is_some();
        self.shm_pools.insert(
            (client_id, pool_id),
            ShmPool {
                client_id,
                fd,
                size,
                mapping,
                dead: false,
            },
        );
        mapped
    }

    pub fn destroy_shm_pool(&mut self, client_id: u32, pool_id: u32) {
        if let Some(pool) = self.shm_pools.get_mut(&(client_id, pool_id)) {
            pool.dead = true;
        }
        self.try_cleanup_pool(client_id, pool_id);
    }

    /// Returns false if the resized pool could not be mapped — see
    /// [`Self::register_shm_pool`].
    pub fn resize_shm_pool(&mut self, client_id: u32, pool_id: u32, new_size: u32) -> bool {
        self.mark_pool_damaged(client_id, pool_id);
        let Some(pool) = self.shm_pools.get_mut(&(client_id, pool_id)) else {
            return false;
        };
        pool.size = new_size;
        // The old mapping is replaced, not unmapped: a frame already in flight
        // may still be reading through it, and it will be unmapped when that
        // reference goes.
        pool.mapping = PoolMapping::new(pool.fd, new_size).map(Arc::new);
        pool.mapping.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_buffer(
        &mut self,
        client_id: u32,
        buffer_id: u32,
        pool_id: u32,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) {
        let content_serial = self.next_content_serial();
        // A re-registered id is a fresh buffer: forget any release still owed
        // on the old one, which the client has replaced rather than waited for.
        self.releasing_buffers.remove(&(client_id, buffer_id));
        if let Some(mapping) = self
            .shm_pools
            .get(&(client_id, pool_id))
            .and_then(|pool| pool.mapping.clone())
        {
            self.buffer_guards
                .insert((client_id, buffer_id), Arc::new(BufferGuard::new(mapping)));
        }
        self.buffers.insert(
            (client_id, buffer_id),
            Buffer {
                client_id,
                width,
                height,
                content_serial,
                kind: BufferKind::Shm(ShmBuffer {
                    pool_id,
                    offset,
                    stride,
                    format,
                    // A buffer nobody has read yet is wholly new.
                    damage: None,
                }),
            },
        );
    }

    pub fn destroy_buffer(&mut self, client_id: u32, buffer_id: u32) {
        let pool_id = self
            .buffers
            .get(&(client_id, buffer_id))
            .and_then(|b| b.shm().map(|shm| shm.pool_id));
        self.buffers.remove(&(client_id, buffer_id));
        // The client has destroyed the buffer, so it is not waiting to hear
        // that it may reuse it — and it must never hear so: the id is the
        // client's to reuse the moment the destroy is sent, so a release
        // escaping any of these queues afterwards arrives at whatever object
        // holds the id now. libwayland treats an event on an object of the
        // wrong interface as fatal, which is how a leak here shows up as a
        // client "crashing right after showing" (dolphin, resizing, found
        // this: its old buffer id had become a wl_shm_pool).
        self.buffer_guards.remove(&(client_id, buffer_id));
        self.releasing_buffers.remove(&(client_id, buffer_id));
        self.buffers_pending_release
            .retain(|key| key != &(client_id, buffer_id));
        if let Some(pool_id) = pool_id {
            self.try_cleanup_pool(client_id, pool_id);
        }
    }

    /// Place a popup from a positioner, in coordinates relative to its parent.
    ///
    /// The constraining region is the output the parent window is on, expressed
    /// relative to the parent surface — which is what turns "keep it on screen"
    /// into arithmetic the positioner can do without knowing where anything is
    /// globally. A client that named a `parent_size` is constrained against
    /// that instead: it is telling us the size its window is about to become,
    /// and placing against the size it is leaving would be placing for the past.
    pub fn place_popup(
        &self,
        positioner: ClientObjectId,
        parent_surface: ClientObjectId,
    ) -> Option<(i32, i32, i32, i32)> {
        let pos = self.xdg_positioners.get(&positioner)?;

        let available = if let Some((width, height)) = pos.parent_size {
            Some((0, 0, width, height))
        } else {
            self.surface_output(self.root_of(parent_surface))
                .and_then(|id| self.outputs.iter().find(|o| o.id == id))
                .map(|output| {
                    let parent = self.global_position_of(parent_surface);
                    let (width, height) = output.logical_size();
                    (
                        output.geometry.x - parent.0,
                        output.geometry.y - parent.1,
                        width,
                        height,
                    )
                })
        };

        Some(crate::protocol::xdg_positioner::place(pos, available))
    }

    /// The `xdg_surface` wrapping a `wl_surface`, if it has one.
    pub fn xdg_surface_of(&self, surface: ClientObjectId) -> Option<ClientObjectId> {
        self.xdg_surfaces
            .iter()
            .find(|((client_id, _), xdg)| *client_id == surface.0 && xdg.wl_surface_id == surface.1)
            .map(|(&key, _)| key)
    }

    /// Note a configure serial as sent, so the acknowledgement can be checked
    /// against it.
    ///
    /// Every `xdg_surface.configure` goes through here — see
    /// [`crate::protocol::xdg_surface::send_configure`], which is the
    /// only thing that allocates one. A configure sent without being recorded
    /// is one the client cannot legally acknowledge.
    pub fn record_configure(&mut self, xdg_surface: ClientObjectId, serial: u32) {
        let Some(xdg) = self.xdg_surfaces.get_mut(&xdg_surface) else {
            return;
        };
        xdg.pending_configures.push_back(serial);
        xdg.highest_configure = serial;
        while xdg.pending_configures.len() > MAX_PENDING_CONFIGURES {
            xdg.pending_configures.pop_front();
        }
    }

    /// Whether a surface already has a role.
    ///
    /// A `wl_surface` gets one role and keeps it for life, and the protocol
    /// makes a second one an error on whichever request asked for it. Gathered
    /// in one place because the roles live in five different collections, and
    /// a check that forgot one would let two subsystems each believe they
    /// place the same surface.
    pub fn has_role(&self, surface: ClientObjectId) -> bool {
        self.cursor_role_surfaces.contains(&surface)
            || self.dnd_icon_surfaces.contains(&surface)
            || self.surface_layer.contains_key(&surface)
            || self.xdg_surface_of(surface).is_some()
            || self.surfaces.get(&surface).is_some_and(|s| s.is_subsurface)
    }

    /// The layer surface giving a `wl_surface` its role, if one does.
    pub fn layer_surface_of(&self, surface: ClientObjectId) -> Option<ClientObjectId> {
        self.surface_layer
            .get(&surface)
            .map(|&layer_id| (surface.0, layer_id))
    }

    /// Whether a surface's commits are cached rather than applied.
    ///
    /// A subsurface is *effectively* synchronised if it is synchronised itself
    /// or if the subsurface above it is — the mode is inherited down the tree,
    /// so desyncing a child of a synchronised parent does not free it. That is
    /// the protocol's wording and it is what makes a whole subtree update as
    /// one piece: a client sets its root subsurface synchronised and everything
    /// beneath it lands in the same frame.
    ///
    /// Only subsurfaces. A toplevel or a popup has no commit mode, and its
    /// commits always apply.
    ///
    /// Bounded by the number of surfaces rather than trusting the chain to end,
    /// the same way [`Self::root_of`] is.
    pub fn is_effectively_synced(&self, key: ClientObjectId) -> bool {
        let (client_id, mut current) = key;
        for _ in 0..=self.surfaces.len() {
            let Some(surface) = self.surfaces.get(&(client_id, current)) else {
                return false;
            };
            // The chain stops at the first thing that is not a subsurface: a
            // toplevel's commit mode is not a thing, so it cannot make its
            // children synchronised.
            if !surface.is_subsurface {
                return false;
            }
            if surface.subsurface_sync {
                return true;
            }
            let Some(parent) = surface.parent else {
                return false;
            };
            current = parent;
        }
        false
    }

    /// Every synchronised subsurface directly under a surface.
    pub fn synced_children(&self, key: ClientObjectId) -> Vec<ClientObjectId> {
        self.surfaces
            .get(&key)
            .map(|surface| {
                surface
                    .children
                    .iter()
                    .map(|&child| (key.0, child))
                    .filter(|&child| self.is_effectively_synced(child))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `surface_id` is `candidate_id` or one of its ancestors.
    ///
    /// Asked before any request links one surface under another, because a
    /// surface that ends up its own ancestor is not a wrong answer but a hang
    /// or a blown stack: the tree is recursed to compose and to hit-test, and
    /// walked to find a surface's global position. Two requests is all it
    /// costs a client to build a cycle, so both of the requests that can
    /// create one — `wl_subcompositor.get_subsurface` and
    /// `xdg_surface.get_popup` — ask here rather than leaving it to the
    /// walkers.
    ///
    /// Bounded by the number of surfaces rather than trusting the chain to
    /// end, the same way [`Self::root_of`] is. This is the check that keeps
    /// the tree acyclic, so it must not be the one thing that a cycle could
    /// hang.
    pub fn is_ancestor(&self, client_id: u32, surface_id: u32, candidate_id: u32) -> bool {
        let mut current = candidate_id;
        for _ in 0..=self.surfaces.len() {
            if current == surface_id {
                return true;
            }
            let Some(parent) = self
                .surfaces
                .get(&(client_id, current))
                .and_then(|s| s.parent)
            else {
                return false;
            };
            current = parent;
        }
        // Only reachable if a cycle already exists, which this is meant to
        // prevent. Refusing the link is the safe answer either way.
        true
    }

    /// The surface at the root of a subsurface or popup tree.
    fn root_of(&self, key: ClientObjectId) -> ClientObjectId {
        let (client_id, mut current) = key;
        for _ in 0..self.surfaces.len() {
            let Some(parent) = self
                .surfaces
                .get(&(client_id, current))
                .and_then(|s| s.parent)
            else {
                break;
            };
            current = parent;
        }
        (client_id, current)
    }

    /// Where a surface sits in global coordinates, walking up its parents.
    fn global_position_of(&self, key: ClientObjectId) -> (i32, i32) {
        let (client_id, mut current) = key;
        let (mut x, mut y) = (0i32, 0i32);
        for _ in 0..=self.surfaces.len() {
            let Some(surface) = self.surfaces.get(&(client_id, current)) else {
                break;
            };
            x = x.saturating_add(surface.subsurface_position.0);
            y = y.saturating_add(surface.subsurface_position.1);
            if let Some(parent) = surface.parent {
                current = parent;
            } else {
                // The root of the tree carries the only global position; every
                // surface below it is an offset from its parent.
                x = x.saturating_add(surface.position.0);
                y = y.saturating_add(surface.position.1);
                break;
            }
        }
        (x, y)
    }

    /// Raise a window, bringing its dialogs up with it.
    ///
    /// A child is kept above its parent, so raising either has to move both —
    /// otherwise clicking a window would bury the dialog that belongs to it,
    /// which is exactly the case `set_parent` exists to prevent.
    ///
    /// The parent goes up first and the children follow, deepest last, so a
    /// chain of dialogs keeps its own order.
    pub fn raise_with_children(&mut self, toplevel: ClientObjectId) {
        for key in self.toplevel_family(toplevel) {
            if let Some(surface) = self.surface_of_toplevel(key) {
                self.shell.raise(surface);
            }
        }
        self.dirty = true;
    }

    /// A toplevel's whole family — the root of its parent chain first, then
    /// every descendant, nearest first. This is the set a raise lifts
    /// together, public so a shell can walk it while it is detached from the
    /// state and `raise_with_children` (which goes through `self.shell`)
    /// would reach nothing.
    pub fn toplevel_family(&self, toplevel: ClientObjectId) -> Vec<ClientObjectId> {
        // Start from the root of the family, so raising a dialog lifts the
        // whole group rather than tearing it off the top of its parent.
        let mut root = toplevel;
        for _ in 0..self.xdg_toplevels.len() {
            let Some(parent) = self
                .xdg_toplevels
                .get(&root)
                .and_then(|t| t.parent)
                .map(|id| (root.0, id))
            else {
                break;
            };
            if !self.xdg_toplevels.contains_key(&parent) {
                break;
            }
            root = parent;
        }

        std::iter::once(root)
            .chain(self.descendants_of(root))
            .collect()
    }

    /// Every toplevel below one in the parent chain, nearest first.
    ///
    /// Breadth-first, and bounded by the number of toplevels that exist:
    /// `set_parent` refuses a link that would close a loop, but this must not
    /// depend on that for its own termination.
    fn descendants_of(&self, root: ClientObjectId) -> Vec<ClientObjectId> {
        let mut found = Vec::new();
        let mut frontier = vec![root];
        for _ in 0..self.xdg_toplevels.len() {
            let mut next = Vec::new();
            for (&key, toplevel) in &self.xdg_toplevels {
                if key.0 != root.0 || found.contains(&key) || key == root {
                    continue;
                }
                let parent = toplevel.parent.map(|id| (key.0, id));
                if parent.is_some_and(|p| frontier.contains(&p)) {
                    next.push(key);
                }
            }
            if next.is_empty() {
                break;
            }
            found.extend(next.iter().copied());
            frontier = next;
        }
        found
    }

    /// Put a window into or out of maximized or fullscreen, and ask it to
    /// adopt the size that goes with it.
    ///
    /// The compositor owns the position and the client owns the size, so this
    /// moves the window itself and *asks* for the size through a configure —
    /// the window does not actually change shape until the client acknowledges
    /// and commits a buffer at the new size.
    ///
    /// Returns false if there is nothing to do or nowhere to do it: a window on
    /// no output has no size to fill.
    pub fn set_window_state(
        &mut self,
        toplevel: ClientObjectId,
        maximized: bool,
        fullscreen: bool,
    ) -> bool {
        let Some(current) = self.xdg_toplevels.get(&toplevel) else {
            return false;
        };
        if current.maximized == maximized && current.fullscreen == fullscreen {
            return false;
        }
        let was_normal = !current.maximized && !current.fullscreen;
        let Some(surface) = self.surface_of_toplevel(toplevel) else {
            return false;
        };

        // An interactive drag and a window state cannot both own the geometry.
        // The drag is the one the user is holding, so the state change wins and
        // the drag is dropped rather than left writing positions underneath it.
        if self
            .pointer_grab
            .is_some_and(|grab| grab.toplevel == toplevel)
        {
            self.pointer_grab = None;
        }

        // Fullscreen wins where both are set: it is the larger claim, and a
        // window covering its output is already filling it.
        if maximized || fullscreen {
            let Some(output) = self
                .surface_output(surface)
                .and_then(|id| self.outputs.iter().find(|o| o.id == id))
            else {
                return false;
            };
            // The area a panel has *not* reserved. A maximised window that
            // covered the bar would leave the user unable to reach either.
            //
            // Fullscreen is the exception the protocol makes: covering the
            // output entirely is the whole request, so it takes the lot.
            let output_id = output.id;
            let (origin, size) = if fullscreen {
                (
                    (output.geometry.x, output.geometry.y),
                    output.logical_size(),
                )
            } else {
                let usable = crate::layer::usable_area(self, output_id);
                (
                    (output.geometry.x + usable.x, output.geometry.y + usable.y),
                    (usable.width, usable.height),
                )
            };

            // Captured only on the way in from a normal window. Going
            // maximized then fullscreen must still return to where the window
            // started, and a second capture would record the maximized
            // geometry instead.
            if was_normal {
                let position = self.surfaces.get(&surface).map_or((0, 0), |s| s.position);
                let current_size = self.surface_size(surface);
                if let Some(toplevel) = self.xdg_toplevels.get_mut(&toplevel) {
                    toplevel.restore =
                        Some((position.0, position.1, current_size.0, current_size.1));
                }
            }
            if let Some(s) = self.surfaces.get_mut(&surface) {
                s.position = origin;
            }
            if let Some(t) = self.xdg_toplevels.get_mut(&toplevel) {
                t.maximized = maximized;
                t.fullscreen = fullscreen;
            }
            // A fullscreen window that something else is drawn over is not
            // fullscreen. Maximized windows are ordinary windows that happen to
            // fill the screen, and are left where they are in the stack.
            if fullscreen {
                self.raise_with_children(toplevel);
            }
            crate::protocol::xdg_toplevel::configure(self, toplevel, size.0, size.1);
        } else {
            let restore = self.xdg_toplevels.get_mut(&toplevel).and_then(|t| {
                t.maximized = false;
                t.fullscreen = false;
                t.restore.take()
            });
            // A window that was mapped straight into a maximized state has no
            // geometry to go back to. Zero tells the client to pick its own
            // size, which is what it was told on its very first configure.
            let (position, size) =
                restore.map_or(((0, 0), (0, 0)), |(x, y, w, h)| ((x, y), (w, h)));
            if restore.is_some()
                && let Some(s) = self.surfaces.get_mut(&surface)
            {
                s.position = position;
            }
            crate::protocol::xdg_toplevel::configure(self, toplevel, size.0, size.1);
        }
        self.dirty = true;
        true
    }

    /// The `wl_surface` a toplevel draws into.
    pub fn surface_of_toplevel(&self, toplevel: ClientObjectId) -> Option<ClientObjectId> {
        let xdg_surface_id = self.xdg_toplevels.get(&toplevel)?.xdg_surface_id;
        let xdg = self.xdg_surfaces.get(&(toplevel.0, xdg_surface_id))?;
        Some((toplevel.0, xdg.wl_surface_id))
    }

    /// Whether a window is filling or covering its output, and so should not be
    /// dragged, resized, or confined as an ordinary window.
    pub fn is_window_state_locked(&self, surface: ClientObjectId) -> bool {
        self.toplevel_for_surface(surface)
            .and_then(|key| self.xdg_toplevels.get(&key))
            .is_some_and(|t| t.maximized || t.fullscreen)
    }

    /// Begin an interactive move of a window.
    ///
    /// Sends the client a pointer leave first: the compositor owns the pointer for
    /// the duration, and a client left believing the pointer is still inside it
    /// would keep drawing hover states it can no longer see the end of.
    pub fn start_move_grab(&mut self, surface: ClientObjectId) {
        // A window filling its output has no room to be dragged into, and a
        // drag would only fight the state that put it there.
        if self.drag.is_some() || self.is_window_state_locked(surface) {
            return;
        }
        // A lingering resize anchor would reposition the window on its next
        // commit, fighting the drag over who owns the origin.
        self.resize_anchor = None;
        let Some(toplevel) = self.toplevel_for_surface(surface) else {
            return;
        };
        let Some(position) = self.surfaces.get(&surface).map(|s| s.position) else {
            return;
        };
        self.release_pointer_to_grab();
        self.pointer_grab = Some(PointerGrab {
            surface,
            toplevel,
            kind: GrabKind::Move {
                offset_x: position.0 - f64_to_i32(self.cursor_x),
                offset_y: position.1 - f64_to_i32(self.cursor_y),
            },
        });
    }

    /// Begin an interactive resize of a window's edge or corner.
    pub fn start_resize_grab(&mut self, surface: ClientObjectId, edges: ResizeEdges) {
        if self.drag.is_some() || self.is_window_state_locked(surface) {
            return;
        }
        let Some(toplevel) = self.toplevel_for_surface(surface) else {
            return;
        };
        let Some(position) = self.surfaces.get(&surface).map(|s| s.position) else {
            return;
        };
        let size = self.surface_size(surface);
        if size.0 <= 0 || size.1 <= 0 {
            return;
        }
        self.release_pointer_to_grab();
        self.pointer_grab = Some(PointerGrab {
            surface,
            toplevel,
            kind: GrabKind::Resize {
                edges,
                start_pointer: (self.cursor_x, self.cursor_y),
                start_size: size,
                last_sent: size,
            },
        });
        // Only a drag moving the left or top edge ever repositions; for the
        // others the origin is itself the anchor and needs no help.
        self.resize_anchor = (edges.left() || edges.top()).then_some(ResizeAnchor {
            surface,
            edges,
            right: position.0 + size.0,
            bottom: position.1 + size.1,
        });
    }

    /// Re-derive a resizing window's position from the size it actually
    /// committed, keeping the grabbed resize's fixed edges fixed.
    ///
    /// Called as a commit's state applies. Doing this per commit rather than
    /// per pointer motion is what keeps a left- or top-edge resize steady: a
    /// position moved at motion time gets composed against whatever old-size
    /// buffer the client last committed, and the anchored edge wobbles by
    /// the lag on every frame — violent flicker for the whole drag.
    pub fn apply_resize_anchor(&mut self, key: ClientObjectId) {
        let Some(anchor) = self.resize_anchor else {
            return;
        };
        if anchor.surface != key {
            return;
        }
        let (width, height) = self.surface_size(key);
        if width <= 0 || height <= 0 {
            return;
        }
        if let Some(surface) = self.surfaces.get_mut(&key) {
            if anchor.edges.left() {
                surface.position.0 = anchor.right - width;
            }
            if anchor.edges.top() {
                surface.position.1 = anchor.bottom - height;
            }
        }
        // The anchor lives as long as the grab does, plus the one commit
        // that lands after release — a client mid-redraw when the button
        // came up still gets its final size placed correctly, but its own
        // later resizes are its own business.
        if self.pointer_grab.is_none() {
            self.resize_anchor = None;
        }
    }

    /// Put a source on the clipboard, or clear it.
    ///
    /// The source that held it is cancelled, and every offer drawing from it
    /// goes dead — a client still holding a paste offer for the old clipboard
    /// gets an empty read rather than the wrong contents.
    pub fn set_selection(&mut self, source: Option<ClientObjectId>) {
        if let Some(old) = self.selection
            && Some(old) != source
        {
            cancel_source(self, old);
            self.invalidate_offers_from(old);
        }
        self.selection = source;
        self.announce_selection();
    }

    /// Take a source off the clipboard without cancelling it.
    ///
    /// Used where the source is going away on its own account — destroyed, or
    /// its client disconnecting — and telling it would be telling nobody.
    fn clear_selection(&mut self) {
        self.selection = None;
        self.announce_selection();
    }

    /// Tell everyone entitled to know what the clipboard now is.
    ///
    /// Two audiences with different rules. The focused client is told because
    /// the clipboard follows focus; a data-control client is told whatever has
    /// focus, because it is outside the focus model altogether — that is what
    /// it binds the interface for, and a clipboard manager that only heard
    /// about selections while it had focus would never hear about one at all.
    fn announce_selection(&mut self) {
        if let Some(client_id) = self.focused_surface.map(|(client_id, _)| client_id) {
            wl_data_device::send_selection_to_client(self, client_id);
        }
        zwlr_data_control_device::send_selection_to_all(self);
    }

    /// Cut every offer drawing from a source loose from it.
    ///
    /// The offers are not destroyed. Their clients still own the ids and will
    /// still send requests on them, and a server id is never announced as free
    /// — so the compositor forgets what an offer contains and keeps the fact
    /// that it exists. A read from one then closes the pipe, which the client
    /// sees as an empty transfer.
    fn invalidate_offers_from(&mut self, source: ClientObjectId) {
        for offer in self.data_offers.values_mut() {
            if offer.source == Some(source) {
                offer.source = None;
                offer.accepted = None;
            }
        }
    }

    /// Drop a source, unwinding whatever it was in the middle of.
    ///
    /// `notify` sends `cancelled`, which is right when the compositor is taking
    /// the source away and wrong when the client is destroying it itself.
    pub fn retire_data_source(&mut self, source: ClientObjectId, notify: bool) {
        if self.selection == Some(source) {
            self.clear_selection();
        }
        // Either selection, because one map holds the sources of all three
        // interfaces and a data-control source can be put on either — so a
        // source being retired has to be looked for in both places rather than
        // in the one its own interface suggests.
        if self.primary_selection == Some(source) {
            self.clear_primary_selection();
        }
        if self.drag.as_ref().is_some_and(|d| d.source == Some(source)) {
            self.cancel_drag();
        }
        self.invalidate_offers_from(source);
        if notify {
            cancel_source(self, source);
        }
        self.data_sources.remove(&source);
    }

    /// Put a source on the primary selection, or clear it.
    ///
    /// The same shape as [`Self::set_selection`], on the other selection. The
    /// source that held it is cancelled and every offer drawing from it goes
    /// dead, so a client still holding one gets an empty read rather than text
    /// the user selected some time ago.
    pub fn set_primary_selection(&mut self, source: Option<ClientObjectId>) {
        if let Some(old) = self.primary_selection
            && Some(old) != source
        {
            cancel_source(self, old);
            self.invalidate_offers_from(old);
        }
        self.primary_selection = source;
        self.announce_primary_selection();
    }

    /// Take a source off the primary selection without cancelling it.
    ///
    /// Used where the source is going away on its own account — destroyed, or
    /// its client disconnecting — and telling it would be telling nobody.
    fn clear_primary_selection(&mut self) {
        self.primary_selection = None;
        self.announce_primary_selection();
    }

    /// Tell everyone entitled to know what the primary selection now is.
    ///
    /// The focused client, because the primary selection follows focus like the
    /// clipboard, and every data-control client, because those are outside the
    /// focus model entirely — see [`Self::announce_selection`].
    fn announce_primary_selection(&mut self) {
        if let Some(client_id) = self.focused_surface.map(|(client_id, _)| client_id) {
            zwp_primary_selection_device::send_selection_to_client(self, client_id);
        }
        zwlr_data_control_device::send_primary_selection_to_all(self);
    }

    /// Begin a drag, taking the pointer for the duration.
    ///
    /// Sits beside [`Self::start_move_grab`] and [`Self::start_resize_grab`]
    /// because it is the third thing that can own the pointer, and because all
    /// three need `release_pointer_to_grab`, which stays private.
    pub fn start_drag(
        &mut self,
        source: Option<ClientObjectId>,
        origin_client: u32,
        origin: ClientObjectId,
        icon: Option<ClientObjectId>,
    ) {
        // The client the pointer was over is told it has left, for the same
        // reason a move grab tells it: it would otherwise keep drawing a hover
        // state for a pointer it will hear no more about.
        self.release_pointer_to_grab();
        self.drag = Some(Drag {
            source,
            origin_client,
            origin,
            icon,
            focus: None,
            focus_offers: Vec::new(),
        });
        self.dirty = true;
    }

    /// End a drag without a drop, telling both sides.
    pub fn cancel_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if let Some(focus) = drag.focus {
            for device_id in wl_data_device::devices_of(self, focus.0) {
                wl_data_device::send_leave(self, focus.0, device_id);
            }
        }
        for offer in drag.focus_offers {
            self.invalidate_offer(offer);
        }
        if let Some(source) = drag.source {
            cancel_source(self, source);
        }
        self.dirty = true;
    }

    /// Note that an offer has been dropped on, which is what makes
    /// `wl_data_offer.finish` legal on it.
    pub fn mark_offer_dropped(&mut self, offer: ClientObjectId) {
        if let Some(offer) = self.data_offers.get_mut(&offer) {
            offer.kind = OfferKind::Dropped;
        }
    }

    /// Cut one offer loose from its source, leaving the object in place.
    pub fn invalidate_offer(&mut self, offer: ClientObjectId) {
        if let Some(offer) = self.data_offers.get_mut(&offer) {
            offer.source = None;
            offer.accepted = None;
        }
    }

    /// Settle the action for one drag offer, and tell both sides if it changed.
    ///
    /// Run whenever either side's half of the negotiation moves: the target's
    /// `set_actions` or `accept`, and each time the drag enters a surface.
    /// A selection offer has no action to settle — actions are a drag concept.
    pub fn resolve_offer_action(&mut self, offer_key: ClientObjectId) {
        let Some(offer) = self.data_offers.get(&offer_key) else {
            return;
        };
        if offer.kind == OfferKind::Selection {
            return;
        }
        let Some(source_key) = offer.source else {
            return;
        };
        let Some(source) = self.data_sources.get(&source_key) else {
            return;
        };

        let action = resolve_action(
            source.actions,
            self.object_version(source_key),
            offer.actions,
            offer.preferred_action,
            self.object_version(offer_key),
        );

        if offer.resolved_action == action {
            return;
        }
        if let Some(offer) = self.data_offers.get_mut(&offer_key) {
            offer.resolved_action = action;
        }
        wl_data_offer::send_action(self, offer_key, action);
        wl_data_source::send_action(self, source_key, action);
    }

    /// Re-settle every live drag offer.
    ///
    /// Used when the *source's* half of the negotiation moves, which it can do
    /// without naming any particular offer.
    pub fn resolve_drag_actions(&mut self) {
        let keys: Vec<ClientObjectId> = self
            .data_offers
            .iter()
            .filter(|(_, o)| o.kind != OfferKind::Selection && o.source.is_some())
            .map(|(&key, _)| key)
            .collect();
        for key in keys {
            self.resolve_offer_action(key);
        }
    }

    /// Whether a drag target has said it will take what is being dragged.
    ///
    /// True only if it named a mime type *and* the two sides settled on an
    /// action — a target that accepted the content but agreed to do nothing
    /// with it has not accepted the drop.
    pub fn drag_target_accepted(&self) -> bool {
        self.drag.as_ref().is_some_and(|drag| {
            drag.focus_offers.iter().any(|key| {
                self.data_offers
                    .get(key)
                    .is_some_and(|o| o.accepted.is_some() && o.resolved_action != DND_ACTION_NONE)
            })
        })
    }

    /// The interface version an object was bound at, or 1 if its client is gone.
    fn object_version(&self, key: ClientObjectId) -> u32 {
        self.clients.version_of(key.0, key.1).unwrap_or(1)
    }

    /// Take the pointer away from whichever client currently has it.
    fn release_pointer_to_grab(&mut self) {
        let Some(pointer_surface) = self.pointer_surface.take() else {
            return;
        };
        for ptr in self.pointers.clone() {
            if ptr.client_id == pointer_surface.0 {
                wl_pointer::send_leave(self, ptr.client_id, ptr.object_id, pointer_surface.1);
                wl_pointer::send_frame(self, ptr.client_id, ptr.object_id);
            }
        }
    }

    /// The size range a client has said it will accept.
    ///
    /// Returns `((min_w, min_h), (max_w, max_h))`, where a zero means the
    /// client named no limit in that dimension.
    pub fn client_size_limits(&self, surface: ClientObjectId) -> ((i32, i32), (i32, i32)) {
        self.toplevel_for_surface(surface)
            .and_then(|key| self.xdg_toplevels.get(&key))
            .map_or(((0, 0), (0, 0)), |t| (t.min_size, t.max_size))
    }

    /// The `xdg_toplevel` object driving a window, given its `wl_surface`.
    pub fn toplevel_for_surface(&self, key: ClientObjectId) -> Option<ClientObjectId> {
        self.xdg_surfaces
            .iter()
            .find(|((client_id, _), xdg)| *client_id == key.0 && xdg.wl_surface_id == key.1)
            .and_then(|(&(client_id, _), xdg)| match xdg.role {
                Some(XdgRole::Toplevel(id)) => Some((client_id, id)),
                _ => None,
            })
    }

    /// Put the pointer at a position, or as near to it as the outputs allow.
    pub fn move_cursor_to(&mut self, x: f64, y: f64) {
        let (x, y) = self.constrain_to_outputs(x, y);
        self.cursor_x = x;
        self.cursor_y = y;
    }

    /// Move the pointer by a delta.
    ///
    /// What a mouse actually reports: at the libinput layer a mouse has no
    /// position at all, only movement, and the pointer position is the
    /// compositor's to own and to keep somewhere sensible.
    pub fn move_cursor_by(&mut self, dx: f64, dy: f64) {
        self.move_cursor_to(self.cursor_x + dx, self.cursor_y + dy);
    }

    /// Pull a position onto the outputs, if it is not on one already.
    ///
    /// Constraining matters more than it looks. A relative device gives deltas
    /// and nothing else, so an unconstrained pointer walks off the desktop and
    /// never comes back — and the failure is silent rather than loud: the
    /// cursor stops being drawn, hit testing finds nothing, and clicks go
    /// nowhere, with no edge for the user to push against.
    ///
    /// The constraint is the union of the outputs, not their bounding box.
    /// Two outputs of different heights side by side leave a notch belonging to
    /// no output, and a pointer there would be exactly as lost.
    fn constrain_to_outputs(&self, x: f64, y: f64) -> (f64, f64) {
        // Every reader converts this to `i32` unchecked, so a non-finite value
        // must never be stored.
        if !x.is_finite() || !y.is_finite() {
            return (self.cursor_x, self.cursor_y);
        }
        if self.outputs.is_empty() {
            // Nothing to constrain against, but the conversion still has to be
            // safe.
            return (
                x.clamp(f64::from(i32::MIN), f64::from(i32::MAX)),
                y.clamp(f64::from(i32::MIN), f64::from(i32::MAX)),
            );
        }

        let mut nearest: Option<(f64, f64, f64)> = None;
        for output in &self.outputs {
            let Some((x0, y0, x1, y1)) = cursor_bounds(output) else {
                continue;
            };
            if x >= x0 && x <= x1 && y >= y0 && y <= y1 {
                return (x, y);
            }
            // The closest this output can get to where the pointer wanted to be.
            let (cx, cy) = (x.clamp(x0, x1), y.clamp(y0, y1));
            let distance = (cx - x).powi(2) + (cy - y).powi(2);
            if nearest.is_none_or(|(_, _, best)| distance < best) {
                nearest = Some((cx, cy, distance));
            }
        }
        nearest.map_or((self.cursor_x, self.cursor_y), |(x, y, _)| (x, y))
    }

    /// The output a new window should open on: the one under the pointer, or
    /// failing that whichever output comes first.
    pub fn output_for_new_window(&self) -> Option<OutputId> {
        let (x, y) = (f64_to_i32(self.cursor_x), f64_to_i32(self.cursor_y));
        self.outputs
            .iter()
            .find(|o| output_contains(o, x, y))
            .or_else(|| self.outputs.first())
            .map(|o| o.id)
    }

    /// Size of a window in surface-local coordinates.
    ///
    /// Zero until the client has attached a buffer, which is the usual state
    /// when a toplevel is first mapped.
    pub fn surface_size(&self, key: ClientObjectId) -> (i32, i32) {
        self.surface_buffer_mapping(key)
            .map_or((0, 0), |m| (m.dest_width, m.dest_height))
    }

    /// Take an output away, and tell every client it has gone.
    ///
    /// Returns the `wl_registry` name it was advertised under, so the caller
    /// can withdraw the global — the send needs the clients borrowed, which
    /// this method cannot do while it is walking its own state.
    ///
    /// Everything tying a client to the output goes here: the bindings, and the
    /// record of which surfaces had entered it. No `wl_surface.leave` is sent
    /// for it, deliberately. The client is about to destroy its `wl_output` on
    /// hearing `global_remove`, and an event naming an object it has already
    /// let go is at best ignored and at worst a decode error.
    pub fn remove_output(&mut self, output_id: OutputId) -> Option<u32> {
        self.outputs.retain(|output| output.id != output_id);
        self.output_bindings.retain(|_, &mut id| id != output_id);
        for surface in self.surfaces.values_mut() {
            surface.entered_outputs.remove(&output_id);
            surface.visible_on.remove(&output_id);
        }
        // The shell owned windows on that output; tell it so they can be
        // given back and re-homed onto an output that exists.
        self.with_shell(super::super::shell::Shell::outputs_changed);
        self.dirty = true;
        self.output_global_names.remove(&output_id)
    }

    /// Which output a window is on: the one owning the workspace it is in.
    ///
    /// `None` for a window waiting to be placed, and for anything that is not
    /// a toplevel — popups and subsurfaces hang off a toplevel and take its
    /// output along with its position.
    pub fn surface_output(&self, surface_key: ClientObjectId) -> Option<OutputId> {
        self.shell.toplevel_output(surface_key).or_else(|| {
            // A layer surface is not in a workspace — it belongs to an output
            // directly and never moves between them. Without this a popup
            // opened from a panel has no output to be kept on screen against,
            // and slides off the edge instead of flipping.
            self.layer_surface_of(surface_key)
                .and_then(|key| self.layer_surfaces.get(&key))
                .and_then(|layer| layer.output)
        })
    }

    /// The topmost window the user can currently see and click on: the top of
    /// the workspace showing on the output under the pointer, or failing that
    /// of the first output that has a window.
    pub fn top_visible_toplevel(&self) -> Option<ClientObjectId> {
        self.output_for_new_window()
            .and_then(|output_id| self.shell.top_visible(output_id))
            .or_else(|| {
                self.outputs
                    .iter()
                    .find_map(|o| self.shell.top_visible(o.id))
            })
    }

    /// Work out which part of a surface's buffer is shown, and at what size.
    ///
    /// A `wp_viewport` source crops and a viewport destination sizes; failing
    /// either, the whole buffer is shown divided down by the surface's buffer
    /// scale, which is what turns an oversized buffer from a scaled client back
    /// into its logical size on screen.
    ///
    /// Shared by scene building and by damage mapping, which needs to run this
    /// backwards — the two must agree or damage lands in the wrong place.
    pub fn surface_buffer_mapping(&self, key: ClientObjectId) -> Option<BufferMapping> {
        let surface = self.surfaces.get(&key)?;
        let buffer = self.buffers.get(&(key.0, surface.buffer_id?))?;
        if buffer.width <= 0 || buffer.height <= 0 {
            return None;
        }

        let viewport = self
            .surface_viewport
            .get(&key)
            .and_then(|&vp_id| self.viewports.get(&(key.0, vp_id)));

        let src = viewport.and_then(|v| v.source).unwrap_or((
            0.0,
            0.0,
            f64::from(buffer.width),
            f64::from(buffer.height),
        ));
        let scale = surface.buffer_scale.max(1);
        let (dest_width, dest_height) = if let Some((w, h)) = viewport.and_then(|v| v.destination) {
            (w, h)
        } else {
            // A quarter turn exchanges the axes: a surface whose buffer is
            // rotated 90 degrees is as wide as that buffer is tall.
            let (w, h) = (f64_to_i32(src.2) / scale, f64_to_i32(src.3) / scale);
            if surface.buffer_transform.swaps_axes() {
                (h, w)
            } else {
                (w, h)
            }
        };
        if dest_width <= 0 || dest_height <= 0 {
            return None;
        }

        Some(BufferMapping {
            src,
            dest_width,
            dest_height,
        })
    }

    /// Hand out the next content serial.
    pub fn next_content_serial(&mut self) -> u64 {
        self.next_content_serial += 1;
        self.next_content_serial
    }

    /// Record that a buffer's contents have changed, and how much of it.
    ///
    /// An empty `damage` means the whole buffer must be treated as new. Damage
    /// accumulates across changes nobody has read yet: if two commits land
    /// between two scenes, the second must not erase what the first reported.
    ///
    /// Only an upload has anything to record. A dma-buf is sampled where it
    /// lies, so a client drawing into one needs nothing sent and nothing
    /// invalidated — and a new serial would tell the backend to throw away an
    /// import that is still perfectly good, once per committed frame.
    pub fn mark_buffer_damaged(&mut self, client_id: u32, buffer_id: u32, damage: &[TextureRect]) {
        let serial = self.next_content_serial();
        let Some(buffer) = self.buffers.get_mut(&(client_id, buffer_id)) else {
            return;
        };
        let Some(shm) = buffer.shm_mut() else {
            return;
        };
        match (&mut shm.damage, damage.is_empty()) {
            // Widening to "everything" is sticky until the damage is read:
            // once one change could not be described, no later rectangle
            // can narrow the window back down.
            (_, true) | (None, false) => shm.damage = None,
            (Some(existing), false) => existing.extend_from_slice(damage),
        }
        buffer.content_serial = serial;
    }

    /// Whether anything is still reading a buffer's memory.
    ///
    /// True only while some `TextureImage` borrowing it is alive. A buffer with
    /// no guard at all — destroyed, or never mapped — counts as idle.
    pub fn buffer_is_being_read(&self, key: ClientObjectId) -> bool {
        // A dma-buf has no mapping to guard, so the image itself is what the
        // readers hold: state keeps one `Arc` and every in-flight texture
        // clones it. Answering from `buffer_guards` alone would report every
        // dma-buf idle and release it out from under the frame drawing it.
        if let Some(BufferKind::Dmabuf(image)) = self.buffers.get(&key).map(|b| &b.kind) {
            return Arc::strong_count(image) > 1;
        }
        self.buffer_guards
            .get(&key)
            .is_some_and(|guard| Arc::strong_count(guard) > 1)
    }

    /// Forget the damage accumulated so far, having acted on it.
    ///
    /// Resets to "nothing has changed" rather than to "unknown" — the two are
    /// opposites, and clearing to the wrong one would either force a full
    /// upload every frame or skip a real change.
    pub fn clear_buffer_damage(&mut self) {
        for buffer in self.buffers.values_mut() {
            if let Some(shm) = buffer.shm_mut() {
                shm.damage = Some(Vec::new());
            }
        }
    }

    /// Mark every buffer backed by a pool as wholly changed.
    ///
    /// Used when the mapping itself moves, which invalidates any copy taken
    /// from it regardless of what the client did or did not draw.
    fn mark_pool_damaged(&mut self, client_id: u32, pool_id: u32) {
        let keys: Vec<ClientObjectId> = self
            .buffers
            .iter()
            .filter(|((cid, _), b)| {
                *cid == client_id && b.shm().is_some_and(|shm| shm.pool_id == pool_id)
            })
            .map(|(&key, _)| key)
            .collect();
        for (cid, bid) in keys {
            self.mark_buffer_damaged(cid, bid, &[]);
        }
    }

    /// Free pool resources if the pool has been destroyed and no buffers
    /// still reference it.
    fn try_cleanup_pool(&mut self, client_id: u32, pool_id: u32) {
        let pool_is_dead = self
            .shm_pools
            .get(&(client_id, pool_id))
            .is_some_and(|p| p.dead);
        if !pool_is_dead {
            return;
        }
        let has_buffers = self
            .buffers
            .values()
            .any(|b| b.client_id == client_id && b.shm().is_some_and(|shm| shm.pool_id == pool_id));
        if !has_buffers && let Some(pool) = self.shm_pools.remove(&(client_id, pool_id)) {
            drop(pool.mapping);
            unsafe { libc::close(pool.fd) };
        }
    }

    pub fn create_surface(&mut self, client_id: u32, surface_id: u32) {
        self.surfaces.insert(
            (client_id, surface_id),
            Surface {
                client_id,
                buffer_id: None,
                frame_callbacks: Vec::new(),
                presentation_feedbacks: Vec::new(),
                pending: SurfacePending::default(),
                input_region: None,
                opaque_region: None,
                buffer_scale: 1,
                buffer_transform: BufferTransform::default(),
                offset: (0, 0),
                entered_outputs: HashSet::new(),
                visible_on: HashSet::new(),
                parent: None,
                children: Vec::new(),
                subsurface_position: (0, 0),
                is_subsurface: false,
                subsurface_sync: true,
                cached: None,
                position: (0, 0),
            },
        );
    }

    pub fn destroy_surface(&mut self, client_id: u32, surface_id: u32) {
        let key = (client_id, surface_id);
        let parent = self.surfaces.remove(&key).and_then(|s| s.parent);

        // A role is permanent for the life of the *surface*, not of the id. The
        // client is about to be told the id is free again, and an id it
        // reallocates is a new surface with no role — so leaving these behind
        // would refuse a role to a surface that has never had one.
        self.cursor_role_surfaces.remove(&key);
        self.dnd_icon_surfaces.remove(&key);

        // Input that was aimed at this surface has nowhere to go now.
        //
        // The pointer is the one that bites: it is only recomputed when the
        // pointer *moves*, so a surface destroyed under a stationary pointer —
        // a menu closing, which is the common case — would leave the next
        // click delivered against an object the client has already destroyed,
        // instead of reaching whatever is now underneath. Clearing it costs at
        // worst a missing enter until the pointer next moves, which the very
        // next motion event repairs.
        if self.pointer_surface == Some(key) {
            self.pointer_surface = None;
        }
        if self.focused_surface == Some(key) {
            self.focused_surface = None;
        }
        // The surface is gone, so the question its `wp_fractional_scale_v1` was
        // asking has no answer. The object itself lives until the client
        // destroys it — it owns the id — but it is about nothing now, and leaving
        // it here would have the per-frame pass keep answering for a surface
        // that no longer exists.
        self.fractional_scales
            .retain(|&(cid, _), binding| !(cid == client_id && binding.surface_id == surface_id));
        self.touch_points.retain(|_, &mut point| point != key);

        // A subsurface or popup that goes without being unparented first would
        // otherwise leave its id in the parent's child list, where the tree
        // walks would keep looking it up and finding nothing.
        if let Some(parent) = parent
            && let Some(parent) = self.surfaces.get_mut(&(client_id, parent))
        {
            parent.children.retain(|&id| id != surface_id);
        }

        // A drag cannot outlive the surface it started from — there would be
        // nothing left to say where it came from, and the client that owns it
        // has evidently moved on.
        if self.drag.as_ref().is_some_and(|d| d.origin == key) {
            self.cancel_drag();
            return;
        }
        // An icon is not so load-bearing: the drag carries on without one.
        if let Some(drag) = self.drag.as_mut() {
            if drag.icon == Some(key) {
                drag.icon = None;
                self.dirty = true;
            }
            if self.drag.as_ref().is_some_and(|d| d.focus == Some(key)) {
                self.end_drag_focus();
            }
        }
    }

    /// Take the drag off whatever surface it was over, telling that client.
    pub fn end_drag_focus(&mut self) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        let Some(focus) = drag.focus else {
            return;
        };
        let offers = drag.focus_offers.clone();

        for device_id in wl_data_device::devices_of(self, focus.0) {
            wl_data_device::send_leave(self, focus.0, device_id);
        }
        for offer in offers {
            // The source is told the target will take nothing, so it can stop
            // drawing a drop cursor for a window the pointer has left.
            wl_data_device::clear_accepted(self, offer);
            self.invalidate_offer(offer);
        }
        if let Some(drag) = self.drag.as_mut() {
            drag.focus = None;
            drag.focus_offers.clear();
        }
    }

    pub fn create_region(&mut self, client_id: u32, region_id: u32) {
        self.regions.insert(
            (client_id, region_id),
            Region {
                client_id,
                ..Default::default()
            },
        );
    }

    pub fn destroy_region(&mut self, client_id: u32, region_id: u32) {
        self.regions.remove(&(client_id, region_id));
    }

    pub fn create_xdg_surface(&mut self, client_id: u32, xdg_surface_id: u32, wl_surface_id: u32) {
        self.xdg_surfaces.insert(
            (client_id, xdg_surface_id),
            XdgSurfaceState {
                client_id,
                wl_surface_id,
                role: None,
                configured: false,
                geometry: None,
                pending_configures: VecDeque::new(),
                highest_configure: 0,
            },
        );
    }

    pub fn destroy_xdg_surface(&mut self, client_id: u32, xdg_surface_id: u32) {
        self.xdg_surfaces.remove(&(client_id, xdg_surface_id));
    }

    pub fn create_xdg_toplevel(&mut self, client_id: u32, toplevel_id: u32, xdg_surface_id: u32) {
        self.xdg_toplevels.insert(
            (client_id, toplevel_id),
            XdgToplevelState {
                client_id,
                xdg_surface_id,
                title: None,
                app_id: None,
                min_size: (0, 0),
                max_size: (0, 0),
                maximized: false,
                fullscreen: false,
                restore: None,
                parent: None,
                sent_bounds: None,
            },
        );
        let Some(xdg_surface) = self.xdg_surfaces.get_mut(&(client_id, xdg_surface_id)) else {
            return;
        };
        xdg_surface.role = Some(XdgRole::Toplevel(toplevel_id));
        let surface_key = (client_id, xdg_surface.wl_surface_id);

        // Where the window opens — and whether it opens anywhere at all yet —
        // is the shell's decision. Asked before the initial configure goes
        // out, so the bounds sent with it describe the output chosen here.
        self.with_shell(|shell, state| shell.toplevel_created(state, surface_key));
        self.dirty = true;
    }

    pub fn destroy_xdg_toplevel(&mut self, client_id: u32, toplevel_id: u32) {
        if let Some(toplevel) = self.xdg_toplevels.remove(&(client_id, toplevel_id)) {
            // Remove the associated wl_surface from the stack and clear focus
            if let Some(xdg_surface) = self.xdg_surfaces.get(&(client_id, toplevel.xdg_surface_id))
            {
                let surface_key = (client_id, xdg_surface.wl_surface_id);
                self.with_shell(|shell, state| shell.toplevel_destroyed(state, surface_key));
                if self.resize_anchor.is_some_and(|a| a.surface == surface_key) {
                    self.resize_anchor = None;
                }
                if self.focused_surface == Some(surface_key) {
                    self.focused_surface = None;
                }
                self.dirty = true;
            }
            if let Some(decoration_id) = self.toplevel_decoration.remove(&(client_id, toplevel_id))
            {
                self.decorations.remove(&(client_id, decoration_id));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_xdg_popup(
        &mut self,
        client_id: u32,
        popup_id: u32,
        xdg_surface_id: u32,
        parent_xdg_surface_id: u32,
        positioner: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        self.xdg_popups.insert(
            (client_id, popup_id),
            XdgPopupState {
                client_id,
                xdg_surface_id,
                parent_xdg_surface_id,
                positioner,
                x,
                y,
                width,
                height,
            },
        );
        if let Some(xdg_surface) = self.xdg_surfaces.get_mut(&(client_id, xdg_surface_id)) {
            xdg_surface.role = Some(XdgRole::Popup(popup_id));
        }
    }

    pub fn destroy_xdg_popup(&mut self, client_id: u32, popup_id: u32) {
        self.xdg_popups.remove(&(client_id, popup_id));
    }

    pub fn create_xdg_positioner(&mut self, client_id: u32, positioner_id: u32) {
        self.xdg_positioners.insert(
            (client_id, positioner_id),
            XdgPositionerState {
                client_id,
                ..Default::default()
            },
        );
    }

    pub fn destroy_xdg_positioner(&mut self, client_id: u32, positioner_id: u32) {
        self.xdg_positioners.remove(&(client_id, positioner_id));
    }

    pub fn create_viewport(&mut self, client_id: u32, viewport_id: u32, surface_id: u32) {
        self.viewports.insert(
            (client_id, viewport_id),
            ViewportState {
                client_id,
                surface_id,
                source: None,
                destination: None,
                pending_source: None,
                pending_destination: None,
            },
        );
        self.surface_viewport
            .insert((client_id, surface_id), viewport_id);
    }

    pub fn destroy_viewport(&mut self, client_id: u32, viewport_id: u32) {
        if let Some(vp) = self.viewports.remove(&(client_id, viewport_id)) {
            self.surface_viewport.remove(&(client_id, vp.surface_id));
        }
    }

    pub fn create_decoration(&mut self, client_id: u32, decoration_id: u32, toplevel_id: u32) {
        self.decorations.insert(
            (client_id, decoration_id),
            DecorationState {
                client_id,
                toplevel_id,
            },
        );
        self.toplevel_decoration
            .insert((client_id, toplevel_id), decoration_id);
    }

    pub fn destroy_decoration(&mut self, client_id: u32, decoration_id: u32) {
        if let Some(dec) = self.decorations.remove(&(client_id, decoration_id)) {
            self.toplevel_decoration
                .remove(&(client_id, dec.toplevel_id));
        }
    }

    /// Remove all pools, buffers, and surfaces belonging to a disconnecting client.
    pub fn remove_client_resources(&mut self, client_id: u32) {
        // Buffers go before pools, not after: `destroy_shm_pool` frees a pool
        // only once nothing references it, so tearing them down in the other
        // order leaves every one of this client's pools marked dead but never
        // freed — its mapping alive and its descriptor open for the life of the
        // compositor.
        self.buffers.retain(|_, b| b.client_id != client_id);
        self.dmabuf_params.retain(|&(cid, _), _| cid != client_id);
        // Each of these pins a descriptor, so a leak here is a leaked fd for
        // the life of the compositor rather than a stale map entry.
        self.pending_dmabuf_imports
            .retain(|_, pending| pending.client_id != client_id);
        self.buffer_guards.retain(|&(cid, _), _| cid != client_id);
        self.releasing_buffers.retain(|&(cid, _)| cid != client_id);
        let pool_ids: Vec<u32> = self
            .shm_pools
            .iter()
            .filter(|(_, p)| p.client_id == client_id)
            .map(|(&(_, obj_id), _)| obj_id)
            .collect();
        for id in pool_ids {
            self.destroy_shm_pool(client_id, id);
        }
        self.surfaces.retain(|_, s| s.client_id != client_id);
        self.regions.retain(|_, r| r.client_id != client_id);
        self.xdg_toplevels.retain(|_, t| t.client_id != client_id);
        self.xdg_popups.retain(|_, p| p.client_id != client_id);
        self.xdg_surfaces.retain(|_, s| s.client_id != client_id);
        self.xdg_positioners.retain(|_, p| p.client_id != client_id);
        // A layer surface is otherwise only forgotten when its client asks to
        // destroy it, and a bar is at least as likely to be killed as to exit
        // tidily. Left behind, its `wl_surface` is gone but
        // `layer::ordered_for_output` still finds it — so a dead panel goes on
        // reserving its exclusive zone, holding a strip of the output that no
        // window can use for the life of the compositor.
        self.layer_surfaces.retain(|_, l| l.client_id != client_id);
        self.surface_layer.retain(|(cid, _), _| *cid != client_id);
        self.viewports.retain(|_, v| v.client_id != client_id);
        self.surface_viewport
            .retain(|(cid, _), _| *cid != client_id);
        self.decorations.retain(|_, d| d.client_id != client_id);
        self.toplevel_decoration
            .retain(|(cid, _), _| *cid != client_id);
        self.pointers.retain(|p| p.client_id != client_id);
        self.keyboards.retain(|k| k.client_id != client_id);
        self.touches.retain(|t| t.client_id != client_id);
        self.touch_points.retain(|_, &mut key| key.0 != client_id);
        self.output_bindings.retain(|(cid, _), _| *cid != client_id);
        self.cursor_surfaces.remove(&client_id);
        self.pointer_enter_serial.remove(&client_id);
        self.cursor_role_surfaces
            .retain(|(cid, _)| *cid != client_id);
        self.subsurface_map.retain(|(cid, _), _| *cid != client_id);
        self.grabbed_popups.retain(|(cid, _)| *cid != client_id);
        self.last_button_serial.remove(&client_id);
        self.pointer_grab = self.pointer_grab.filter(|g| g.surface.0 != client_id);
        self.resize_anchor = self.resize_anchor.filter(|a| a.surface.0 != client_id);
        self.with_shell(|shell, state| shell.client_removed(state, client_id));
        self.buffers_pending_release
            .retain(|(cid, _)| *cid != client_id);

        // The data protocols, before the surfaces they refer to are forgotten:
        // cancelling a drag sends a leave naming the surface it was over, and
        // clearing the selection sends to whoever has focus.
        //
        // A drag whose origin has gone cannot continue — there is nobody to
        // hand anything to. A drag whose *target* has gone can: the button is
        // still down, and it will resolve as a cancel on release.
        if self
            .drag
            .as_ref()
            .is_some_and(|d| d.origin_client == client_id)
        {
            self.cancel_drag();
        } else if let Some(drag) = self.drag.as_mut()
            && drag.focus.is_some_and(|(cid, _)| cid == client_id)
        {
            drag.focus = None;
            drag.focus_offers.clear();
        }
        // The clipboard outlives its owner only as an offer nobody can read
        // from, so it is cleared rather than left naming a source that is gone.
        if self.selection.is_some_and(|(cid, _)| cid == client_id) {
            self.clear_selection();
        }
        if self
            .primary_selection
            .is_some_and(|(cid, _)| cid == client_id)
        {
            self.clear_primary_selection();
        }
        let sources: Vec<ClientObjectId> = self
            .data_sources
            .keys()
            .filter(|&&(cid, _)| cid == client_id)
            .copied()
            .collect();
        for source in sources {
            self.invalidate_offers_from(source);
        }
        self.data_sources.retain(|&(cid, _), _| cid != client_id);
        self.data_offers.retain(|_, o| o.client_id != client_id);
        self.data_devices.retain(|d| d.client_id != client_id);

        self.primary_devices.retain(|d| d.client_id != client_id);
        self.data_control_devices
            .retain(|d| d.client_id != client_id);
        self.fractional_scales
            .retain(|&(cid, _), _| cid != client_id);
        self.cursor_shapes.remove(&client_id);
        self.dnd_icon_surfaces.retain(|(cid, _)| *cid != client_id);
        self.recent_input_serials.remove(&client_id);

        if let Some((cid, _)) = self.focused_surface
            && cid == client_id
        {
            self.focused_surface = None;
        }

        if let Some((cid, _)) = self.pointer_surface
            && cid == client_id
        {
            self.pointer_surface = None;
        }
    }
}

/// Settle the action for a drag from the two sides' masks.
///
/// Both sides name a set of actions they will allow, and the receiving side
/// names one within its set that it would rather have. The preference wins
/// where the other side also offers it; failing that the lowest bit of what
/// they agree on does, which puts copy ahead of move ahead of ask.
///
/// A client too old to have `set_actions` at all is not the same as one that
/// has it and never called it, which is why the versions come in here rather
/// than the defaults being written into the state. A version 1 or 2 source can
/// only mean a plain copy, and a version 1 or 2 target has no way to refuse
/// anything, so it is taken to accept whatever is on offer and to prefer a
/// copy — which is exactly the behaviour it had before actions existed.
fn resolve_action(
    source_actions: u32,
    source_version: u32,
    offer_actions: u32,
    offer_preferred: u32,
    offer_version: u32,
) -> u32 {
    let actions_since = crate::protocol::wl_data_source::ACTIONS_SINCE;
    let source_actions = if source_version >= actions_since {
        source_actions
    } else {
        DND_ACTION_COPY
    };
    let (offer_actions, offer_preferred) = if offer_version >= actions_since {
        (offer_actions, offer_preferred)
    } else {
        (DND_ACTION_COPY, DND_ACTION_COPY)
    };

    let available = source_actions & offer_actions;
    if available == 0 {
        return DND_ACTION_NONE;
    }
    if offer_preferred & available != 0 {
        return offer_preferred;
    }
    for action in [DND_ACTION_COPY, DND_ACTION_MOVE, DND_ACTION_ASK] {
        if available & action != 0 {
            return action;
        }
    }
    DND_ACTION_NONE
}

/// Whether a point falls inside the region built from these operations.
///
/// The operations are replayed in order rather than reduced to a set of
/// disjoint rectangles. Order is significant: a rectangle added after a
/// subtraction re-includes the overlapping area, so keeping adds and subtracts
/// in separate lists would get that case wrong. Replaying is exact for point
/// queries, which is all the compositor needs a region for.
pub fn region_contains(rects: &[RegionRect], x: i32, y: i32) -> bool {
    let mut inside = false;
    for rect in rects {
        if rect.contains(x, y) {
            inside = rect.op == RegionOp::Add;
        }
    }
    inside
}

/// Bring a client-chosen surface offset within [`MAX_SURFACE_OFFSET`].
pub fn clamp_surface_offset(x: i32, y: i32) -> (i32, i32) {
    (
        x.clamp(-MAX_SURFACE_OFFSET, MAX_SURFACE_OFFSET),
        y.clamp(-MAX_SURFACE_OFFSET, MAX_SURFACE_OFFSET),
    )
}
