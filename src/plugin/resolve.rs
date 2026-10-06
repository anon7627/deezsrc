//! `track.resolve`: a stream in a format the subscription allows and the
//! output plays natively, served decrypted by the local relay.
//!
//! 1. `song.getData` (gateway): the track token, file sizes, gain, and a
//!    `FALLBACK` version of the track, if any.
//! 2. `media.deezer.com/v1/get_url` with the license token, the track token
//!    and the formats to try, best first: the first one Deezer has.
//! 3. When Deezer refuses the track (rights, region), its fallback version
//!    is tried the same way; then `unavailable`, with Deezer's reason.
//! 4. The CDN URL goes to the relay, which hands back a `127.0.0.1` URL.

use serde::Deserialize;
use serde_json::{json, Value};

use super::items::{self, Ref};
use super::relay::{self, Relay};
use super::rpc::{RpcError, NOT_FOUND};
use super::settings::Quality;
use crate::crypto;
use crate::deezer::{now_unix_s, num, Deezer, User};

/// Lifetime assumed for a stream URL that carries no expiry of its own.
const DEFAULT_TTL_S: i64 = 10 * 60;
/// Margin taken off an announced expiry.
const EXPIRY_MARGIN_S: i64 = 60;
/// Every Deezer stream: 44.1 kHz, FLAC in 16 bits.
const RATE: u32 = 44_100;
const FLAC_BITS: u32 = 16;
/// ReplayGain reference against Deezer's `GAIN`, as open-source clients
/// convert it.
const GAIN_OFFSET_DB: f64 = 18.4;

/// What the host's current output accepts natively (`initialize.output`,
/// `output.changed`).
#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Output {
    #[serde(default)]
    pub max_rate: Option<u32>,
    #[serde(default)]
    pub max_bits: Option<u32>,
    #[serde(default)]
    pub rates: Vec<u32>,
}

impl Output {
    /// Whether a stream of this format plays natively (0 = not announced).
    pub fn accepts(&self, rate: u32, bits: u32) -> bool {
        let rate_ok = rate == 0
            || if self.rates.is_empty() { self.max_rate.is_none_or(|m| rate <= m) } else { self.rates.contains(&rate) };
        let bits_ok = bits == 0 || self.max_bits.is_none_or(|m| bits <= m);
        rate_ok && bits_ok
    }
}

/// `FILESIZE_<format>` of a song: `Some(0)` when Deezer has no such file.
fn file_size(song: &Value, q: Quality) -> Option<u64> {
    num(song.get(format!("FILESIZE_{}", q.format()).as_str()))
}

/// Formats to ask for, best first: at most `max`, allowed by the
/// subscription, played by the output as they are, and present for this
/// song.
pub fn formats(max: Quality, user: &User, output: &Output, song: &Value) -> Vec<Quality> {
    [Quality::Flac, Quality::Mp3_320, Quality::Mp3_128]
        .into_iter()
        .filter(|&q| q <= max)
        .filter(|&q| match q {
            Quality::Flac => user.lossless && output.accepts(RATE, FLAC_BITS),
            Quality::Mp3_320 => user.hq && output.accepts(RATE, 0),
            Quality::Mp3_128 => output.accepts(RATE, 0),
        })
        // MP3 128 is offered for every song, even when its size is missing.
        .filter(|&q| q == Quality::Mp3_128 || file_size(song, q) != Some(0))
        .collect()
}

fn codec(q: Quality) -> &'static str {
    if q == Quality::Flac { "flac" } else { "mp3" }
}

/// `exp` of the media answer, minus a margin.
pub fn expires_at(exp: Option<i64>, now: i64) -> i64 {
    match exp.filter(|&t| t > now) {
        Some(t) => (t - EXPIRY_MARGIN_S).max(now + 1),
        None => now + DEFAULT_TTL_S,
    }
}

/// ReplayGain track gain from Deezer's `GAIN`.
pub fn replay_gain(gain: Option<&Value>) -> Option<f64> {
    let g = match gain? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (g != 0.0).then(|| (-(g + GAIN_OFFSET_DB) * 100.0).round() / 100.0)
}

/// Deezer's answer for one song.
enum Media {
    Url { url: String, quality: Quality, exp: Option<i64> },
    /// No media: Deezer's errors (`[{code, message}]`).
    Refused(Vec<Value>),
}

/// The media of `get_url`'s answer for its only track.
fn media_of(v: &Value) -> Media {
    let entry = v.pointer("/data/0").cloned().unwrap_or_default();
    let errors = entry.get("errors").or_else(|| v.get("errors")).and_then(Value::as_array).cloned().unwrap_or_default();
    let media = entry.pointer("/media/0");
    let url = media.and_then(|m| m.pointer("/sources/0/url")).and_then(Value::as_str);
    let quality = media.and_then(|m| m.get("format")).and_then(Value::as_str).and_then(Quality::from_format);
    match (url, quality) {
        (Some(url), Some(quality)) => {
            let exp = media.and_then(|m| m.get("exp")).and_then(Value::as_i64);
            Media::Url { url: url.to_string(), quality, exp }
        }
        _ => Media::Refused(errors),
    }
}

