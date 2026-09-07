//! The HTTP seam beneath the gate.
//!
//! Everything above this trait — routing, priority, spacing, AIMD, retries — is the gate's
//! business. Everything below is "post a body to a URL and tell me what came back". Putting
//! the seam here is what makes PLAN.md D12 possible: a full 24-hour index is ~830 MB and
//! 20–30 minutes, which is not a thing to re-run on every iteration of the indexer. So a
//! recording of a small window stands in for the chain, and phases 3 and 4 iterate in
//! seconds.
//!
//! [`ReplayTransport`] **fails loudly on a cache miss**. That is the important property: a
//! replay that silently fell through to the network would give tests a hidden dependency on
//! a live chain and on whatever that chain happened to look like that day.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

/// What came back from an endpoint. Deliberately not parsed: the gate needs the status to
/// decide about 429s and challenge pages before anything tries to read JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("network: {0}")]
    Network(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error(
        "replay cache miss: {method} (no recorded response; the recording does not cover this call)"
    )]
    CacheMiss { method: String },
    #[error("io: {0}")]
    Io(String),
}

/// Post a JSON-RPC body to an endpoint.
#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    async fn post(&self, url: &str, body: &str) -> Result<HttpResponse, TransportError>;
}

// --- live -----------------------------------------------------------------------------

/// The real thing.
#[derive(Debug)]
pub struct LiveTransport {
    client: reqwest::Client,
    timeout: Duration,
}

impl LiveTransport {
    pub fn new(timeout: Duration) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // A stable, honest user agent. Endpoints treat unlabelled clients worse, and
            // an operator who wants to block us should be able to.
            .user_agent(concat!("quarrel/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| TransportError::Network(e.to_string()))?;
        Ok(Self { client, timeout })
    }
}

#[async_trait]
impl Transport for LiveTransport {
    async fn post(&self, url: &str, body: &str) -> Result<HttpResponse, TransportError> {
        let res = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    TransportError::Timeout(self.timeout)
                } else {
                    TransportError::Network(e.to_string())
                }
            })?;
        let status = res.status().as_u16();
        let body = res
            .text()
            .await
            .map_err(|e| TransportError::Network(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

// --- the cache key --------------------------------------------------------------------

/// A request's identity, independent of its JSON-RPC `id`.
///
/// The `id` field increments per process, so including it would make every recording a
/// single-use artefact. Method plus params is what actually determines the response.
pub fn cache_key(body: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return format!("raw:{body}"),
    };
    let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("?");
    let params = parsed
        .get("params")
        .map(|p| p.to_string())
        .unwrap_or_default();
    format!("{method}:{params}")
}

fn method_of(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| "?".into())
}

/// A response's JSON-RPC `id` has to be rewritten to match the request being replayed,
/// or the caller will reject it as a mismatched reply.
fn retarget_id(recorded_body: &str, request_body: &str) -> String {
    let (Ok(mut rec), Ok(req)) = (
        serde_json::from_str::<serde_json::Value>(recorded_body),
        serde_json::from_str::<serde_json::Value>(request_body),
    ) else {
        return recorded_body.to_owned();
    };
    if let (Some(obj), Some(id)) = (rec.as_object_mut(), req.get("id")) {
        obj.insert("id".into(), id.clone());
    }
    rec.to_string()
}

// --- recording ------------------------------------------------------------------------

/// Wraps a live transport and writes every exchange to disk.
#[derive(Debug)]
pub struct RecordingTransport {
    inner: Box<dyn Transport>,
    dir: PathBuf,
    entries: Mutex<HashMap<String, String>>,
}

impl RecordingTransport {
    pub fn new(inner: Box<dyn Transport>, dir: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            dir: dir.into(),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Write the recording. One JSON file mapping cache key to response body.
    pub fn flush(&self) -> Result<usize, TransportError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| TransportError::Io(e.to_string()))?;
        let entries = self.entries.lock().expect("recording lock");
        let path = self.dir.join("responses.json");
        let json = serde_json::to_string_pretty(&*entries)
            .map_err(|e| TransportError::Io(e.to_string()))?;
        std::fs::write(&path, json).map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(entries.len())
    }
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn post(&self, url: &str, body: &str) -> Result<HttpResponse, TransportError> {
        let res = self.inner.post(url, body).await?;
        // Only successful responses are worth recording: a recorded 429 would make the
        // replay reproduce a transient endpoint mood rather than the chain.
        if res.status == 200 && !res.body.contains("\"error\"") {
            self.entries
                .lock()
                .expect("recording lock")
                .insert(cache_key(body), res.body.clone());
        }
        Ok(res)
    }
}

// --- replay ---------------------------------------------------------------------------

/// Serves recorded responses and nothing else.
#[derive(Debug)]
pub struct ReplayTransport {
    entries: HashMap<String, String>,
    misses: Mutex<Vec<String>>,
}

