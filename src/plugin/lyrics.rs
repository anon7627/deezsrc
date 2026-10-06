//! `lyrics.get {ref}` (capability `lyrics`): the gateway's `song.getLyrics`
//! answers
//!
//! ```json
//! {"LYRICS_ID": "2780622", "LYRICS_TEXT": "Work it\r\nMake it…",
//!  "LYRICS_SYNC_JSON": [{"lrc_timestamp": "[00:01.27]", "milliseconds": "1270",
//!                        "duration": "2150", "line": "Work it"}, {"line": ""}]}
//! ```
//!
//! Timed lines become `synced`, the text `plain`. Deezer has no lyrics for
//! many tracks, and its answer may change shape: anything unreadable is
//! `not_found`, and the host goes on with its other sources. Tracks without
//! lyrics are remembered for a while so that the host's retries stay local.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::items::Ref;
use super::rpc::{RpcError, RpcResult, AUTH_REQUIRED, NETWORK, RATE_LIMITED};
use crate::deezer::{num, Deezer};

/// How long a track without lyrics is not asked about again.
const MISSING_TTL: Duration = Duration::from_secs(6 * 3600);
const MAX_MISSING: usize = 2000;

static MISSING: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

fn known_missing(id: &str) -> bool {
    let mut m = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    m.retain(|_, at| at.elapsed() < MISSING_TTL);
    m.contains_key(id)
}

fn note_missing(id: &str) {
    let mut m = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    if m.len() >= MAX_MISSING {
        m.clear();
    }
    m.insert(id.to_string(), Instant::now());
}

fn not_found(id: &str) -> RpcError {
    RpcError::not_found(format!("no lyrics for track {id}"))
}

/// The protocol answer from `song.getLyrics`, `None` when it holds none.
pub fn convert(v: &Value) -> Option<Value> {
    let lines = v.get("LYRICS_SYNC_JSON").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let text = |l: &Value| l.get("line").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut synced: Vec<Value> = Vec::new();
    let mut last = 0;
    for l in lines {
        match num(l.get("milliseconds")) {
            Some(t) => {
                last = t;
                synced.push(json!({"time_ms": t, "text": text(l)}));
            }
            // A blank line between verses carries no time: keep it, at the
            // previous line's.
            None if text(l).is_empty() && !synced.is_empty() => synced.push(json!({"time_ms": last, "text": ""})),
            None => {}
        }
    }
    let plain = v
        .get("LYRICS_TEXT")
        .and_then(Value::as_str)
        .map(|t| t.replace("\r\n", "\n").replace("<br />", "\n").replace("<br>", "\n"))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let mut out = json!({});
    if synced.iter().any(|l| l["text"].as_str().is_some_and(|t| !t.is_empty())) {
        out["synced"] = synced.into();
    }
    if let Some(plain) = plain {
        out["plain"] = plain.into();
    }
    (out.as_object().is_some_and(|o| !o.is_empty())).then_some(out)
}

pub async fn get(dz: &Deezer, reference: &str) -> RpcResult {
    let Some(Ref::Track(id)) = Ref::parse(reference) else {
        return Err(RpcError::not_found(format!("{reference} has no lyrics")));
    };
    if known_missing(&id) {
        return Err(not_found(&id));
    }
    let answer = match dz.gw("song.getLyrics", json!({"SNG_ID": id})).await {
        Ok(v) => v,
        Err(e) => {
            let e = RpcError::from(e);
            // Signed out, offline or rate limited: the host asks again later.
            if matches!(e.code, AUTH_REQUIRED | NETWORK | RATE_LIMITED) {
                return Err(e);
            }
            tracing::debug!("lyrics of track {id}: {}", e.message);
            note_missing(&id);
            return Err(not_found(&id));
        }
    };
    match convert(&answer) {
        Some(lyrics) => Ok(lyrics),
        None => {
            note_missing(&id);
            Err(not_found(&id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_and_plain_lyrics() {
        let v = json!({"LYRICS_ID": "1", "LYRICS_TEXT": "Work it\r\nMake it\r\n",
            "LYRICS_SYNC_JSON": [
                {"lrc_timestamp": "[00:01.27]", "milliseconds": "1270", "duration": "2150", "line": "Work it"},
                {"line": ""},
                {"lrc_timestamp": "[00:03.42]", "milliseconds": 3420, "line": " Make it "}
            ]});
        assert_eq!(convert(&v), Some(json!({
            "synced": [{"time_ms": 1270, "text": "Work it"}, {"time_ms": 1270, "text": ""}, {"time_ms": 3420, "text": "Make it"}],
            "plain": "Work it\nMake it"
        })));
        assert_eq!(convert(&json!({"LYRICS_TEXT": "a<br />b"})), Some(json!({"plain": "a\nb"})));
        assert_eq!(convert(&json!({"LYRICS_SYNC_JSON": [{"line": ""}], "LYRICS_TEXT": " "})), None);
        assert_eq!(convert(&json!({})), None);
    }
}
