//! Decodes arbitrary bytes as a stream of frames.
//!
//! Beyond not crashing, every decoded message must encode back to the
//! payload it came from, and a frame header over the limit must be refused
//! before its payload is needed.

#![no_main]

use hyakume::protocol::{self, HEADER_BYTES, ProtocolError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut rest = data;
    while let Ok(Some((msg, used))) = protocol::decode_frame(rest) {
        let payload = &rest[HEADER_BYTES..used];
        assert_eq!(protocol::encode_payload(&msg), payload);
        rest = &rest[used..];
    }
    if let Some(header) = data.first_chunk::<HEADER_BYTES>() {
        let len = u32::from_be_bytes(*header) as usize;
        if len > hyakume::limits::MAX_FRAME_BYTES - HEADER_BYTES {
            assert_eq!(
                protocol::decode_frame(&data[..HEADER_BYTES]),
                Err(ProtocolError::TooLarge)
            );
        }
    }
});
