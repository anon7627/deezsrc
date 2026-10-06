//! Local relay: serves each resolved track, decrypted, on
//! `http://127.0.0.1:<port>/<token>/<track>.<flac|mp3>`.
//!
//! - `Content-Length` is the size of the encrypted file, which decryption
//!   keeps: the host sees an ordinary file.
//! - `Range: bytes=a-b` is honoured (`206`): the relay asks the CDN for the
//!   2048-byte chunks covering the range, decrypts them and drops what lies
//!   outside it. Every chunk decrypts on its own, so any offset works.
//! - Tokens are random and forgotten when the CDN URL expires; an unknown
//!   token answers `410 Gone`, and the host resolves the track again. So
//!   does a CDN that refuses the URL (401, 403, 404, 410).
//!
//! The bytes served are the original FLAC or MP3 file: nothing is decoded
//! or re-encoded. CDN URLs are signed: they are never logged.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::stream::{self, BoxStream, StreamExt};
use tokio::sync::OnceCell;

use crate::crypto::{Decryptor, CHUNK};
use crate::deezer::now_unix_s;

/// Streams registered at once; the oldest go first.
const MAX_STREAMS: usize = 64;

/// A resolved track.
pub struct Stream {
    /// The id the file was fetched for (a fallback's, maybe), which gives
    /// the key.
    pub track_id: String,
    /// CDN URL of the encrypted file.
    pub url: String,
    pub key: [u8; 16],
    /// File size in bytes.
    pub size: u64,
    /// `flac` or `mp3`.
    pub codec: &'static str,
    pub expires_at: i64,
}

struct Entry {
    stream: Stream,
    registered: i64,
}

#[derive(Default)]
struct Shared {
    streams: Mutex<HashMap<String, Arc<Entry>>>,
    http: reqwest::Client,
}

/// The relay; its HTTP server starts with the first stream.
#[derive(Default, Clone)]
pub struct Relay {
    shared: Arc<Shared>,
    port: Arc<OnceCell<u16>>,
}

impl Relay {
    /// Register `stream`; returns its URL.
    pub async fn register(&self, stream: Stream) -> Result<String> {
        let port = self.port.get_or_try_init(|| self.start()).await?;
        let name = format!("{}.{}", stream.track_id, stream.codec);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let now = now_unix_s();
        let mut streams = self.shared.streams.lock().unwrap_or_else(|e| e.into_inner());
        streams.retain(|_, e| e.stream.expires_at > now);
        while streams.len() >= MAX_STREAMS {
            let oldest = streams.iter().min_by_key(|(_, e)| e.registered).map(|(t, _)| t.clone());
            if let Some(t) = oldest {
                streams.remove(&t);
            }
        }
        streams.insert(token.clone(), Arc::new(Entry { stream, registered: now }));
        Ok(format!("http://127.0.0.1:{port}/{token}/{name}"))
    }

    async fn start(&self) -> Result<u16> {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.context("relay: bind")?;
        let port = listener.local_addr()?.port();
        let app = Router::new().route("/{token}/{file}", get(serve).head(serve)).with_state(self.shared.clone());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!("relay stopped: {e}");
            }
        });
        tracing::info!("relay listening on 127.0.0.1:{port}");
        Ok(port)
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against a file of `size` bytes:
/// the inclusive range, `Err` when it cannot be satisfied, `None` for the
/// whole file (no header, or one that is not a single byte range).
fn byte_range(headers: &HeaderMap, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = headers.get(header::RANGE)?.to_str().ok()?.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let range = match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(a), Some(b)) if a <= b => (a, b.min(size.saturating_sub(1))),
        (Some(a), None) if b.is_empty() => (a, size.saturating_sub(1)),
        (None, Some(n)) if a.is_empty() && n > 0 => (size.saturating_sub(n), size.saturating_sub(1)),
        _ => return None,
    };
    Some(if size == 0 || range.0 >= size || range.0 > range.1 { Err(()) } else { Ok(range) })
}

/// The CDN range to ask for bytes `start..=end`: whole chunks, so that
/// each one can be decrypted.
fn upstream_range(start: u64, end: u64, size: u64) -> (u64, u64) {
    let chunk = CHUNK as u64;
    let from = start - start % chunk;
    let to = ((end / chunk + 1) * chunk).min(size) - 1;
    (from, to)
}

/// Where a `206` answer starts (`Content-Range: bytes a-b/n`).
fn content_range_start(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    v.trim().strip_prefix("bytes ")?.split('-').next()?.trim().parse().ok()
}

fn mime(codec: &str) -> &'static str {
    if codec == "flac" { "audio/flac" } else { "audio/mpeg" }
}

