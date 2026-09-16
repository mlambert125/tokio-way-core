//! Wayland wire format serialization and deserialization.
//!
//! Provides low-level helpers for reading and writing the Wayland binary
//! protocol: `ArgWriter` (builder for outgoing message args), `ArgReader`
//! (cursor-based parser for incoming args), and the `message()` constructor.

use std::os::fd::OwnedFd;
use tokio_way_sock::WaylandEvent;

/// Cursor-based reader for parsing Wayland message arguments.
pub struct ArgReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[allow(dead_code)]
impl<'a> ArgReader<'a> {
    /// Create a new argument reader
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Attempt to read a u32 from the buffer and advance the cursor
    pub fn u32(&mut self) -> Option<u32> {
        let val = read_u32(self.buf, self.pos)?;
        self.pos += 4;
        Some(val)
    }

    /// Attempt to read an i32 from the buffer and advance the cursor
    pub fn i32(&mut self) -> Option<i32> {
        let val = read_i32(self.buf, self.pos)?;
        self.pos += 4;
        Some(val)
    }

    /// Attempt to read a wayland string from the buffer and advance the cursor
    pub fn string(&mut self) -> Option<String> {
        let (s, consumed) = read_string(self.buf, self.pos)?;
        self.pos += consumed;
        Some(s)
    }

    /// Attempt to read a nullable wayland string and advance the cursor.
    ///
    /// The outer `None` is a decode failure; the inner one is a null string,
    /// which `wl_data_offer.accept` sends to say it will take nothing. See
    /// [`read_string_or_null`] for why the two cannot be collapsed.
    ///
    /// The nesting is the point rather than an oversight — the two layers mean
    /// different things and the caller acts differently on each — so the lint
    /// against it does not apply.
    #[allow(clippy::option_option)]
    pub fn string_or_null(&mut self) -> Option<Option<String>> {
        let (s, consumed) = read_string_or_null(self.buf, self.pos)?;
        self.pos += consumed;
        Some(s)
    }

    /// Attempt to read a fixed-point decimal from the buffer, convert it to a `f64` and advance the cursor
    pub fn fixed(&mut self) -> Option<f64> {
        let raw = self.i32()?;
        Some(f64::from(raw) / 256.0)
    }

    /// Alias for u32 — reads a `new_id` argument.
    pub fn new_id(&mut self) -> Option<u32> {
        self.u32()
    }
}

/// Writer for building Wayland message argument buffers.
pub struct ArgWriter {
    buf: Vec<u8>,
}

#[allow(dead_code)]
impl ArgWriter {
    /// Create a new `ArgWriter` with an emptargument buffer
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Adds a `u32` to the argument buffer
    pub fn u32(mut self, val: u32) -> Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Adds an `i32` to the argument buffer
    pub fn i32(mut self, val: i32) -> Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Adds a wayland string to the argument buffer
    pub fn string(mut self, val: &str) -> Self {
        assert!(
            val.len() < u32::MAX as usize,
            "String too long for Wayland protocol"
        );
        let len = u32::try_from(val.len()).expect("String too long for Wayland protocol") + 1;
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(val.as_bytes());
        self.buf.push(0); // null terminator
        // pad to 4-byte boundary
        let padded = ((len as usize) + 3) & !3;
        let padding = padded - len as usize;
        self.buf.extend(std::iter::repeat_n(0u8, padding));
        self
    }

    /// Adds a `wl_array` of `u32`s.
    ///
    /// On the wire an array is a byte count followed by the bytes, padded out
    /// to a four-byte boundary — the count is of *bytes*, not elements, which
    /// is the easy thing to get wrong when there is no helper and each caller
    /// counts for itself.
    pub fn array_u32(mut self, values: &[u32]) -> Self {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let len = u32::try_from(bytes.len()).expect("array too long for Wayland protocol");
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(&bytes);
        let padding = (4 - (bytes.len() % 4)) % 4;
        self.buf.extend(std::iter::repeat_n(0u8, padding));
        self
    }

    /// Adds a nullable object or `new_id` argument. A null object is a zero id.
    pub fn object(self, val: Option<u32>) -> Self {
        self.u32(val.unwrap_or(0))
    }

