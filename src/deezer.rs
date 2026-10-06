//! Deezer clients:
//! - the web gateway (`www.deezer.com/ajax/gw-light.php`), with the
//!   account's `arl` cookie: the session, favourites, playlists, Flow,
//!   track tokens, lyrics;
//! - the public API (`api.deezer.com`): the catalogue;
//! - the media service (`media.deezer.com/v1/get_url`): stream URLs.
//!
//! Nothing here logs the `arl`, the `api_token`, the license token or URLs
//! with their query: error messages name the method or path only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{HeaderValue, COOKIE, SET_COOKIE};
use serde_json::{json, Value};

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";
/// The session (`api_token`, license token) is read again after this.
const SESSION_MAX_AGE: Duration = Duration::from_secs(3600);
/// Wait suggested to the host when Deezer's quota is exceeded.
pub const QUOTA_RETRY_S: u64 = 5;

/// Where the three services live. Tests point them at a mock server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub gw: String,
    pub api: String,
    pub media: String,
}

impl Endpoints {
    pub fn new(gw: Option<&str>, api: Option<&str>, media: Option<&str>) -> Self {
        let root = |v: Option<&str>, default: &str| v.unwrap_or(default).trim_end_matches('/').to_string();
        Endpoints {
            gw: root(gw, "https://www.deezer.com"),
            api: root(api, "https://api.deezer.com"),
            media: root(media, "https://media.deezer.com"),
        }
    }
}

/// Non-2xx answer, kept typed so callers can tell rate limiting (429) from
/// a missing item (404).
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    /// `Retry-After` in seconds, when sent.
    pub retry_after: Option<u64>,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.body)
    }
}

impl std::error::Error for HttpError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    NotFound,
    /// Too many requests (the public API's code 4).
    Quota,
    Other,
}

/// An error Deezer put in an otherwise successful answer.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// The `arl` is missing, expired or revoked.
#[derive(Debug, Clone, Copy)]
pub struct SignedOut;

impl std::fmt::Display for SignedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Deezer does not accept the session cookie (arl): sign in again")
    }
}

impl std::error::Error for SignedOut {}

/// What `deezer.getUserData` says about the account.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct User {
    pub id: u64,
    /// Public profile name, never the e-mail address.
    pub name: String,
    /// Subscription name (`Deezer Premium`…).
    pub offer: Option<String>,
    pub license_token: String,
    /// The subscription allows FLAC.
    pub lossless: bool,
    /// The subscription allows MP3 320 kbps.
    pub hq: bool,
}

impl User {
    fn from_user_data(results: &Value) -> Option<Self> {
        let user = results.get("USER")?;
        let id = num(user.get("USER_ID")).unwrap_or(0);
        if id == 0 {
            return None;
        }
        let opts = user.get("OPTIONS").cloned().unwrap_or_default();
        let flag = |key: &str| opts.get(key).is_some_and(truthy);
        let offer = results.get("OFFER_NAME").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        Some(User {
            id,
            name: user.get("BLOG_NAME").and_then(Value::as_str).unwrap_or("").trim().to_string(),
            offer: offer.map(str::to_string),
            license_token: opts.get("license_token").and_then(Value::as_str).unwrap_or("").to_string(),
            lossless: flag("web_lossless") || flag("mobile_lossless"),
            hq: flag("web_hq") || flag("mobile_hq"),
        })
    }
}

/// `true`, `1`, `"1"` or `"true"`.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_u64().is_some_and(|n| n != 0),
        Value::String(s) => matches!(s.as_str(), "1" | "true"),
        _ => false,
    }
}

/// A number, or a number in a string (the gateway sends both).
pub fn num(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

struct Session {
    /// `checkForm` of `deezer.getUserData`, tied to the `sid` cookie.
    api_token: String,
    /// Cookies the gateway set (`sid`…), sent back with the `arl`.
    cookies: Vec<(String, String)>,
    user: User,
    started: Instant,
}

#[derive(Clone)]
pub struct Deezer {
    http: reqwest::Client,
    ep: Arc<Endpoints>,
    arl: Option<Arc<String>>,
    session: Arc<tokio::sync::Mutex<Option<Session>>>,
}

pub fn now_unix_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Transport errors without their URL, which may carry tokens.
fn transport(e: reqwest::Error) -> anyhow::Error {
    anyhow::Error::from(e.without_url())
}

async fn http_error(resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status().as_u16();
    let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()?.trim().parse().ok());
    let body = resp.text().await.unwrap_or_default();
    HttpError { status, retry_after, body: body.chars().take(200).collect() }.into()
}

/// The public API's `{"error": {"type", "message", "code"}}`.
fn public_error(v: &Value) -> Option<ApiError> {
    let e = v.get("error")?.as_object()?;
    let code = e.get("code").and_then(Value::as_u64).unwrap_or(0);
    let message = e.get("message").and_then(Value::as_str).unwrap_or("error").to_string();
    let kind = match code {
        4 => ApiErrorKind::Quota,
        800 => ApiErrorKind::NotFound,
        _ => ApiErrorKind::Other,
    };
    Some(ApiError { kind, message: format!("{message} (code {code})") })
}