fn reason(errors: &[Value]) -> String {
    let parts: Vec<String> = errors
        .iter()
        .map(|e| {
            let msg = e.get("message").and_then(Value::as_str).unwrap_or("refused");
            match e.get("code") {
                Some(code) => format!("{msg} ({code})"),
                None => msg.to_string(),
            }
        })
        .collect();
    if parts.is_empty() { "no media".into() } else { parts.join("; ") }
}

/// `song.getData`, `not_found` when Deezer does not know the song.
async fn song(dz: &Deezer, id: &str) -> Result<Value, RpcError> {
    let v = dz.gw("song.getData", json!({"SNG_ID": id})).await.map_err(|e| {
        let mut e = RpcError::from(e);
        if e.code == NOT_FOUND {
            e.message = format!("track {id} is not in the Deezer catalogue ({})", e.message);
        }
        e
    })?;
    if items::id_at(&v, "SNG_ID").is_none() {
        return Err(RpcError::not_found(format!("track {id}: unreadable song.getData answer")));
    }
    Ok(v)
}

/// Size of the file behind `url`, from a one-byte range request.
async fn probe_size(http: &reqwest::Client, url: &str) -> anyhow::Result<u64> {
    let resp = http.get(url).header(reqwest::header::RANGE, "bytes=0-0").send().await.map_err(|e| e.without_url())?;
    let total = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok()?.rsplit('/').next()?.trim().parse().ok());
    match (resp.status().as_u16(), total) {
        (206, Some(total)) => Ok(total),
        (200, _) => resp.content_length().ok_or_else(|| anyhow::anyhow!("CDN: no length")),
        (status, _) => anyhow::bail!("CDN answered HTTP {status}"),
    }
}

/// What play reports need from a resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The song played (a fallback's id, maybe).
    pub song_id: String,
    pub format: &'static str,
    pub duration_s: Option<u64>,
}

