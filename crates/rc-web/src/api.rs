//! Typed wrappers around the `POST /api/<name>` endpoints served by `rcmd web`.

use gloo_net::http::Request;
use rc_core::api::{
    ApiError, DiffRequest, DiffResponse, FileContent, FileOp, InitResponse, ListRequest, OpResult, OpenRequest,
    ReadRequest,
};
use rc_core::{Listing, Location};
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

pub async fn op(op: FileOp) -> Result<OpResult, String> {
    call("op", &op).await
}

pub async fn open(location: Location) -> Result<(), String> {
    call::<_, serde_json::Value>("open", &OpenRequest { location })
        .await
        .map(drop)
}
