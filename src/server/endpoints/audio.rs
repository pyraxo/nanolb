use super::forward::{Call, JSON, pass};
use super::{Req, bad_request, prelude, require_model};
use crate::app::{App, Key, Provider};
use crate::server::error::ApiError;
use crate::store::Endpoint;
use crate::time::now_ms;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Multipart, State};
use axum::response::Response;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

const BACKEND_MODEL: &str = "transcribe";

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

fn check_model<'a>(form: &'a Fields, key: &Key) -> Result<&'a str, ApiError> {
    let model = form
        .text
        .get("model")
        .map(String::as_str)
        .ok_or_else(|| bad_request("model is required"))?;
    require_model(key, model)?;
    Ok(model)
}

async fn transcribe(app: Arc<App>, key: Arc<Key>, mp: Multipart, v1: bool) -> Result<Response, ApiError> {
    let started = now_ms();
    prelude(&key)?;
    let form = read_form(mp).await?;
    let model = if v1 { check_model(&form, &key)? } else { BACKEND_MODEL };
    let Some(file) = form.files.iter().find(|f| f.name == "file") else {
        return Err(bad_request("file is required"));
    };
    let prompt = form.text.get("prompt");
    let content_type = file.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());
    let req = Req::new(&app, &key, Endpoint::Transcribe, model, started);
    let call = Call::post(Provider::Codex, "/transcribe", JSON);
    let shape = |rb: reqwest::RequestBuilder| {
        let mut f = Form::new().part("file", part(&file.data, &file.file_name, &content_type));
        if let Some(p) = prompt {
            f = f.text("prompt", p.clone());
        }
        rb.multipart(f)
    };
    let sent = req.send(&call, &shape).await?;
    req.record(&sent.acc.id, sent.res.status().as_u16(), &Value::Null, sent.ttfb);
    Ok(pass(sent))
}

pub(super) async fn transcribe_backend(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    mp: Multipart,
) -> Result<Response, ApiError> {
    transcribe(app, key, mp, false).await
}

pub(super) async fn transcribe_v1(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    mp: Multipart,
) -> Result<Response, ApiError> {
    transcribe(app, key, mp, true).await
}
