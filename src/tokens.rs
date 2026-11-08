use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::{qr, state::AppState};

/// How long one QR stays on screen if nobody scans it.
pub const TOKEN_TTL: i64 = 15;
/// Extra seconds the server accepts a code after it left the screen (network lag).
pub const GRACE: i64 = 3;

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// 16 random bytes = 128 bits, shown as 22 URL-safe characters.
pub fn new_token() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

pub fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub async fn issue(db: &SqlitePool, activity_id: i64) -> Result<(String, i64), sqlx::Error> {
    let token = new_token();
    let t = now();
    let exp = t + TOKEN_TTL;
    sqlx::query(
        "INSERT INTO tokens (token_hash, activity_id, issued_at, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(hash(&token))
    .bind(activity_id)
    .bind(t)
    .bind(exp)
    .execute(db)
    .await?;
    Ok((token, exp))
}

pub fn qr_message(st: &AppState, token: &str, exp: i64) -> String {
    let url = format!("{}/a/{}", st.base_url, token);
    json!({
        "type": "qr",
        "svg": qr::svg(&url),
        "expires_at": exp,
        "ttl": TOKEN_TTL,
        "now": now(),
    })
    .to_string()
}

/// Issue a fresh code, remember it, and push it to the lecturer's screen.
pub async fn rotate(st: &AppState, id: i64) -> Result<(), sqlx::Error> {
    let (token, exp) = issue(&st.db, id).await?;
    st.set_current(id, token.clone(), exp);
    st.publish(id, qr_message(st, &token, exp));
    Ok(())
}

/// The code to show right now: the live one, or a new one if it expired.
pub async fn ensure_current(st: &AppState, id: i64) -> Result<(String, i64), sqlx::Error> {
    if let Some((t, e)) = st.current(id) {
        if e > now() {
            return Ok((t, e));
        }
    }
    let (token, exp) = issue(&st.db, id).await?;
    st.set_current(id, token.clone(), exp);
    Ok((token, exp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_22_chars_and_unique() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 22);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_stable_hex() {
        assert_eq!(hash("x"), hash("x"));
        assert_eq!(hash("x").len(), 64);
    }
}