async fn serve(
    State(shared): State<Arc<Shared>>,
    Path((token, _file)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let entry = shared.streams.lock().unwrap_or_else(|e| e.into_inner()).get(&token).cloned();
    let Some(entry) = entry.filter(|e| e.stream.expires_at > now_unix_s()) else {
        return (StatusCode::GONE, "stream expired").into_response();
    };
    let size = entry.stream.size;
    let (status, start, end) = match byte_range(&headers, size) {
        None => (StatusCode::OK, 0, size.saturating_sub(1)),
        Some(Ok((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Some(Err(())) => {
            return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{size}"))]).into_response();
        }
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime(entry.stream.codec))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        match decrypted(&shared, &entry, start, end).await {
            Ok(body) => Body::from_stream(body),
            Err((code, e)) => {
                tracing::warn!("relay: track {}: {e:#}", entry.stream.track_id);
                return (code, "upstream error").into_response();
            }
        }
    };
    builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

type Chunks = BoxStream<'static, std::io::Result<Bytes>>;

/// Bytes `start..=end` of the file, decrypted as they come from the CDN.
/// Fails before anything is sent when the CDN refuses: `410` when the URL
/// is no longer good (the host then resolves again), `502` otherwise.
async fn decrypted(shared: &Shared, entry: &Entry, start: u64, end: u64) -> Result<Chunks, (StatusCode, anyhow::Error)> {
    let (from, to) = upstream_range(start, end, entry.stream.size);
    let resp = shared
        .http
        .get(&entry.stream.url)
        .header(reqwest::header::RANGE, format!("bytes={from}-{to}"))
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, anyhow::Error::from(e.without_url()).context("CDN request")))?;
    let status = resp.status().as_u16();
    let served_from = match status {
        206 => content_range_start(resp.headers()).unwrap_or(from),
        // The whole file: the CDN ignored the range.
        200 => 0,
        401 | 403 | 404 | 410 => return Err((StatusCode::GONE, anyhow::anyhow!("CDN answered HTTP {status}"))),
        _ => return Err((StatusCode::BAD_GATEWAY, anyhow::anyhow!("CDN answered HTTP {status}"))),
    };
    if served_from % CHUNK as u64 != 0 || served_from > start {
        return Err((StatusCode::BAD_GATEWAY, anyhow::anyhow!("CDN served from byte {served_from}, asked {from}")));
    }
    let decryptor = Decryptor::new(&entry.stream.key, served_from);
    // Bytes to drop before `start`, then bytes to send.
    let skip = start - served_from;
    let remaining = end - start + 1;
    let state = (resp.bytes_stream().boxed(), decryptor, skip, remaining, false);
    let chunks = stream::unfold(state, |(mut upstream, mut dec, mut skip, mut remaining, done)| async move {
        if remaining == 0 {
            return None;
        }
        if done {
            let e = std::io::Error::other(format!("CDN body ended {remaining} bytes early"));
            return Some((Err(e), (upstream, dec, skip, 0, true)));
        }
        loop {
            let mut out = Vec::new();
            let finished = match upstream.next().await {
                Some(Ok(bytes)) => {
                    dec.feed(&bytes, &mut out);
                    false
                }
                Some(Err(e)) => {
                    let e = std::io::Error::other(format!("CDN body: {}", e.without_url()));
                    return Some((Err(e), (upstream, dec, skip, 0, true)));
                }
                None => {
                    dec.finish(&mut out);
                    true
                }
            };
            let drop = skip.min(out.len() as u64) as usize;
            skip -= drop as u64;
            let mut out = out.split_off(drop);
            out.truncate(remaining.min(out.len() as u64) as usize);
            remaining -= out.len() as u64;
            if !out.is_empty() {
                return Some((Ok(Bytes::from(out)), (upstream, dec, skip, remaining, finished)));
            }
            if finished {
                let e = std::io::Error::other(format!("CDN body ended {remaining} bytes early"));
                return Some((Err(e), (upstream, dec, skip, 0, true)));
            }
        }
    });
    Ok(chunks.boxed())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(h: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, h.parse().unwrap());
        byte_range(&headers, size)
    }

    #[test]
    fn ranges() {
        assert_eq!(byte_range(&HeaderMap::new(), 100), None);
        assert_eq!(range("bytes=0-9", 100), Some(Ok((0, 9))));
        assert_eq!(range("bytes=90-", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=90-500", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=100-", 100), Some(Err(())));
        assert_eq!(range("bytes=0-1,5-6", 100), None, "several ranges: the whole file");
        assert_eq!(range("items=0-1", 100), None);
    }

    #[test]
    fn upstream_ranges_cover_whole_chunks() {
        assert_eq!(upstream_range(0, 0, 10_000), (0, 2047));
        assert_eq!(upstream_range(2047, 2048, 10_000), (0, 4095));
        assert_eq!(upstream_range(5000, 9999, 10_000), (4096, 9999), "the last chunk is short");
        assert_eq!(upstream_range(4096, 6143, 10_000), (4096, 6143));
    }
}
