//! `wp_presentation_feedback` protocol handler.
//!
//! Feedback objects are one-shot: the compositor sends either a `presented`
//! or `discarded` event, then the object is defunct. Clients don't send
//! requests to this interface (no opcodes to handle).

use tokio_way_sock::WaylandRequestWithClientInfo;

use super::super::state::CompositorState;

// Event opcodes
/// `sync_output(output: object<wl_output>)`. Optional, and never sent: it
/// names the output a frame was synchronised to, which this compositor does
/// not track per frame.
#[allow(dead_code)]
pub const SYNC_OUTPUT: u16 = 0;
/// `presented(tv_sec_hi, tv_sec_lo, tv_nsec, refresh, seq_hi, seq_lo, flags)`.
pub const PRESENTED: u16 = 1;
/// `discarded()`, for a frame that never made it to screen.
#[allow(dead_code)]
pub const DISCARDED: u16 = 2;

/// The `kind` bitmask bits of `presented`. Each says what the backend could
/// vouch for about how the frame reached the screen — see
/// [`tokio_way_backends::messages::PresentationFlags`].
pub const KIND_VSYNC: u32 = 0x1;
pub const KIND_HW_CLOCK: u32 = 0x2;
pub const KIND_HW_COMPLETION: u32 = 0x4;
pub const KIND_ZERO_COPY: u32 = 0x8;

/// Pack what the backend vouched for into the `kind` bitmask.
pub fn kind_mask(flags: tokio_way_backends::messages::PresentationFlags) -> u32 {
    let mut mask = 0;
    if flags.vsync {
        mask |= KIND_VSYNC;
    }
    if flags.hw_clock {
        mask |= KIND_HW_CLOCK;
    }
    if flags.hw_completion {
        mask |= KIND_HW_COMPLETION;
    }
    if flags.zero_copy {
        mask |= KIND_ZERO_COPY;
    }
    mask
}

pub fn handle(state: &mut CompositorState, msg: &WaylandRequestWithClientInfo) {
    super::reject_unknown_request(state, msg, "wp_presentation_feedback");
}
