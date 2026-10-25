use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use hyakume_core::model::Identity;
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::error::Failure;

/// The identity behind the request's bearer token.
pub struct Caller(pub Identity);

impl FromRequestParts<AppState> for Caller {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Failure> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(Failure::unauthorized)?;
        let identity = app
            .store
            .authenticate(&hash_token(token), app.session_idle_ms)
            .await?
            .ok_or_else(Failure::unauthorized)?;
        Ok(Self(identity))
    }
}

/// 256 random bits, hex encoded with a recognizable prefix so leaked tokens
/// are easy to spot in logs or secret scanners.
pub fn new_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(format!("hyk_{}", hex(&bytes)))
}

/// Sessions are looked up by this hash, so a copy of the database does not
/// contain usable tokens.
pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