/// Resolve `reference` to a relay URL.
pub async fn resolve(dz: &Deezer, relay: &Relay, reference: &str, output: &Output, max: Quality) -> Result<(Value, Resolved), RpcError> {
    let Some(Ref::Track(id)) = Ref::parse(reference) else {
        return Err(RpcError::not_found(format!("{reference} is not a track")));
    };
    let user = dz.user().await?;
    let mut current = song(dz, &id).await?;
    let mut tried = vec![id.clone()];
    let (song, url, quality, exp) = loop {
        let formats = formats(max, &user, output, &current);
        if formats.is_empty() {
            return Err(RpcError::unavailable(format!(
                "no format of track {id} fits: the output takes {:?} Hz, the subscription {}",
                output.max_rate.or_else(|| output.rates.iter().copied().max()),
                if user.lossless { "FLAC and MP3" } else if user.hq { "MP3 up to 320 kbps" } else { "MP3 128 kbps" }
            )));
        }
        let names: Vec<&str> = formats.iter().map(|q| q.format()).collect();
        let refusal = match current.get("TRACK_TOKEN").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            None => vec![json!({"message": "no track token"})],
            Some(token) => match media_of(&dz.get_url(token, &names).await?) {
                Media::Url { url, quality, exp } => break (current, url, quality, exp),
                Media::Refused(errors) => errors,
            },
        };
        let fallback = current.pointer("/FALLBACK/SNG_ID").and_then(|f| items::text(&json!({"id": f}), "id"));
        let song_id = items::id_at(&current, "SNG_ID").unwrap_or_default();
        match fallback.filter(|f| items::valid_id(f) && !tried.contains(f)) {
            Some(f) => {
                tracing::info!("track {song_id} refused ({}); trying its other version {f}", reason(&refusal));
                tried.push(f.clone());
                current = song(dz, &f).await?;
            }
            None => {
                let mut e = RpcError::unavailable(format!("Deezer does not stream track {id}: {}", reason(&refusal)));
                e.data = Some(json!({"errors": refusal}));
                return Err(e);
            }
        }
    };
    let song_id = items::id_at(&song, "SNG_ID").unwrap_or(id);
    let size = match file_size(&song, quality).filter(|&s| s > 0) {
        Some(size) => size,
        None => probe_size(dz.http(), &url).await.map_err(|e| RpcError::from(e.context("stream size")))?,
    };
    let now = now_unix_s();
    let expires = expires_at(exp, now);
    let stream = relay::Stream {
        track_id: song_id.clone(),
        url,
        key: crypto::track_key(&song_id),
        size,
        codec: codec(quality),
        expires_at: expires,
    };
    let relay_url = relay.register(stream).await?;
    let mut format = json!({"sample_rate": RATE, "channels": 2, "codec": codec(quality)});
    if quality == Quality::Flac {
        format["bits"] = FLAC_BITS.into();
    }
    let duration_s = num(song.get("DURATION")).filter(|&d| d > 0);
    let mut out = json!({
        "url": relay_url,
        "expires_at": expires,
        "format": format,
        "live": false,
        "delivery": "proxied",
    });
    if let Some(d) = duration_s {
        out["duration_ms"] = (d * 1000).into();
    }
    if let Some(gain) = replay_gain(song.get("GAIN")) {
        out["replaygain"] = json!({"track_gain": gain});
    }
    Ok((out, Resolved { song_id, format: quality.format(), duration_s }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(lossless: bool, hq: bool) -> User {
        User { id: 1, name: "a".into(), offer: None, license_token: "lt".into(), lossless, hq }
    }

    fn out(max_rate: u32, max_bits: u32, rates: &[u32]) -> Output {
        Output { max_rate: Some(max_rate), max_bits: Some(max_bits), rates: rates.to_vec() }
    }

    #[test]
    fn formats_follow_setting_subscription_output_and_song() {
        use Quality::*;
        let song = json!({"FILESIZE_FLAC": "30000000", "FILESIZE_MP3_320": "9000000", "FILESIZE_MP3_128": "3600000"});
        let any = Output::default();
        assert_eq!(formats(Flac, &user(true, true), &any, &song), [Flac, Mp3_320, Mp3_128]);
        assert_eq!(formats(Mp3_320, &user(true, true), &any, &song), [Mp3_320, Mp3_128], "setting");
        assert_eq!(formats(Flac, &user(false, true), &any, &song), [Mp3_320, Mp3_128], "no lossless in the plan");
        assert_eq!(formats(Flac, &user(false, false), &any, &song), [Mp3_128], "free plan");
        assert_eq!(formats(Flac, &user(true, true), &out(192_000, 24, &[]), &song), [Flac, Mp3_320, Mp3_128]);
        assert_eq!(formats(Flac, &user(true, true), &out(48_000, 16, &[44_100, 48_000]), &song), [Flac, Mp3_320, Mp3_128]);
        assert!(formats(Flac, &user(true, true), &out(48_000, 24, &[48_000]), &song).is_empty(), "no 44.1 kHz");
        let no_flac = json!({"FILESIZE_FLAC": "0", "FILESIZE_MP3_320": 0, "FILESIZE_MP3_128": "0"});
        assert_eq!(formats(Flac, &user(true, true), &any, &no_flac), [Mp3_128], "files Deezer does not have");
        assert_eq!(formats(Flac, &user(true, true), &any, &json!({})), [Flac, Mp3_320, Mp3_128], "sizes unknown: ask");
    }

    #[test]
    fn media_answers() {
        let ok = json!({"data": [{"media": [{"media_type": "FULL", "cipher": {"type": "BF_CBC_STRIPE"}, "format": "MP3_320",
            "sources": [{"url": "https://cdn/x.mp3?hmac=y", "provider": "ak"}], "nbf": 100, "exp": 4_000_000_000i64}]}]});
        match media_of(&ok) {
            Media::Url { url, quality, exp } => {
                assert_eq!((url.as_str(), quality, exp), ("https://cdn/x.mp3?hmac=y", Quality::Mp3_320, Some(4_000_000_000)))
            }
            Media::Refused(_) => panic!("refused"),
        }
        let refused = json!({"data": [{"errors": [{"code": 2002, "message": "Track token has no sufficient rights on requested media"}]}]});
        match media_of(&refused) {
            Media::Refused(e) => assert_eq!(reason(&e), "Track token has no sufficient rights on requested media (2002)"),
            Media::Url { .. } => panic!("not refused"),
        }
        assert!(matches!(media_of(&json!({"data": [{"media": []}]})), Media::Refused(e) if e.is_empty()));
    }

    #[test]
    fn expiry_and_gain() {
        let now = 1_000_000;
        assert_eq!(expires_at(Some(1_000_600), now), 1_000_540);
        assert_eq!(expires_at(Some(999), now), now + DEFAULT_TTL_S);
        assert_eq!(expires_at(None, now), now + DEFAULT_TTL_S);
        assert_eq!(replay_gain(Some(&json!("-12.4"))), Some(-6.0));
        assert_eq!(replay_gain(Some(&json!(-9.1))), Some(-9.3));
        assert_eq!(replay_gain(Some(&json!("0"))), None, "no measure");
        assert_eq!(replay_gain(None), None);
    }
}