impl ReplayTransport {
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, TransportError> {
        let path = dir.as_ref().join("responses.json");
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            TransportError::Io(format!("cannot read recording at {}: {e}", path.display()))
        })?;
        let entries: HashMap<String, String> =
            serde_json::from_str(&raw).map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(Self {
            entries,
            misses: Mutex::new(Vec::new()),
        })
    }

    pub fn from_entries(entries: HashMap<String, String>) -> Self {
        Self {
            entries,
            misses: Mutex::new(Vec::new()),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Calls the recording could not answer. Useful for widening a recording deliberately
    /// rather than by accident.
    pub fn misses(&self) -> Vec<String> {
        self.misses.lock().expect("miss lock").clone()
    }
}

#[async_trait]
impl Transport for ReplayTransport {
    async fn post(&self, _url: &str, body: &str) -> Result<HttpResponse, TransportError> {
        match self.entries.get(&cache_key(body)) {
            Some(recorded) => Ok(HttpResponse {
                status: 200,
                body: retarget_id(recorded, body),
            }),
            None => {
                // Loud, not silent. A replay that fell through to the network would give
                // every offline test a hidden dependency on a live chain.
                let method = method_of(body);
                self.misses.lock().expect("miss lock").push(cache_key(body));
                Err(TransportError::CacheMiss { method })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Canned(String);

    #[async_trait]
    impl Transport for Canned {
        async fn post(&self, _url: &str, _body: &str) -> Result<HttpResponse, TransportError> {
            Ok(HttpResponse {
                status: 200,
                body: self.0.clone(),
            })
        }
    }

    fn req(id: u32, method: &str, params: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#)
    }

    #[test]
    fn the_cache_key_ignores_the_jsonrpc_id() {
        // Otherwise every recording would be single-use, since ids increment per process.
        let a = cache_key(&req(1, "eth_blockNumber", "[]"));
        let b = cache_key(&req(9_999, "eth_blockNumber", "[]"));
        assert_eq!(a, b);
    }

    #[test]
    fn the_cache_key_separates_different_params() {
        let a = cache_key(&req(1, "eth_getBlockByNumber", r#"["0x1",false]"#));
        let b = cache_key(&req(1, "eth_getBlockByNumber", r#"["0x2",false]"#));
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn replay_fails_loudly_on_a_miss() {
        let t = ReplayTransport::from_entries(HashMap::new());
        let err = t
            .post("http://ignored", &req(1, "eth_getLogs", "[{}]"))
            .await
            .expect_err("a miss must be an error, never a silent fallthrough");
        assert!(matches!(err, TransportError::CacheMiss { .. }));
        assert!(err.to_string().contains("eth_getLogs"), "{err}");
        assert_eq!(
            t.misses().len(),
            1,
            "misses are recorded so a recording can be widened"
        );
    }

    #[tokio::test]
    async fn replay_returns_the_recorded_body_with_the_callers_id() {
        let mut entries = HashMap::new();
        entries.insert(
            cache_key(&req(1, "eth_blockNumber", "[]")),
            r#"{"jsonrpc":"2.0","id":1,"result":"0x2a"}"#.to_string(),
        );
        let t = ReplayTransport::from_entries(entries);

        // A later call uses a different id; the reply must carry that id back.
        let res = t
            .post("http://ignored", &req(77, "eth_blockNumber", "[]"))
            .await
            .unwrap();
        assert_eq!(res.status, 200);
        let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(v["id"], 77, "the reply must match the request it answers");
        assert_eq!(v["result"], "0x2a");
    }

    #[tokio::test]
    async fn recording_round_trips_through_a_directory() {
        let dir = std::env::temp_dir().join(format!("quarrel-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let rec = RecordingTransport::new(
            Box::new(Canned(r#"{"jsonrpc":"2.0","id":1,"result":"0x7"}"#.into())),
            &dir,
        );
        rec.post("http://x", &req(1, "eth_blockNumber", "[]"))
            .await
            .unwrap();
        assert_eq!(rec.flush().unwrap(), 1);

        let replay = ReplayTransport::load(&dir).unwrap();
        assert_eq!(replay.len(), 1);
        let res = replay
            .post("http://x", &req(2, "eth_blockNumber", "[]"))
            .await
            .unwrap();
        assert!(res.body.contains("0x7"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn recording_skips_error_and_throttle_responses() {
        let dir = std::env::temp_dir().join(format!("quarrel-rec-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // A recorded 429 would make replays reproduce a transient endpoint mood.
        let rec = RecordingTransport::new(
            Box::new(Canned(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":429}}"#.into(),
            )),
            &dir,
        );
        rec.post("http://x", &req(1, "eth_getLogs", "[{}]"))
            .await
            .unwrap();
        assert_eq!(
            rec.flush().unwrap(),
            0,
            "errors must not enter the recording"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
