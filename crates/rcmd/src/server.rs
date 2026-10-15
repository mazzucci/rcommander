//! `rcmd web`: serves the WASM front end and a JSON API over the local filesystem.
//!
//! Security model: the server only listens on 127.0.0.1, rejects requests whose `Host`
//! header is not loopback (DNS-rebinding protection), and requires a random per-run token
//! (`X-RC-Token`) on every API call. The token is handed to the browser in the URL fragment,
//! which is never sent over the network.

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rc_core::api::{ApiError, DiffRequest, FileOp, InitResponse, ListRequest, OpenRequest, ReadRequest};
use rc_core::{Location, Settings};
use rc_vfs::Vfs;
use rust_embed::RustEmbed;
use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

/// Built front end (`trunk build` in crates/rc-web). Embedded in release builds, read from
/// disk in debug builds; missing entirely if the front end was never built.
#[derive(RustEmbed)]
#[folder = "../rc-web/dist"]
#[allow_missing = true]
struct Assets;

pub struct Config {
    pub port: u16,
    pub open_browser: bool,
    /// Application to open the URL with (e.g. "Google Chrome"); default browser if `None`.
    pub browser: Option<String>,
    pub left: Location,
    pub right: Location,
    pub settings: Settings,
    pub settings_path: Option<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    vfs: Arc<Vfs>,
    token: Arc<str>,
    init: Arc<InitResponse>,
    settings: Arc<RwLock<Settings>>,
    /// Where settings are saved; `None` keeps them in memory (tests).
    settings_path: Option<Arc<PathBuf>>,
}

pub fn run(vfs: Arc<Vfs>, config: Config) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(serve(vfs, config))
}

async fn serve(vfs: Arc<Vfs>, config: Config) -> Result<()> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", config.port)).await {
        Ok(l) => l,
        // Port taken (another instance?): fall back to any free port, and say so.
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("port {} is in use; picking a free one", config.port);
            tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?
        }
        Err(e) => return Err(e).with_context(|| format!("cannot listen on 127.0.0.1:{}", config.port)),
    };
    let addr = listener.local_addr()?;
    let token = random_token()?;
    let init = InitResponse {
        left: vfs.normalize(config.left, None),
        right: vfs.normalize(config.right, None),
        home: Vfs::home(),
        settings: Settings::default(),
    };
    let app = router(
        AppState {
            vfs,
            token: token.as_str().into(),
            init: Arc::new(init),
            settings: Arc::new(RwLock::new(config.settings)),
            settings_path: config.settings_path.map(Arc::new),
        },
        addr.port(),
    );

    let url = format!("http://{addr}/#token={token}");
    println!("rcommander is running at {url}\nPress Ctrl+C to stop.");
    if Assets::get("index.html").is_none() {
        eprintln!("warning: {NOT_BUILT_HINT}");
    }
    let mut launcher = None;
    if config.open_browser {
        // Open a private redirect page rather than the URL itself: browsers keep their
        // command line for life, and other local users could read the token there.
        let opened = launcher_page(&url).and_then(|page| {
            let target = page.to_string_lossy().into_owned();
            launcher = Some(page);
            Ok(match &config.browser {
                Some(app) => open::with_detached(&target, app),
                None => open::that_detached(&target),
            }?)
        });
        if let Err(e) = opened {
            eprintln!("could not open a browser ({e:#}); open the URL above manually");
        }
    }
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Some(page) = launcher {
        let _ = std::fs::remove_file(page);
    }
    Ok(served?)
}

