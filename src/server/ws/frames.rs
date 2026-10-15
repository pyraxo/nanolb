use crate::app::{Account, App};
use crate::time::now_ms;
use crate::upstream::{self, UpstreamError, codex};
use axum::extract::ws;
use axum::http::{HeaderMap, header};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{self as tung, Message};

pub(super) const PASS_HEADERS: &[&str] = &["openai-model", "x-reasoning-included", "x-models-etag", "x-request-id"];

pub(super) fn ws_url(http: &str) -> String {
    if let Some(r) = http.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = http.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        http.to_owned()
    }
}

pub(super) fn handshake(app: &App, acc: &Account, client: &HeaderMap, url: Option<&str>) -> Result<Request, UpstreamError> {
    let built = codex::request(app, acc, reqwest::Method::GET, "/codex/responses", Some(client))
        .build()
        .map_err(UpstreamError::Handshake)?;
    let url = url.map_or_else(|| ws_url(built.url().as_str()), str::to_owned);
    let mut req = url.into_client_request().map_err(UpstreamError::HandshakeUrl)?;
    let hs = req.headers_mut();
    for (k, v) in built.headers() {
        if k != header::ACCEPT {
            hs.append(k.clone(), v.clone());
        }
    }
    Ok(req)
}

pub(super) fn pass_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src {
        if PASS_HEADERS.contains(&k.as_str()) || k.as_str().starts_with("x-codex-") {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

pub(super) fn retry_until(status: u16, err: &Value) -> Option<i64> {
    let code = err["code"].as_str().or_else(|| err["type"].as_str()).unwrap_or_default();
    let reset = upstream::limit_reset_from_error(&json!({ "error": err }));
    match status {
        429 => Some(reset.unwrap_or_else(|| now_ms() + 60_000)),
        401 => Some(now_ms() + 300_000),
        403 => Some(0),
        500.. => Some(now_ms() + 5_000),
        _ if reset.is_some() => reset,
        _ if code.contains("rate_limit")
            || code.contains("usage_limit")
            || code.contains("overloaded")
            || code.contains("server_error") =>
        {
            Some(now_ms() + 5_000)
        }
        _ => None,
    }
}

pub(super) fn event_type(t: &str) -> &str {
    t.find("\"type\":\"")
        .map(|i| &t[i + 8..])
        .and_then(|r| r.find('"').map(|j| &r[..j]))
        .unwrap_or_default()
}

pub(super) fn status_of(status: Option<u64>, default: u16) -> u16 {
    status.and_then(|s| u16::try_from(s).ok()).unwrap_or(default)
}

pub(super) fn error_frame(status: u16, typ: &str, msg: &str, retry_after: Option<i64>) -> String {
    let mut v = json!({"type": "error", "status": status, "error": {"type": typ, "message": msg}});
    if let Some(s) = retry_after {
        v["headers"] = json!({"retry-after": s.to_string()});
        v["error"]["resets_at"] = json!((now_ms() / 1000) + s);
    }
    v.to_string()
}

pub(super) fn error_message(status: u16, typ: &str, msg: &str) -> ws::Message {
    ws::Message::Text(error_frame(status, typ, msg, None).into())
}

pub(super) fn close_to_client(f: Option<CloseFrame>) -> ws::Message {
    ws::Message::Close(f.map(|f| ws::CloseFrame {
        code: f.code.into(),
        reason: ws::Utf8Bytes::try_from(Bytes::from(f.reason)).unwrap_or_default(),
    }))
}

pub(super) fn to_client(m: Message) -> Option<ws::Message> {
    Some(match m {
        Message::Text(t) => ws::Message::Text(ws::Utf8Bytes::try_from(Bytes::from(t)).ok()?),
        Message::Binary(b) => ws::Message::Binary(b),
        Message::Close(f) => close_to_client(f),
        _ => return None,
    })
}

pub(super) fn to_upstream(m: ws::Message) -> Option<Message> {
    Some(match m {
        ws::Message::Text(t) => Message::Text(tung::Utf8Bytes::try_from(Bytes::from(t)).ok()?),
        ws::Message::Binary(b) => Message::Binary(b),
        ws::Message::Close(f) => Message::Close(f.map(|f| CloseFrame {
            code: f.code.into(),
            reason: tung::Utf8Bytes::try_from(Bytes::from(f.reason)).unwrap_or_default(),
        })),
        _ => return None,
    })
}
