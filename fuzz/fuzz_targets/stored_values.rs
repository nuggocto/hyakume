//! Parses arbitrary input the way values from the state database and the
//! command line are parsed: state names, routes, aliases, and saved offers.
//! Beyond not crashing, every accepted value must survive a round trip
//! through its own encoding.

#![no_main]

use hyakume::invitation::Route;
use hyakume::offer::Offer;
use hyakume::protocol::{self, Message};
use hyakume::transfer::{ItemState, OfferState};
use hyakume::trust::LocalAlias;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Some(state) = ItemState::from_name(text) {
            assert_eq!(state.name(), text);
        }
        if let Some(state) = OfferState::from_name(text) {
            assert_eq!(state.name(), text);
        }
        if let Ok(route) = Route::parse(text) {
            assert_eq!(Route::parse(&route.to_text().unwrap()).unwrap(), route);
        }
        if let Ok(alias) = LocalAlias::new(text) {
            assert_eq!(alias.as_str(), text);
        }
    }
    if let Ok(Message::Offer(fields)) = protocol::decode_payload(data)
        && let Ok(offer) = Offer::validate(fields)
    {
        assert_eq!(Offer::validate(offer.fields()).unwrap(), offer);
    }
});