/// Write a page that redirects to `url` into a directory only this user can read.
fn launcher_page(url: &str) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = base.join(format!("rcommander-{uid}"));
    std::fs::create_dir_all(&dir)?;
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid {
        anyhow::bail!("{} is not a directory owned by you", dir.display());
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let page = dir.join(format!("open-{}.html", std::process::id()));
    let _ = std::fs::remove_file(&page);
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>rcommander</title>\
         <meta http-equiv=refresh content=\"0;url={url}\"><script>location.replace({url:?})</script>"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&page)?;
    std::io::Write::write_all(&mut file, html.as_bytes())?;
    Ok(page)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn router(state: AppState, port: u16) -> Router {
    let api = Router::new()
        .route("/init", post(init))
        .route("/list", post(list))
        .route("/read", post(read))
        .route("/diff", post(diff))
        .route("/op", post(op))
        .route("/open", post(open_external))
        .route("/edit", post(edit_external))
        .route("/settings", post(save_settings))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .nest("/api", api)
        .fallback(static_file)
        .layer(middleware::from_fn(move |req, next| {
            require_loopback_host(port, req, next)
        }))
        .with_state(state)
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("cannot read /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Reject requests not addressed to us by a loopback name (DNS rebinding), and add
/// defensive headers to every response.
async fn require_loopback_host(port: u16, req: Request, next: Next) -> Response {
    let mut hosts = req.headers().get_all(header::HOST).iter();
    let host = hosts.next().and_then(|h| h.to_str().ok()).unwrap_or("");
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if hosts.next().is_some() || !allowed.iter().any(|a| a == host) {
        return api_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, header::HeaderValue::from_static("DENY"));
    resp
}

/// Compare without an early exit, so timing reveals nothing about the token.
fn same_token(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_token(State(state): State<AppState>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let given = headers.get("x-rc-token").map_or(&[][..], |v| v.as_bytes());
    let ok = same_token(given, state.token.as_bytes());
    if !ok {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "missing or invalid token; reopen the URL printed by `rcmd web`",
        );
    }
    next.run(req).await
}

fn api_error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ApiError { error: msg.into() })).into_response()
}

