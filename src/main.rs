mod auth;
mod qr;
mod routes;
mod state;
mod tokens;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use state::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mock_lti = std::env::var("MOCK_LTI").map(|v| v == "1").unwrap_or(false);
    let secret = match std::env::var("SECRET") {
        Ok(s) if s.len() >= 16 => s,
        _ if mock_lti => {
            tracing::warn!("MOCK_LTI=1 with a built-in dev SECRET. Never run like this in production.");
            "dev-secret-change-me-please".to_string()
        }
        _ => panic!("Set SECRET to a random string of at least 16 characters"),
    };
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000);
    let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| format!("http://localhost:{port}"));
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://attendance.db".into());

    let opts = SqliteConnectOptions::from_str(&db_url)
        .expect("bad DATABASE_URL")
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .expect("cannot open database");
    sqlx::migrate!().run(&db).await.expect("migrations failed");

    let st = AppState {
        db,
        secret: Arc::new(secret.into_bytes()),
        base_url: Arc::new(base_url.trim_end_matches('/').to_string()),
        mock_lti,
        hubs: Arc::new(Mutex::new(HashMap::new())),
    };

    tokio::spawn(rotator(st.clone()));

    // 127.0.0.1 = this computer only (safe default). Set BIND=0.0.0.0 to accept other devices on the same network.
    let host: IpAddr = std::env::var("BIND").ok().and_then(|h| h.parse().ok()).unwrap_or(Ipv4Addr::LOCALHOST.into());
    let addr = SocketAddr::new(host, port);
    tracing::info!("listening on http://{addr} (public URL: {})", st.base_url);
    let listener = tokio::net::TcpListener::bind(addr).await.expect("cannot bind port");
    axum::serve(listener, routes::app(st)).await.expect("server error");
}

/// Once a second, for every activity with a lecturer screen open:
/// swap in a fresh QR when the current one has expired, and announce when the window closes.
async fn rotator(st: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        st.prune();
        for id in st.watched() {
            match routes::activity_state(&st, id).await {
                Some("open") => {
                    let expired = st.current(id).map(|(_, e)| e <= tokens::now()).unwrap_or(true);
                    if expired {
                        if let Err(e) = tokens::rotate(&st, id).await {
                            tracing::error!("rotate failed: {e}");
                        }
                    }
                }
                Some(_) => {
                    if st.clear_current(id) {
                        st.publish(id, json!({ "type": "closed" }).to_string());
                    }
                }
                None => {}
            }
        }
    }
}
