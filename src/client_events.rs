//! Bounded, replayable Ruffle game-event journal shared by pipe consumers.
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use xenomit_game_api::{ClientEventBatch, ClientEventJournal};

static JOURNAL: LazyLock<Mutex<ClientEventJournal>> =
    LazyLock::new(|| Mutex::new(ClientEventJournal::default()));

pub fn push(kind: &str, text: Option<String>, data: serde_json::Value) {
    let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()
        .as_millis().min(u128::from(u64::MAX)) as u64;
    JOURNAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(at, kind, text, data);
}

pub fn poll(cursor: u64) -> Result<ClientEventBatch, String> {
    JOURNAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        .poll(cursor).map_err(|error| error.to_string())
}