/// Run blocking filesystem work off the async runtime and turn errors into JSON.
async fn blocking<T, F>(f: F) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => api_error(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn init(State(s): State<AppState>) -> Json<InitResponse> {
    let mut init = (*s.init).clone();
    init.settings = s.settings.read().expect("settings lock").clone();
    Json(init)
}

async fn save_settings(State(s): State<AppState>, Json(settings): Json<Settings>) -> Response {
    blocking(move || {
        if let Some(path) = &s.settings_path {
            rc_vfs::settings::save_to(path, &settings)?;
        }
        *s.settings.write().expect("settings lock") = settings.clone();
        Ok(settings)
    })
    .await
}

async fn edit_external(State(s): State<AppState>, Json(req): Json<OpenRequest>) -> Response {
    blocking(move || {
        let editor = s.settings.read().expect("settings lock").editor.clone();
        s.vfs.edit_external(&s.vfs.normalize(req.location, None), &editor)?;
        Ok(serde_json::json!({}))
    })
    .await
}

async fn list(State(s): State<AppState>, Json(req): Json<ListRequest>) -> Response {
    blocking(move || s.vfs.list(&s.vfs.normalize(req.location, None))).await
}

async fn read(State(s): State<AppState>, Json(req): Json<ReadRequest>) -> Response {
    blocking(move || s.vfs.read_content(&s.vfs.normalize(req.location, None))).await
}

async fn diff(State(s): State<AppState>, Json(req): Json<DiffRequest>) -> Response {
    blocking(move || {
        let norm = |l: Option<Location>| l.map(|l| s.vfs.normalize(l, None));
        let (left, right) = (norm(req.left), norm(req.right));
        s.vfs.diff(left.as_ref(), right.as_ref())
    })
    .await
}

async fn op(State(s): State<AppState>, Json(op): Json<FileOp>) -> Response {
    blocking(move || {
        let n = |l: Location| s.vfs.normalize(l, None);
        let all = |v: Vec<Location>| v.into_iter().map(n).collect();
        let op = match op {
            FileOp::Copy {
                sources,
                dest,
                overwrite,
            } => FileOp::Copy {
                sources: all(sources),
                dest: n(dest),
                overwrite,
            },
            FileOp::Move {
                sources,
                dest,
                overwrite,
            } => FileOp::Move {
                sources: all(sources),
                dest: n(dest),
                overwrite,
            },
            FileOp::Delete { targets, permanent } => FileOp::Delete {
                targets: all(targets),
                permanent,
            },
            FileOp::Mkdir { location } => FileOp::Mkdir { location: n(location) },
            FileOp::Rename { from, to } => FileOp::Rename {
                from: n(from),
                to: n(to),
            },
        };
        Ok(s.vfs.op(&op, &mut |_| {}))
    })
    .await
}

async fn open_external(State(s): State<AppState>, Json(req): Json<OpenRequest>) -> Response {
    blocking(move || {
        s.vfs.open_external(&s.vfs.normalize(req.location, None))?;
        Ok(serde_json::json!({}))
    })
    .await
}

async fn static_file(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(file) => (
            [(header::CONTENT_TYPE, file.metadata.mimetype().to_string())],
            file.data,
        )
            .into_response(),
        None if path == "index.html" => Html(format!(
            "<!doctype html><title>rcommander</title><body style=\"font-family:system-ui;padding:2em\">\
             <h1>rcommander</h1><p>{NOT_BUILT_HINT}</p>"
        ))
        .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Debug builds read the front end from disk on each request; release builds embed it
/// when `rcmd` is compiled.
#[cfg(debug_assertions)]
const NOT_BUILT_HINT: &str =
    "the web front end is not built: run `trunk build --release` in crates/rc-web, then reload";
#[cfg(not(debug_assertions))]
const NOT_BUILT_HINT: &str = "this rcmd was compiled without the web front end: run `trunk build --release` in \
crates/rc-web, then `cargo build --release -p rcmd`";

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const PORT: u16 = 7878;

    fn app(dir: &std::path::Path) -> Router {
        let loc = Location::local(dir.to_string_lossy());
        let state = AppState {
            vfs: Arc::new(Vfs::new()),
            token: "secret".into(),
            init: Arc::new(InitResponse {
                left: loc.clone(),
                right: loc,
                home: "/".into(),
                settings: Settings::default(),
            }),
            settings: Arc::new(RwLock::new(Settings::default())),
            settings_path: None,
        };
        router(state, PORT)
    }

    fn post(path: &str, body: serde_json::Value, token: Option<&str>, host: &str) -> Request {
        let mut b = Request::post(path)
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = token {
            b = b.header("x-rc-token", t);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    async fn call(app: Router, req: Request) -> (StatusCode, serde_json::Value) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn rejects_missing_token_and_foreign_host() {
        let tmp = tempfile::tempdir().unwrap();
        let host = format!("127.0.0.1:{PORT}");
        let (status, body) = call(app(tmp.path()), post("/api/init", serde_json::json!({}), None, &host)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"].as_str().unwrap().contains("token"));
        let (status, _) = call(
            app(tmp.path()),
            post("/api/init", serde_json::json!({}), Some("secret"), "evil.com:7878"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn list_op_and_diff_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "1\n2\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "1\n3\n").unwrap();
        let dir = tmp.path().to_string_lossy().to_string();
        let host = format!("localhost:{PORT}");
        let loc = |p: &str| serde_json::json!({ "kind": "local", "path": p });

        let mkdir = serde_json::json!({ "op": "mkdir", "location": loc(&format!("{dir}/new")) });
        let (status, body) = call(app(tmp.path()), post("/api/op", mkdir, Some("secret"), &host)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["done"], 1);

        let req = serde_json::json!({ "location": loc(&format!("{dir}/new/..")) });
        let (status, body) = call(app(tmp.path()), post("/api/list", req, Some("secret"), &host)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["location"]["path"], dir.as_str(), "location comes back normalized");
        assert_eq!(body["entries"].as_array().unwrap().len(), 3);

        let req = serde_json::json!({ "left": loc(&format!("{dir}/a.txt")), "right": loc(&format!("{dir}/b.txt")) });
        let (status, body) = call(app(tmp.path()), post("/api/diff", req, Some("secret"), &host)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["hunks"].as_array().unwrap().len(), 1);

        let req = serde_json::json!({ "location": loc("/does/not/exist") });
        let (status, body) = call(app(tmp.path()), post("/api/list", req, Some("secret"), &host)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn rejects_bad_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let ok_host = format!("127.0.0.1:{PORT}");
        let req = |method: &str, path: &str, hosts: &[&str], token: Option<&str>, ctype: &str| {
            let mut b = Request::builder().method(method).uri(path);
            for h in hosts {
                b = b.header(header::HOST, *h);
            }
            if let Some(t) = token {
                b = b.header("x-rc-token", t);
            }
            b.header(header::CONTENT_TYPE, ctype).body(Body::from("{}")).unwrap()
        };
        let json = "application/json";
        let cases = [
            (
                req("POST", "/api/init", &[&ok_host], Some("wrong"), json),
                StatusCode::UNAUTHORIZED,
            ),
            (
                req("POST", "/api/init", &[&ok_host], Some("secre"), json),
                StatusCode::UNAUTHORIZED,
            ),
            (
                req("POST", "/api/init", &[], Some("secret"), json),
                StatusCode::FORBIDDEN,
            ),
            (
                req("POST", "/api/init", &[&ok_host, "evil.com"], Some("secret"), json),
                StatusCode::FORBIDDEN,
            ),
            (
                req("POST", "/api/init", &["evil.localhost:7878"], Some("secret"), json),
                StatusCode::FORBIDDEN,
            ),
            (
                req("GET", "/api/init", &[&ok_host], Some("secret"), json),
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                req("POST", "/api/list", &[&ok_host], Some("secret"), "text/plain"),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                req("GET", "/../Cargo.toml", &[&ok_host], None, json),
                StatusCode::NOT_FOUND,
            ),
        ];
        for (i, (request, expected)) in cases.into_iter().enumerate() {
            let resp = app(tmp.path()).oneshot(request).await.unwrap();
            assert_eq!(resp.status(), expected, "case {i}");
        }
    }

    #[test]
    fn launcher_page_is_private_and_redirects() {
        use std::os::unix::fs::PermissionsExt;
        let page = launcher_page("http://127.0.0.1:1/#token=abc").unwrap();
        let mode = std::fs::metadata(&page).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(std::fs::read_to_string(&page).unwrap().contains("token=abc"));
        std::fs::remove_file(page).unwrap();
    }

    #[tokio::test]
    async fn responses_carry_defensive_headers() {
        let tmp = tempfile::tempdir().unwrap();
        let req = Request::get("/")
            .header(header::HOST, format!("127.0.0.1:{PORT}"))
            .body(Body::empty())
            .unwrap();
        let resp = app(tmp.path()).oneshot(req).await.unwrap();
        assert_eq!(resp.headers()[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn settings_round_trip_through_init() {
        let tmp = tempfile::tempdir().unwrap();
        let host = format!("127.0.0.1:{PORT}");
        let router = app(tmp.path());
        let body = serde_json::json!({ "columns": ["permissions"], "show_hidden": false, "editor": "code" });
        let (status, saved) = call(router.clone(), post("/api/settings", body, Some("secret"), &host)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["editor"], "code");
        let (_, init) = call(router, post("/api/init", serde_json::json!({}), Some("secret"), &host)).await;
        assert_eq!(init["settings"]["columns"], serde_json::json!(["permissions"]));
        assert_eq!(init["settings"]["show_hidden"], false);
    }

    #[tokio::test]
    async fn serves_index_without_token() {
        let tmp = tempfile::tempdir().unwrap();
        let req = Request::get("/")
            .header(header::HOST, format!("127.0.0.1:{PORT}"))
            .body(Body::empty())
            .unwrap();
        let resp = app(tmp.path()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    }
}