    /// Adds a nullable string.
    ///
    /// Not the same as an empty one, and the difference is on the wire: a null
    /// string is a length of zero and nothing else, while an empty string is a
    /// length of one, a NUL byte, and three bytes of padding.
    /// `wl_data_source.target` distinguishes them — null means the target will
    /// take nothing — so writing `string("")` where a null belongs tells the
    /// source that a zero-length mime type was accepted.
    pub fn string_or_null(self, val: Option<&str>) -> Self {
        match val {
            Some(s) => self.string(s),
            None => self.u32(0),
        }
    }

    /// Adds a 64-bit float as a 24.8 fixed point decimal to the buffer
    pub fn fixed(self, val: f64) -> Self {
        self.i32(f64_to_24_8_fixed(val))
    }

    pub fn build(self) -> Vec<u8> {
        self.buf
    }
}

/// Read a u32 from a byte slice at the given offset.
#[allow(dead_code)]
pub fn read_u32(args: &[u8], offset: usize) -> Option<u32> {
    args.get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Read an i32 from a byte slice at the given offset.
#[allow(dead_code)]
pub fn read_i32(args: &[u8], offset: usize) -> Option<i32> {
    args.get(offset..offset + 4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Read a Wayland string that may be null.
///
/// Returns (`Option<String>`, `bytes_consumed`), where the inner `None` is a
/// null string and `Some(String::new())` is an empty one. The two are different
/// on the wire and mean different things: `wl_data_source.target` sends null to
/// say the target will take nothing at all, and a client reading that as an
/// empty string would believe a zero-length mime type had been accepted.
///
/// [`read_string`] collapses the two, which is right for every interface that
/// has no nullable string and wrong for the ones that do.
pub fn read_string_or_null(args: &[u8], offset: usize) -> Option<(Option<String>, usize)> {
    if read_u32(args, offset)? == 0 {
        return Some((None, 4));
    }
    read_string(args, offset).map(|(s, consumed)| (Some(s), consumed))
}

/// Read a Wayland string from a byte slice at the given offset.
/// Returns (`String`, `bytes_consumed` including padding).
/// Wire format: u32 length (including null), then chars + null, padded to 4 bytes.
pub fn read_string(args: &[u8], offset: usize) -> Option<(String, usize)> {
    let len = read_u32(args, offset)? as usize;
    if len == 0 {
        return Some((String::new(), 4));
    }
    let padded = (len + 3) & !3;
    let start = offset + 4;
    let end = start + len - 1; // exclude null terminator
    if args.len() < start + padded {
        return None;
    }
    let s = String::from_utf8_lossy(&args[start..end]).into_owned();
    Some((s, 4 + padded))
}

/// Build a [`WaylandEvent`] with no file descriptors.
pub fn build_message(object_id: u32, op_code: u16, args: Vec<u8>) -> WaylandEvent {
    WaylandEvent {
        object_id,
        op_code,
        args,
        fds: Vec::new(),
    }
}

/// Build a [`WaylandEvent`] carrying file descriptors as ancillary data.
///
/// The event owns them from here on. `SCM_RIGHTS` duplicates a descriptor
/// into the receiving client, so our copy still has to be closed afterwards —
/// dropping the event does that on every path, sent or not, which is what
/// makes a send that fails or a client that has gone away leak nothing.
pub fn build_message_with_fds(
    object_id: u32,
    op_code: u16,
    args: Vec<u8>,
    fds: Vec<OwnedFd>,
) -> WaylandEvent {
    WaylandEvent {
        object_id,
        op_code,
        args,
        fds,
    }
}

/// Convert a f64 to Wayland's 24.8 fixed-point format (i32 with 8 fractional bits).
pub fn f64_to_24_8_fixed(val: f64) -> i32 {
    f64_to_i32(val * 256.0)
}

/// Convert an `f64` to `i32` without any scaling.
///
/// Saturating, and deliberately so. Every value that reaches here has come
/// from a client one way or another — a pointer coordinate is the cursor
/// position less a surface origin the client chose, and a surface origin is
/// whatever a `wl_subsurface.set_position` said it was. `to_int_unchecked` is
/// undefined behaviour for NaN or anything outside the range, which makes the
/// soundness of this function a property of arithmetic several modules away.
/// Rust's `as` clamps to the bounds and maps NaN to zero, which costs nothing
/// measurable and cannot be wrong.
#[allow(clippy::cast_possible_truncation)]
pub fn f64_to_i32(val: f64) -> i32 {
    val as i32
}
