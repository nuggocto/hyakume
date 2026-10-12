//! Parses arbitrary input as an invitation token, in text and binary form.
//!
//! Beyond not crashing, any accepted token must survive a round trip
//! through its own encoding.

#![no_main]

use hyakume::invitation::InvitationToken;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(token) = InvitationToken::from_bytes(data) {
        assert_eq!(token.to_bytes().unwrap().as_slice(), data);
    }
    if let Ok(text) = std::str::from_utf8(data)
        && let Ok(token) = InvitationToken::parse(text)
    {
        let again = InvitationToken::parse(&token.to_text().unwrap()).unwrap();
        assert_eq!(again.to_bytes().unwrap(), token.to_bytes().unwrap());
    }
});
