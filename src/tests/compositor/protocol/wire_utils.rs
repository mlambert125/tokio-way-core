//! Tests for the wire format, and in particular for the one distinction it
//! makes that is easy to write past: a null string is not an empty one.

use tokio_way_core::protocol::wire_utils::{ArgReader, ArgWriter};

#[test]
fn a_null_string_is_not_an_empty_one() {
    // A null string is a length of zero and nothing else. An empty string is a
    // length of one, a NUL byte, and three bytes of padding. Sending the second
    // where the first belongs tells `wl_data_source.target` that a zero-length
    // mime type was accepted, rather than that nothing was.
    let null = ArgWriter::new().string_or_null(None).build();
    let empty = ArgWriter::new().string_or_null(Some("")).build();

    assert_eq!(null, vec![0, 0, 0, 0]);
    assert_eq!(empty, vec![1, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn a_nullable_string_survives_the_round_trip() {
    for value in [None, Some(String::new()), Some("text/plain".to_string())] {
        let args = ArgWriter::new().string_or_null(value.as_deref()).build();
        let mut reader = ArgReader::new(&args);
        assert_eq!(reader.string_or_null(), Some(value));
    }
}

#[test]
fn a_nullable_string_reader_tells_a_short_buffer_from_a_null() {
    // Nothing at all is a decode failure; four zero bytes are a null string.
    assert_eq!(ArgReader::new(&[]).string_or_null(), None);
    assert_eq!(ArgReader::new(&[0, 0, 0, 0]).string_or_null(), Some(None));
}

#[test]
fn a_null_object_is_a_zero_id() {
    assert_eq!(ArgWriter::new().object(None).build(), vec![0, 0, 0, 0]);
    assert_eq!(ArgWriter::new().object(Some(7)).build(), vec![7, 0, 0, 0]);
}
