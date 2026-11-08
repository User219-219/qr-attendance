//! Signed session cookie. In production the session is created from a verified
//! Moodle LTI launch; in dev it comes from /mock/launch. Everything after that is identical.

use axum::http::{header, HeaderMap};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub role: String, // "lecturer" | "student"
    pub id: String,   // reg number or staff id
    pub name: String,
    pub exp: i64, // unix seconds
}

pub fn sign(s: &Session, secret: &[u8]) -> String {
    let payload = B64.encode(serde_json::to_vec(s).expect("serialize session"));
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
    mac.update(payload.as_bytes());
    let sig = B64.encode(mac.finalize().into_bytes());
    format!("{payload}.{sig}")
}

pub fn verify(raw: &str, secret: &[u8], now: i64) -> Option<Session> {
    let (payload, sig) = raw.split_once('.')?;
    let sig = B64.decode(sig).ok()?;
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&sig).ok()?; // constant-time comparison
    let s: Session = serde_json::from_slice(&B64.decode(payload).ok()?).ok()?;
    (s.exp > now).then_some(s)
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let h = headers.get(header::COOKIE)?.to_str().ok()?;
    h.split(';').find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

pub fn session_from(headers: &HeaderMap, secret: &[u8], now: i64) -> Option<Session> {
    verify(&cookie(headers, "sid")?, secret, now)
}

pub fn set_cookie(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    let sec = if secure { "; Secure" } else { "" };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{sec}")
}
