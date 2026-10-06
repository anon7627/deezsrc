//! JSON-RPC 2.0, one message per line, and the error codes of the source
//! plugin protocol.

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::deezer::{ApiError, ApiErrorKind, HttpError, SignedOut, QUOTA_RETRY_S};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL: i64 = -32603;
pub const AUTH_REQUIRED: i64 = -32001;
pub const NOT_FOUND: i64 = -32002;
pub const UNAVAILABLE: i64 = -32003;
pub const RATE_LIMITED: i64 = -32004;
pub const NETWORK: i64 = -32005;

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

pub type RpcResult = Result<Value, RpcError>;

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(NOT_FOUND, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(UNAVAILABLE, message)
    }

    pub fn auth_required() -> Self {
        Self::new(AUTH_REQUIRED, "not signed in")
    }

    fn rate_limited(message: String, retry_after: u64) -> Self {
        let mut err = Self::new(RATE_LIMITED, message);
        err.data = Some(json!({"retry_after": retry_after.clamp(1, 3600)}));
        err
    }

    fn to_json(&self) -> Value {
        let mut e = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            e["data"] = data.clone();
        }
        e
    }
}

/// Map API and transport failures onto the protocol codes.
impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        let message = format!("{e:#}");
        if e.chain().any(|c| c.downcast_ref::<SignedOut>().is_some()) {
            return Self::new(AUTH_REQUIRED, message);
        }
        if let Some(h) = e.chain().find_map(|c| c.downcast_ref::<HttpError>()) {
            let code = match h.status {
                401 => AUTH_REQUIRED,
                403 => UNAVAILABLE,
                404 => NOT_FOUND,
                429 => return Self::rate_limited(message, h.retry_after.unwrap_or(30)),
                500..=599 => NETWORK,
                _ => INTERNAL,
            };
            return Self::new(code, message);
        }
        if let Some(a) = e.chain().find_map(|c| c.downcast_ref::<ApiError>()) {
            return match a.kind {
                ApiErrorKind::Quota => Self::rate_limited(message, QUOTA_RETRY_S),
                ApiErrorKind::NotFound => Self::new(NOT_FOUND, message),
                ApiErrorKind::Other => Self::new(INTERNAL, message),
            };
        }
        if e.chain().any(|c| c.downcast_ref::<reqwest::Error>().is_some_and(|r| r.is_timeout() || r.is_connect() || r.is_request())) {
            return Self::new(NETWORK, message);
        }
        Self::new(INTERNAL, message)
    }
}

/// One line read from the host.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
    /// An answer: this plugin sends no requests, so it is ignored.
    Response,
    /// Unparseable line, or not a JSON-RPC message: answer with this error.
    Invalid { id: Value, error: RpcError },
}

impl PartialEq for RpcError {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code
    }
}

pub fn parse(line: &str) -> Incoming {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Incoming::Invalid { id: Value::Null, error: RpcError::new(PARSE_ERROR, e.to_string()) },
    };
    let id = v.get("id").cloned();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    match (v.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => Incoming::Request { id, method: method.to_string(), params },
        (Some(method), None) => Incoming::Notification { method: method.to_string(), params },
        (None, Some(_)) if v.get("result").is_some() || v.get("error").is_some() => Incoming::Response,
        (None, id) => Incoming::Invalid {
            id: id.unwrap_or(Value::Null),
            error: RpcError::new(INVALID_REQUEST, "not a JSON-RPC 2.0 message"),
        },
    }
}

/// Serialised writes to stdout: every task sends whole messages here.
#[derive(Clone)]
pub struct Out {
    tx: mpsc::UnboundedSender<Value>,
}

impl Out {
    /// Start the stdout writer. It stops once every `Out` is dropped.
    pub fn stdout() -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let task = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(msg) = rx.recv().await {
                let mut line = msg.to_string();
                line.push('\n');
                if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                    break;
                }
            }
        });
        (Self { tx }, task)
    }

    pub fn respond(&self, id: Value, result: RpcResult) {
        let msg = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e.to_json()}),
        };
        let _ = self.tx.send(msg);
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_classified() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":3,"method":"search","params":{"query":"x"}}"#),
            Incoming::Request { id: json!(3), method: "search".into(), params: json!({"query": "x"}) }
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"output.changed","params":{}}"#),
            Incoming::Notification { method: "output.changed".into(), params: json!({}) }
        );
        assert_eq!(parse(r#"{"jsonrpc":"2.0","id":"a","result":null}"#), Incoming::Response);
        assert!(matches!(parse("{oops"), Incoming::Invalid { error: RpcError { code: PARSE_ERROR, .. }, .. }));
        assert!(matches!(parse(r#"{"id":1}"#), Incoming::Invalid { error: RpcError { code: INVALID_REQUEST, .. }, .. }));
    }

    #[test]
    fn errors_map_to_protocol_codes() {
        let http = |status, retry_after| anyhow::Error::from(HttpError { status, retry_after, body: String::new() });
        assert_eq!(RpcError::from(http(401, None)).code, AUTH_REQUIRED);
        assert_eq!(RpcError::from(http(404, None).context("api album/1")).code, NOT_FOUND);
        let limited = RpcError::from(http(429, Some(12)));
        assert_eq!(limited.code, RATE_LIMITED);
        assert_eq!(limited.data, Some(json!({"retry_after": 12})));
        assert_eq!(RpcError::from(http(503, None)).code, NETWORK);
        assert_eq!(RpcError::from(anyhow::Error::from(SignedOut).context("gateway x")).code, AUTH_REQUIRED);
        let quota = RpcError::from(anyhow::Error::from(ApiError { kind: ApiErrorKind::Quota, message: "Quota limit exceeded".into() }));
        assert_eq!((quota.code, quota.data), (RATE_LIMITED, Some(json!({"retry_after": QUOTA_RETRY_S}))));
        let gone = anyhow::Error::from(ApiError { kind: ApiErrorKind::NotFound, message: "no data".into() });
        assert_eq!(RpcError::from(gone).code, NOT_FOUND);
        assert_eq!(RpcError::from(anyhow::anyhow!("other")).code, INTERNAL);
    }
}
