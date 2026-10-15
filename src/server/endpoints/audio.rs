use super::forward::{Call, JSON, forward, pass};
use super::{Req, bad_request, prelude, require_model};
use crate::app::{App, Provider};
use crate::server::error::ApiError;
use crate::store::Endpoint;
use crate::time::now_ms;
use axum::body::Bytes;
use axum::extract::{Multipart, State};
use axum::http::HeaderMap;
use axum::response::Response;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

const TRANSCRIBE_MODEL: &str = "gpt-4o-transcribe";

pub(super) struct Upload {
    pub(super) name: String,
    pub(super) file_name: String,
    pub(super) content_type: Option<String>,
    pub(super) data: Bytes,
}

#[derive(Default)]
pub(super) struct Fields {
    pub(super) text: HashMap<String, String>,
    pub(super) files: Vec<Upload>,
}

pub(super) async fn read_form(mut mp: Multipart) -> Result<Fields, ApiError> {
    let invalid = |e: axum::extract::multipart::MultipartError| bad_request(format!("invalid multipart body: {e}"));
    let mut out = Fields::default();
    while let Some(f) = mp.next_field().await.map_err(invalid)? {
        let name = f.name().unwrap_or_default().to_owned();
        let file_name = f.file_name().map(str::to_owned);
        let content_type = f.content_type().map(str::to_owned);
        let data = f.bytes().await.map_err(invalid)?;
        match file_name {
            Some(file_name) => out.files.push(Upload {
                name,
                file_name,
                content_type,
                data,
            }),
            None => {
                out.text.insert(name, String::from_utf8_lossy(&data).into_owned());
            }
        }
    }
    Ok(out)
}

fn part(data: &Bytes, file_name: &str, content_type: &str) -> Part {
    let make = || Part::stream_with_length(reqwest::Body::from(data.clone()), data.len() as u64).file_name(file_name.to_owned());
    make().mime_str(content_type).unwrap_or_else(|_| make())
}

fn check_model(form: &Fields, key: &crate::app::Key) -> Result<(), ApiError> {
    match form.text.get("model").map(String::as_str) {
        None => Err(bad_request("model is required")),
        Some(m) if m != TRANSCRIBE_MODEL => Err(bad_request(format!("model {m} is not supported, use {TRANSCRIBE_MODEL}"))),
        Some(m) => require_model(key, m),
    }
}

async fn transcribe(app: Arc<App>, h: HeaderMap, mp: Multipart, v1: bool) -> Result<Response, ApiError> {
    let started = now_ms();
    let key = prelude(&app, &h)?;
    let form = read_form(mp).await?;
    if v1 {
        check_model(&form, &key)?;
    }
    let Some(file) = form.files.iter().find(|f| f.name == "file") else {
        return Err(bad_request("file is required"));
    };
    let prompt = form.text.get("prompt");
    let content_type = file.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());
    let req = Req {
        app: &app,
        key: &key,
        endpoint: Endpoint::Transcribe,
        model: TRANSCRIBE_MODEL,
        started,
    };
    let call = Call::post(Provider::Codex, "/transcribe", JSON);
    let shape = |rb: reqwest::RequestBuilder| {
        let mut f = Form::new().part("file", part(&file.data, &file.file_name, &content_type));
        if let Some(p) = prompt {
            f = f.text("prompt", p.clone());
        }
        rb.multipart(f)
    };
    let sent = forward(&app, &key, &call, &shape)
        .await
        .map_err(|e| req.failed(Provider::Codex, e))?;
    req.record(&sent.acc.id, sent.res.status().as_u16(), &Value::Null, sent.ttfb);
    Ok(pass(sent))
}

pub(super) async fn transcribe_backend(State(app): State<Arc<App>>, h: HeaderMap, mp: Multipart) -> Result<Response, ApiError> {
    transcribe(app, h, mp, false).await
}

pub(super) async fn transcribe_v1(State(app): State<Arc<App>>, h: HeaderMap, mp: Multipart) -> Result<Response, ApiError> {
    transcribe(app, h, mp, true).await
}
