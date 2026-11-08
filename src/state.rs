use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use sqlx::SqlitePool;
use tokio::sync::broadcast;

/// Per-activity live channel plus the plaintext code currently on the lecturer's screen.
/// Only hashes are stored in the database, so the live code lives in memory.
pub struct Hub {
    pub tx: broadcast::Sender<String>,
    pub current: Option<(String, i64)>, // (token, expires_at)
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub secret: Arc<Vec<u8>>,
    pub base_url: Arc<String>,
    pub mock_lti: bool,
    pub hubs: Arc<Mutex<HashMap<i64, Hub>>>,
}

fn new_hub() -> Hub {
    Hub { tx: broadcast::channel(64).0, current: None }
}

impl AppState {
    pub fn subscribe(&self, id: i64) -> broadcast::Receiver<String> {
        let mut m = self.hubs.lock().unwrap();
        m.entry(id).or_insert_with(new_hub).tx.subscribe()
    }

    pub fn publish(&self, id: i64, msg: String) {
        if let Some(h) = self.hubs.lock().unwrap().get(&id) {
            let _ = h.tx.send(msg); // Err just means nobody is watching
        }
    }

    pub fn set_current(&self, id: i64, token: String, expires_at: i64) {
        let mut m = self.hubs.lock().unwrap();
        m.entry(id).or_insert_with(new_hub).current = Some((token, expires_at));
    }

    pub fn current(&self, id: i64) -> Option<(String, i64)> {
        self.hubs.lock().unwrap().get(&id).and_then(|h| h.current.clone())
    }

    /// Returns true if there was a live code to clear.
    pub fn clear_current(&self, id: i64) -> bool {
        let mut m = self.hubs.lock().unwrap();
        m.get_mut(&id).map(|h| h.current.take().is_some()).unwrap_or(false)
    }

    /// Activities that currently have at least one lecturer screen connected.
    pub fn watched(&self) -> Vec<i64> {
        let m = self.hubs.lock().unwrap();
        m.iter().filter(|(_, h)| h.tx.receiver_count() > 0).map(|(k, _)| *k).collect()
    }

    /// Drop hubs nobody is watching so the map cannot grow forever.
    pub fn prune(&self) {
        self.hubs.lock().unwrap().retain(|_, h| h.tx.receiver_count() > 0);
    }
}
