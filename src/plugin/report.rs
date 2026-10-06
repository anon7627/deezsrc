//! Play reports to Deezer (`reporting` capability, setting
//! `report_playback`), modelled on the web player's listen logs: when a
//! track ends, the gateway method `log.listen` gets
//!
//! ```json
//! {"params": {"media": {"id": "<song id>", "type": "song", "format": "FLAC"},
//!             "ts_listen": <start, unix s>, "timestamp": <end, unix s>,
//!             "lt": <seconds listened>, "type": 0,
//!             "stat": {"seek": 0, "pause": 0, "sync": 1},
//!             "ctxt": {"t": "deezsrc", "id": "<song id>"},
//!             "dev": {"t": 0, "v": "<version>"}, "ls": [], "payload": {},
//!             "stream_id": "<uuid>"}}
//! ```
//!
//! The song is the one actually streamed (a fallback version, maybe), in
//! the format `track.resolve` picked. Unsent reports are kept in
//! `data_dir/play-reports.json`, so that a restart or a network failure
//! does not lose them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::resolve::Resolved;
use crate::deezer::{now_unix_s, Deezer};

/// Oldest reports are dropped beyond this many.
const MAX_QUEUED: usize = 500;
/// Resolved streams remembered for a later start (preloads, queues).
const MAX_STREAMS: usize = 64;

#[derive(Clone, Debug)]
struct Play {
    resolved: Resolved,
    started_at: i64,
}

#[derive(Default)]
struct Inner {
    /// By ref, with when they were resolved.
    streams: HashMap<String, (Resolved, i64)>,
    playing: HashMap<String, Play>,
    queue: Vec<Value>,
    loaded: bool,
}

#[derive(Default)]
pub struct Reporter {
    inner: Mutex<Inner>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    events: Vec<Value>,
}

fn queue_path(data_dir: &Path) -> PathBuf {
    data_dir.join("play-reports.json")
}

impl Reporter {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remember what `track.resolve` handed out for `reference`.
    pub fn resolved(&self, reference: &str, resolved: Resolved) {
        let mut inner = self.inner();
        inner.streams.insert(reference.to_string(), (resolved, now_unix_s()));
        if inner.streams.len() > MAX_STREAMS {
            let oldest = inner.streams.iter().min_by_key(|(_, (_, at))| *at).map(|(r, _)| r.clone());
            if let Some(r) = oldest {
                inner.streams.remove(&r);
            }
        }
    }

    /// Forget everything not yet sent (reporting turned off, signed out).
    pub fn clear(&self, data_dir: Option<&Path>) {
        let mut inner = self.inner();
        inner.playing.clear();
        inner.queue.clear();
        inner.loaded = true;
        if let Some(dir) = data_dir {
            let _ = std::fs::remove_file(queue_path(dir));
        }
    }

    /// `playback.started {ref}`: a play of a track this plugin resolved.
    pub fn started(&self, reference: &str) {
        let mut inner = self.inner();
        let Some((resolved, _)) = inner.streams.get(reference).cloned() else { return };
        inner.playing.insert(reference.to_string(), Play { resolved, started_at: now_unix_s() });
    }

    /// `playback.ended {ref, listened_ms}`: queue the report. Returns
    /// whether one was queued.
    pub fn ended(&self, reference: &str, listened_ms: u64, data_dir: Option<&Path>) -> bool {
        let mut inner = self.inner();
        let Some(play) = inner.playing.remove(reference) else { return false };
        let mut listened_s = listened_ms / 1000;
        if let Some(d) = play.resolved.duration_s {
            listened_s = listened_s.min(d);
        }
        if listened_s == 0 {
            return false;
        }
        let id = &play.resolved.song_id;
        Self::load(&mut inner, data_dir);
        inner.queue.push(json!({
            "media": {"id": id, "type": "song", "format": play.resolved.format},
            "ts_listen": play.started_at,
            "timestamp": now_unix_s(),
            "lt": listened_s,
            "type": 0,
            "stat": {"seek": 0, "pause": 0, "sync": 1},
            "ctxt": {"t": "deezsrc", "id": id},
            "dev": {"t": 0, "v": env!("CARGO_PKG_VERSION")},
            "ls": [],
            "payload": {},
            "stream_id": uuid::Uuid::new_v4().to_string(),
        }));
        let excess = inner.queue.len().saturating_sub(MAX_QUEUED);
        inner.queue.drain(..excess);
        Self::save(&inner, data_dir);
        true
    }

    fn load(inner: &mut Inner, data_dir: Option<&Path>) {
        if inner.loaded {
            return;
        }
        inner.loaded = true;
        let Some(dir) = data_dir else { return };
        if let Some(saved) =
            std::fs::read_to_string(queue_path(dir)).ok().and_then(|raw| serde_json::from_str::<Saved>(&raw).ok())
        {
            let mut events = saved.events;
            events.append(&mut inner.queue);
            inner.queue = events;
        }
    }

    fn save(inner: &Inner, data_dir: Option<&Path>) {
        let Some(dir) = data_dir else { return };
        let path = queue_path(dir);
        let result = if inner.queue.is_empty() {
            std::fs::remove_file(&path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
        } else {
            let raw = serde_json::to_string(&Saved { events: inner.queue.clone() }).expect("serialises");
            std::fs::write(&path, raw)
        };
        if let Err(e) = result {
            tracing::warn!("play reports: cannot update {}: {e}", path.display());
        }
    }

    /// Send the queued reports, oldest first. Stops at the first failure;
    /// what is left is sent next time.
    pub async fn flush(&self, dz: &Deezer, data_dir: Option<&Path>) {
        loop {
            let next = {
                let mut inner = self.inner();
                Self::load(&mut inner, data_dir);
                inner.queue.first().cloned()
            };
            let Some(event) = next else { return };
            if let Err(e) = dz.gw("log.listen", json!({"params": event})).await {
                let left = self.inner().queue.len();
                tracing::info!("play reports: {left} kept for later ({e:#})");
                return;
            }
            let mut inner = self.inner();
            // Only drop what was sent: `clear` may have run meanwhile.
            if inner.queue.first() == Some(&event) {
                inner.queue.remove(0);
            }
            Self::save(&inner, data_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(id: &str) -> Resolved {
        Resolved { song_id: id.into(), format: "FLAC", duration_s: Some(185) }
    }

    #[test]
    fn a_play_becomes_a_listen_report() {
        let dir = std::env::temp_dir().join(format!("deezsrc-report-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = Reporter::default();
        // Track 3 played as its other version, 4.
        r.resolved("track/3", resolved("4"));
        r.started("track/3");
        r.started("album/1");
        assert!(!r.ended("album/1", 10_000, Some(&dir)), "only resolved tracks");
        assert!(r.ended("track/3", 999_999, Some(&dir)), "queued");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("play-reports.json")).unwrap()).unwrap();
        let event = &saved["events"][0];
        assert_eq!(event["media"], json!({"id": "4", "type": "song", "format": "FLAC"}));
        assert_eq!(event["lt"], 185, "capped at the track's length");
        assert!(event["ts_listen"].as_i64().unwrap() <= event["timestamp"].as_i64().unwrap());

        // Nothing listened: nothing to report.
        r.started("track/3");
        assert!(!r.ended("track/3", 400, Some(&dir)));

        // A new reporter picks the saved queue up.
        let again = Reporter::default();
        again.resolved("track/5", resolved("5"));
        again.started("track/5");
        assert!(again.ended("track/5", 10_000, Some(&dir)));
        assert_eq!(again.inner().queue.len(), 2);
        again.clear(Some(&dir));
        assert!(!dir.join("play-reports.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