/// The gateway's `error`: `[]` or `{}` when all is well, else
/// `{"KIND": "message"}`.
fn gw_error(v: &Value) -> Option<(String, String)> {
    let e = v.get("error")?;
    let (kind, message) = match e {
        Value::Object(o) => o.iter().next().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string)))?,
        Value::Array(a) => (a.first()?.as_str().unwrap_or("ERROR").to_string(), String::new()),
        _ => return None,
    };
    Some((kind, message))
}

fn is_token_error(kind: &str, message: &str) -> bool {
    kind == "VALID_TOKEN_REQUIRED" || (kind == "GATEWAY_ERROR" && message.to_ascii_lowercase().contains("token"))
}

impl Deezer {
    /// A client without an account: the public API only.
    pub fn new(ep: Endpoints) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .build()
            .expect("http client");
        Deezer { http, ep: Arc::new(ep), arl: None, session: Arc::default() }
    }

    /// The same client, signed in with `arl` (the session is opened on
    /// first use).
    pub fn with_arl(&self, arl: &str) -> Self {
        Deezer { arl: Some(Arc::new(arl.to_string())), session: Arc::default(), ..self.clone() }
    }

    /// The same client, signed out.
    pub fn signed_out(&self) -> Self {
        Deezer { arl: None, session: Arc::default(), ..self.clone() }
    }

    pub fn has_arl(&self) -> bool {
        self.arl.is_some()
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.ep
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The account, opening the session if needed.
    pub async fn user(&self) -> Result<User> {
        let mut session = self.session.lock().await;
        if let Some(s) = session.as_ref().filter(|s| s.started.elapsed() < SESSION_MAX_AGE) {
            return Ok(s.user.clone());
        }
        *session = Some(self.open_session().await?);
        Ok(session.as_ref().expect("just set").user.clone())
    }

    /// Open the session again (new `api_token`, new license token).
    pub async fn refresh(&self) -> Result<User> {
        let mut session = self.session.lock().await;
        *session = None;
        *session = Some(self.open_session().await?);
        Ok(session.as_ref().expect("just set").user.clone())
    }

    fn cookie_header(&self, cookies: &[(String, String)]) -> Result<HeaderValue> {
        let arl = self.arl.as_deref().ok_or(SignedOut)?;
        let mut header = format!("arl={arl}");
        for (k, v) in cookies {
            header.push_str(&format!("; {k}={v}"));
        }
        Ok(HeaderValue::from_str(&header)?)
    }

    async fn gw_post(&self, method: &str, api_token: &str, cookies: &[(String, String)], args: &Value) -> Result<(Value, Vec<(String, String)>)> {
        let url = format!("{}/ajax/gw-light.php", self.ep.gw);
        let resp = self
            .http
            .post(&url)
            .query(&[("method", method), ("input", "3"), ("api_version", "1.0"), ("api_token", api_token)])
            .header(COOKIE, self.cookie_header(cookies)?)
            .json(args)
            .send()
            .await
            .map_err(transport)
            .with_context(|| format!("gateway {method}"))?;
        if !resp.status().is_success() {
            return Err(http_error(resp).await.context(format!("gateway {method}")));
        }
        let set: Vec<(String, String)> = resp
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|h| {
                let pair = h.to_str().ok()?.split(';').next()?.trim();
                let (k, v) = pair.split_once('=')?;
                (k != "arl" && !k.is_empty() && !v.is_empty()).then(|| (k.to_string(), v.to_string()))
            })
            .collect();
        let v: Value = resp.json().await.map_err(transport).with_context(|| format!("gateway {method}: not JSON"))?;
        Ok((v, set))
    }

    async fn open_session(&self) -> Result<Session> {
        let (v, cookies) = self.gw_post("deezer.getUserData", "null", &[], &json!({})).await?;
        if let Some((kind, message)) = gw_error(&v) {
            bail!("gateway deezer.getUserData: {kind} {message}");
        }
        let results = v.get("results").cloned().unwrap_or_default();
        let user = User::from_user_data(&results).ok_or(SignedOut)?;
        let api_token = results.get("checkForm").and_then(Value::as_str).unwrap_or("").to_string();
        if api_token.is_empty() {
            bail!("gateway deezer.getUserData: no api token");
        }
        tracing::debug!("session opened (lossless {}, hq {})", user.lossless, user.hq);
        Ok(Session { api_token, cookies, user, started: Instant::now() })
    }

    /// Call a gateway method; `results` of the answer. A refused
    /// `api_token` opens the session again, then the call is retried once.
    pub async fn gw(&self, method: &str, args: Value) -> Result<Value> {
        for attempt in 0..2 {
            self.user().await?;
            let (token, cookies) = {
                let s = self.session.lock().await;
                let s = s.as_ref().ok_or(SignedOut)?;
                (s.api_token.clone(), s.cookies.clone())
            };
            let (v, set) = self.gw_post(method, &token, &cookies, &args).await?;
            if !set.is_empty() {
                if let Some(s) = self.session.lock().await.as_mut() {
                    for (k, v) in set {
                        match s.cookies.iter_mut().find(|(name, _)| *name == k) {
                            Some(c) => c.1 = v,
                            None => s.cookies.push((k, v)),
                        }
                    }
                }
            }
            let Some((kind, message)) = gw_error(&v) else {
                return Ok(v.get("results").cloned().unwrap_or(Value::Null));
            };
            if is_token_error(&kind, &message) && attempt == 0 {
                tracing::debug!("gateway {method}: api token refused, opening the session again");
                *self.session.lock().await = None;
                continue;
            }
            if kind == "NEED_USER_AUTH_REQUIRED" || (is_token_error(&kind, &message) && attempt > 0) {
                return Err(anyhow!(SignedOut).context(format!("gateway {method}")));
            }
            let text = format!("gateway {method}: {kind} {message}");
            let lower = text.to_ascii_lowercase();
            let kind = if kind == "DATA_ERROR" {
                ApiErrorKind::NotFound
            } else if lower.contains("quota") || lower.contains("too many") || lower.contains("rate limit") {
                ApiErrorKind::Quota
            } else {
                ApiErrorKind::Other
            };
            return Err(ApiError { kind, message: text.trim().to_string() }.into());
        }
        unreachable!("the loop returns")
    }

    /// GET on the public API (`path` without the leading slash).
    pub async fn api(&self, path: impl AsRef<str>, query: Vec<(&'static str, String)>) -> Result<Value> {
        let path = path.as_ref();
        let url = format!("{}/{path}", self.ep.api);
        let resp = self.http.get(&url).query(&query).send().await.map_err(transport).with_context(|| format!("api {path}"))?;
        if !resp.status().is_success() {
            return Err(http_error(resp).await.context(format!("api {path}")));
        }
        let v: Value = resp.json().await.map_err(transport).with_context(|| format!("api {path}: not JSON"))?;
        if let Some(e) = public_error(&v) {
            return Err(anyhow!(e).context(format!("api {path}")));
        }
        Ok(v)
    }

    /// `media.deezer.com/v1/get_url`: a stream URL for each track token, in
    /// the first of `formats` (`FLAC`, `MP3_320`, `MP3_128`) available.
    pub async fn get_url(&self, track_token: &str, formats: &[&str]) -> Result<Value> {
        let user = self.user().await?;
        let formats: Vec<Value> = formats.iter().map(|f| json!({"cipher": "BF_CBC_STRIPE", "format": f})).collect();
        let body = json!({
            "license_token": user.license_token,
            "media": [{"type": "FULL", "formats": formats}],
            "track_tokens": [track_token],
        });
        let url = format!("{}/v1/get_url", self.ep.media);
        let resp = self.http.post(&url).json(&body).send().await.map_err(transport).context("media get_url")?;
        if !resp.status().is_success() {
            return Err(http_error(resp).await.context("media get_url"));
        }
        resp.json().await.map_err(transport).context("media get_url: not JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_data_is_read() {
        let v = json!({"checkForm": "t", "OFFER_NAME": "Deezer Premium", "USER": {"USER_ID": "12", "BLOG_NAME": "alice",
            "EMAIL": "a@b.c", "OPTIONS": {"license_token": "lt", "web_lossless": false, "mobile_lossless": true, "web_hq": "1"}}});
        let u = User::from_user_data(&v).unwrap();
        assert_eq!(u, User { id: 12, name: "alice".into(), offer: Some("Deezer Premium".into()), license_token: "lt".into(), lossless: true, hq: true });
        assert_eq!(User::from_user_data(&json!({"USER": {"USER_ID": 0}})), None, "not signed in");
    }

    #[test]
    fn errors_in_answers() {
        assert!(gw_error(&json!({"error": [], "results": {}})).is_none());
        assert!(gw_error(&json!({"error": {}, "results": {}})).is_none());
        assert_eq!(gw_error(&json!({"error": {"DATA_ERROR": "song not found"}})), Some(("DATA_ERROR".into(), "song not found".into())));
        assert!(is_token_error("VALID_TOKEN_REQUIRED", "Invalid CSRF token"));
        assert!(is_token_error("GATEWAY_ERROR", "invalid api token"));
        let e = public_error(&json!({"error": {"type": "Exception", "message": "Quota limit exceeded", "code": 4}})).unwrap();
        assert_eq!(e.kind, ApiErrorKind::Quota);
        assert_eq!(public_error(&json!({"error": {"type": "DataException", "message": "no data", "code": 800}})).unwrap().kind, ApiErrorKind::NotFound);
        assert!(public_error(&json!({"data": []})).is_none());
    }
}
