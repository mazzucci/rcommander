//! Typed wrappers around the `POST /api/<name>` endpoints served by `rcmd web`.

use gloo_net::http::Request;
use rc_core::api::{
    ApiError, Conflicts, DiffRequest, DiffResponse, FileContent, FileMap, FileOp, FindRequest, FindResult,
    ForgetRequest, InitResponse, JobRequest, JobStatus, ListRequest, MapAnswer, MapQuestion, MapRequest, OpResult,
    OpenRequest, Preview, ReadRequest,
};
use rc_core::{Listing, Location, Settings};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::OnceLock;
use wasm_bindgen::JsValue;

const TOKEN_KEY: &str = "rc-token";
static TOKEN: OnceLock<String> = OnceLock::new();

/// Take the access token from `#token=…` (the URL the server opens), remember it in
/// sessionStorage so a reload keeps working, and strip it from the address bar.
pub fn init_token() {
    let Some(window) = web_sys::window() else { return };
    let storage = window.session_storage().ok().flatten();
    let location = window.location();
    let from_hash = location.hash().ok().and_then(|hash| {
        hash.trim_start_matches('#')
            .split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    });
    let token = match from_hash {
        Some(token) => {
            if let Some(storage) = &storage {
                let _ = storage.set_item(TOKEN_KEY, &token);
            }
            if let (Ok(path), Ok(search), Ok(history)) = (location.pathname(), location.search(), window.history()) {
                let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&format!("{path}{search}")));
            }
            Some(token)
        }
        None => storage.and_then(|s| s.get_item(TOKEN_KEY).ok().flatten()),
    };
    if let Some(token) = token {
        let _ = TOKEN.set(token);
    }
}

pub fn has_token() -> bool {
    TOKEN.get().is_some()
}

/// URL of the server-sent event stream. EventSource cannot send headers, so the token
/// goes in the query string (same origin, never leaves the machine).
pub fn events_url() -> String {
    let token = TOKEN.get().map_or("", String::as_str);
    format!("/api/events?token={}", js_sys::encode_uri_component(token))
}

/// POST `body` as JSON to `/api/<name>` and decode the reply. Errors are user-facing strings.
async fn call<Req: Serialize, Resp: DeserializeOwned>(name: &str, body: &Req) -> Result<Resp, String> {
    let resp = Request::post(&format!("/api/{name}"))
        .header("X-RC-Token", TOKEN.get().map_or("", String::as_str))
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| format!("Cannot reach rcmd: {e}"))?;
    if resp.ok() {
        resp.json()
            .await
            .map_err(|e| format!("Bad reply from /api/{name}: {e}"))
    } else {
        let status = resp.status();
        match resp.json::<ApiError>().await {
            Ok(err) => Err(err.error),
            Err(_) => Err(format!("/api/{name} failed: HTTP {status}")),
        }
    }
}

pub async fn init() -> Result<InitResponse, String> {
    call("init", &serde_json::json!({})).await
}

pub async fn list(location: Location) -> Result<Listing, String> {
    call("list", &ListRequest { location }).await
}

pub async fn read(location: Location) -> Result<FileContent, String> {
    call("read", &ReadRequest { location }).await
}

pub async fn diff(left: Location, right: Location) -> Result<DiffResponse, String> {
    call(
        "diff",
        &DiffRequest {
            left: Some(left),
            right: Some(right),
        },
    )
    .await
}

/// Quick View: the beginning of a file, or a folder summary.
pub async fn preview(location: Location) -> Result<Preview, String> {
    call("preview", &ReadRequest { location }).await
}

/// Diff where either side may be missing (shown as empty, "does not exist").
pub async fn diff_sides(left: Option<Location>, right: Option<Location>) -> Result<DiffResponse, String> {
    call("diff", &DiffRequest { left, right }).await
}

/// Forget entries of "Claude's changes"; the files stay.
pub async fn forget_changes(names: Vec<String>) -> Result<OpResult, String> {
    call("changes/forget", &ForgetRequest { names }).await
}

/// Diff a file against its committed version (git HEAD).
pub async fn diff_head(location: Location) -> Result<DiffResponse, String> {
    call("diff_head", &ReadRequest { location }).await
}

/// What a copy or move would find already at the destination.
pub async fn conflicts(op: &FileOp) -> Result<Conflicts, String> {
    call("conflicts", op).await
}

/// Run an operation; with a `job` id (letters, digits, `-`) [`job`] and [`cancel`]
/// can follow and stop it.
pub async fn op(op: FileOp, job: Option<String>) -> Result<OpResult, String> {
    match job {
        Some(job) => call(&format!("op?job={job}"), &op).await,
        None => call("op", &op).await,
    }
}

/// Find files; `job` lets [`job`] follow it and [`cancel`] stop it.
pub async fn find(req: FindRequest, job: String) -> Result<FindResult, String> {
    call(&format!("find?job={job}"), &req).await
}

/// Claude's map of a file or folder; `job` lets [`job`] follow it and [`cancel`] stop it.
pub async fn map(location: Location, refresh: bool, job: String) -> Result<FileMap, String> {
    call(&format!("map?job={job}"), &MapRequest { location, refresh }).await
}

/// A question to Claude about a map; `job` lets [`job`] follow it and [`cancel`] stop it.
pub async fn map_ask(question: MapQuestion, job: String) -> Result<MapAnswer, String> {
    call(&format!("map/ask?job={job}"), &question).await
}

/// Progress of a running operation.
pub async fn job(job: String) -> Result<JobStatus, String> {
    call("job", &JobRequest { job }).await
}

/// Ask a running operation to stop.
pub async fn cancel(job: String) -> Result<JobStatus, String> {
    call("cancel", &JobRequest { job }).await
}

/// Open a file in the configured editor (or the system default application).
pub async fn edit(location: Location) -> Result<(), String> {
    call::<_, serde_json::Value>("edit", &OpenRequest { location })
        .await
        .map(drop)
}

/// Persist the options; the server replies with what it saved.
pub async fn save_settings(settings: Settings) -> Result<Settings, String> {
    call("settings", &settings).await
}
