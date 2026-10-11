#![warn(clippy::pedantic)]

mod archive;
mod canvas;
mod minimap;
mod offset_data;
mod screenshot;

use archive::Archive;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query},
    http::{header, HeaderValue, Method, Response, StatusCode},
    response::{AppendHeaders, IntoResponse},
    routing::{get, post},
    Extension, Json, Router,
};
use bytes::Bytes;
use lazy_static::lazy_static;
use regex::Regex;
use reqwest::{redirect::Policy, Client};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{fs, sync::RwLock};
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

const CACHE_EXPIRY: Duration = Duration::from_secs(15 * 60); // 15 minutes
/// `/capture` refetches anything older than this, so a scheduled capture always checks
/// upstream instead of archiving whatever a visitor happened to leave in the cache.
const CAPTURE_MAX_AGE: Duration = Duration::from_secs(5 * 60);
const MAX_FILES_IN_RAM_CACHE: usize = 25;
/// largest screenshot scene accepted, in bytes
const MAX_SCREENSHOT_SCENE: usize = 8 * 1024 * 1024;
/// most towns a minimap URL may list
const MAX_MINIMAP_TOWNS: usize = 2000;
lazy_static! {
    static ref SERVER_REGEX: Regex = Regex::new(r"^[a-zA-Z]{2}\d{1,3}$").unwrap();
    /// `1868-3135-3503.png`: the town ids to highlight
    static ref MINIMAP_REGEX: Regex = Regex::new(r"^\d{1,9}(-\d{1,9})*\.png$").unwrap();
    /// `0123456789abcdef.png`, see `screenshot::id`
    static ref SCREENSHOT_REGEX: Regex = Regex::new(r"^[0-9a-f]{16}\.png$").unwrap();
    static ref DATAFILE_WHITELIST: Vec<&'static str> =
        vec!["players.txt", "towns.txt", "alliances.txt", "islands.txt"];
}

#[tokio::main]
async fn main() {
    // Initialize the tracing subscriber
    tracing_subscriber::fmt::init();
    // let subscriber = tracing_subscriber::fmt().json().finish();
    // tracing::subscriber::set_global_default(subscriber).unwrap();

    // Initialize the cache and HTTP client
    let app_state = Arc::new(AppState::new().await);

    // Build our application with a route
    let app = Router::new()
        .route(
            "/screenshots",
            post(handle_screenshot_upload).layer(DefaultBodyLimit::max(MAX_SCREENSHOT_SCENE)),
        )
        .route("/screenshots/{file}", get(handle_screenshot))
        .route("/{server}/history", get(handle_history))
        .route("/{server}/capture", get(handle_capture))
        .route("/{server}/minimap/{towns}", get(handle_minimap))
        .route("/{server}/{datafile}", get(handle_request))
        .layer(
            CorsLayer::new()
                .allow_origin(
                    "https://map.grasstouchers.gg"
                        .parse::<HeaderValue>()
                        .unwrap(),
                )
                .allow_methods([Method::GET, Method::POST]),
        )
        .layer(Extension(app_state));

    // run our app with hyper, listening globally on port 3000
    let listen_address = "0.0.0.0:3000";
    info!("listening on {listen_address}");
    let listener = tokio::net::TcpListener::bind(listen_address)
        .await
        .expect("Failed to build tokio TCP listener");
    axum::serve(listener, app)
        .await
        .expect("Failed to start axum server");
}

struct AppState {
    cache: RwLock<HashMap<String, CacheEntry>>,
    failed_cache: RwLock<HashMap<String, Instant>>,
    client: Client,
    cache_dir: PathBuf,
    /// rendered screenshots, `ID.png`
    screenshot_dir: PathBuf,
    archive: Option<Archive>,
}

impl AppState {
    async fn new() -> Self {
        // Create the HTTP client with custom headers
        let client = Client::builder()
            .user_agent("YourCustomUserAgent")
            .gzip(true)
            .deflate(true)
            .redirect(Policy::none())
            .build()
            .expect("Failed to build reqwest client");
        // Set up the cache directory
        let cache_dir = "./cache".into();
        fs::create_dir_all(&cache_dir).await.unwrap();
        let archive = std::env::var_os("SNAPSHOT_DIR").map(|dir| {
            info!("archiving snapshots to {}", dir.to_string_lossy());
            Archive::new(dir.into())
        });
        if archive.is_none() {
            info!("SNAPSHOT_DIR not set, snapshot archiving disabled");
        }
        // next to the archive so screenshots outlive the container, see `SNAPSHOT_DIR`
        let screenshot_dir: PathBuf = std::env::var_os("SNAPSHOT_DIR").map_or_else(
            || "./screenshots".into(),
            |dir| PathBuf::from(dir).join("screenshots"),
        );
        fs::create_dir_all(&screenshot_dir).await.unwrap();
        Self {
            cache: RwLock::new(HashMap::new()),
            failed_cache: RwLock::new(HashMap::new()),
            client,
            cache_dir,
            screenshot_dir,
            archive,
        }
    }
}

struct CacheEntry {
    data: Bytes,
    timestamp: Instant,
    /// upstream `Last-Modified` in unix seconds, 0 if unknown
    last_modified: u64,
}

#[derive(Deserialize)]
struct SnapshotQuery {
    at: Option<u64>,
}

#[derive(Serialize)]
struct History {
    server: String,
    snapshots: Vec<u64>,
}

async fn handle_request(
    Path((server, datafile)): Path<(String, String)>,
    Query(query): Query<SnapshotQuery>,
    Extension(state): Extension<Arc<AppState>>,
) -> Response<Body> {
    // Validate the server parameter
    if !SERVER_REGEX.is_match(&server) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Validate the datafile parameter
    if !DATAFILE_WHITELIST.contains(&datafile.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }

    if let Some(at) = query.at {
        return snapshot_at(&state, &server, &datafile, at).await;
    }
    match get_live(&state, &server, &datafile, CACHE_EXPIRY).await {
        Ok((data, last_modified)) => {
            (StatusCode::OK, last_modified_header(last_modified), data).into_response()
        }
        Err(status) => status.into_response(),
    }
}

async fn handle_history(
    Path(server): Path<String>,
    Extension(state): Extension<Arc<AppState>>,
) -> Response<Body> {
    if !SERVER_REGEX.is_match(&server) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(archive) = &state.archive else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let snapshots = archive.timeline(&server).await;
    Json(History { server, snapshots }).into_response()
}

async fn handle_capture(
    Path(server): Path<String>,
    Extension(state): Extension<Arc<AppState>>,
) -> StatusCode {
    if !SERVER_REGEX.is_match(&server) {
        return StatusCode::NOT_FOUND;
    }
    let results = tokio::join!(
        get_live(&state, &server, "players.txt", CAPTURE_MAX_AGE),
        get_live(&state, &server, "alliances.txt", CAPTURE_MAX_AGE),
        get_live(&state, &server, "towns.txt", CAPTURE_MAX_AGE),
        get_live(&state, &server, "islands.txt", CAPTURE_MAX_AGE),
    );
    if [results.0, results.1, results.2, results.3]
        .iter()
        .all(Result::is_ok)
    {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::BAD_GATEWAY
    }
}

/// Render a screenshot scene (see `screenshot`) and store it as `ID.png`. Answers with the id,
/// which the map has already worked out and copied as a link.
async fn handle_screenshot_upload(
    Extension(state): Extension<Arc<AppState>>,
    scene: Bytes,
) -> Response<Body> {
    let id = screenshot::id(&scene);
    let path = state.screenshot_dir.join(format!("{id}.png"));
    if fs::metadata(&path).await.is_ok() {
        return (StatusCode::OK, id).into_response();
    }
    let Ok(scene) = String::from_utf8(scene.to_vec()) else {
        return (StatusCode::BAD_REQUEST, "scene isn't UTF-8").into_response();
    };
    let png = match tokio::task::spawn_blocking(move || screenshot::render(&scene)).await {
        Ok(Ok(png)) => png,
        Ok(Err(reason)) => {
            warn!(result = "fail", reason = "bad screenshot scene", %reason);
            return (StatusCode::BAD_REQUEST, reason).into_response();
        }
        Err(err) => {
            warn!(result = "fail", reason = "screenshot render panicked", %err);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if let Err(err) = fs::write(&path, &png).await {
        warn!(result = "fail", reason = "screenshot write", %err, id);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    info!(
        result = "success",
        reason = "screenshot stored",
        id,
        bytes = png.len()
    );
    (StatusCode::CREATED, id).into_response()
}

async fn handle_screenshot(
    Path(file): Path<String>,
    Extension(state): Extension<Arc<AppState>>,
) -> Response<Body> {
    if !SCREENSHOT_REGEX.is_match(&file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match fs::read(state.screenshot_dir.join(&file)).await {
        Ok(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                // the id is the scene's hash, so a stored screenshot never changes
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            png,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A PNG minimap of the current data zoomed to the given towns, see `minimap::render`.
async fn handle_minimap(
    Path((server, towns)): Path<(String, String)>,
    Extension(state): Extension<Arc<AppState>>,
) -> Response<Body> {
    if !SERVER_REGEX.is_match(&server) || !MINIMAP_REGEX.is_match(&towns) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let selected: HashSet<u32> = towns
        .trim_end_matches(".png")
        .split('-')
        .filter_map(|id| id.parse().ok())
        .collect();
    if selected.len() > MAX_MINIMAP_TOWNS {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let (towns_txt, islands_txt) = tokio::join!(
        get_live(&state, &server, "towns.txt", CACHE_EXPIRY),
        get_live(&state, &server, "islands.txt", CACHE_EXPIRY),
    );
    let (Ok((towns_txt, _)), Ok((islands_txt, _))) = (towns_txt, islands_txt) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let towns_txt = String::from_utf8_lossy(&towns_txt);
    let islands_txt = String::from_utf8_lossy(&islands_txt);
    match minimap::render(&towns_txt, &islands_txt, &selected) {
        Some(png) => {
            info!(
                result = "success",
                reason = "minimap",
                server,
                towns = selected.len()
            );
            (
                [
                    (header::CONTENT_TYPE, "image/png"),
                    // towns rarely move, and forums may fetch the image on every view
                    (header::CACHE_CONTROL, "public, max-age=3600"),
                ],
                png,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Serve an archived version as-is; the stored gzip becomes the response's content encoding.
async fn snapshot_at(state: &AppState, server: &str, datafile: &str, at: u64) -> Response<Body> {
    let Some(archive) = &state.archive else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(version) = archive.at(server, datafile, at).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match archive.read_gz(&version).await {
        Ok(gz) => (
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CONTENT_ENCODING, "gzip"),
            ],
            last_modified_header(version.lm),
            gz,
        )
            .into_response(),
        Err(err) => {
            warn!(result = "fail", reason = "archive read", %err, server, datafile);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_live(
    state: &AppState,
    server: &str,
    datafile: &str,
    max_age: Duration,
) -> Result<(Bytes, u64), StatusCode> {
    let cache_key = format!("{server}/{datafile}");

    // Check if there is a cached failure
    if let Some(failed_response) = get_from_failed_cache(state, &cache_key).await {
        if failed_response.elapsed() < CACHE_EXPIRY {
            info!(result = "fail", reason = "cache", server, datafile);
            return Err(StatusCode::BAD_GATEWAY);
        }
    }

    // Check if response is cached in RAM
    if let Some(cached) = get_from_ram_cache(state, &cache_key, max_age).await {
        info!(result = "success", reason = "ram cache", server, datafile);
        return Ok(cached);
    }
    // Check if response is cached on disk
    if let Some((data, last_modified)) = get_from_disk_cache(state, &cache_key, max_age).await {
        info!(result = "success", reason = "file cache", server, datafile);
        update_ram_cache(state, &cache_key, &data, last_modified).await;
        return Ok((data, last_modified));
    }
    // Fetch from the external API
    let Some((data, last_modified)) = fetch_upstream(state, server, datafile).await else {
        update_failed_cache(state, &cache_key).await;
        info!(result = "fail", reason = "upstream", server, datafile);
        return Err(StatusCode::BAD_GATEWAY);
    };
    let (data, last_modified) =
        archive_and_pick_newest(state, server, datafile, last_modified, data).await;
    let (data, last_modified) = keep_newer_cached(state, &cache_key, data, last_modified).await;
    update_disk_cache(state, &cache_key, &data, last_modified).await;
    update_ram_cache(state, &cache_key, &data, last_modified).await;
    info!(result = "success", reason = "upstream", server, datafile);
    Ok((data, last_modified))
}

/// `Last-Modified` for a copy upstream generated at `last_modified` (unix seconds), if known.
fn last_modified_header(last_modified: u64) -> AppendHeaders<Option<(header::HeaderName, String)>> {
    AppendHeaders((last_modified > 0).then(|| {
        (
            header::LAST_MODIFIED,
            httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(last_modified)),
        )
    }))
}

/// Upstream round-robins between nodes that regenerate at different times, so a fetch can
/// return an older copy than one already archived. Never hand that older copy out.
async fn archive_and_pick_newest(
    state: &AppState,
    server: &str,
    datafile: &str,
    last_modified: u64,
    data: Bytes,
) -> (Bytes, u64) {
    let Some(archive) = &state.archive else {
        return (data, last_modified);
    };
    match archive.record(server, datafile, last_modified, &data).await {
        Ok(outcome) => info!(archive = ?outcome, last_modified, server, datafile),
        Err(err) => warn!(archive = "error", %err, server, datafile),
    }
    if let Some(newest) = archive.newest(server, datafile).await {
        if newest.lm > last_modified {
            match archive.read_plain(&newest).await {
                Ok(newer) => return (newer, newest.lm),
                Err(err) => warn!(archive = "error", %err, server, datafile),
            }
        }
    }
    (data, last_modified)
}

/// The archive keeps only one version per 6-hour slot, so it can't stop a lagging node's copy
/// from replacing a newer one fetched earlier in the slot. The RAM cache still holds what this
/// instance served last, even once expired; keep serving that if it's newer.
async fn keep_newer_cached(
    state: &AppState,
    cache_key: &str,
    data: Bytes,
    last_modified: u64,
) -> (Bytes, u64) {
    let cache = state.cache.read().await;
    match cache.get(cache_key) {
        Some(entry) if entry.last_modified > last_modified => {
            (entry.data.clone(), entry.last_modified)
        }
        _ => (data, last_modified),
    }
}

async fn get_from_ram_cache(
    state: &AppState,
    cache_key: &str,
    max_age: Duration,
) -> Option<(Bytes, u64)> {
    let cache = state.cache.read().await;
    if let Some(entry) = cache.get(cache_key) {
        if entry.timestamp.elapsed() < max_age {
            // Cache hit
            return Some((entry.data.clone(), entry.last_modified));
        }
    }
    None
}

async fn get_from_disk_cache(
    state: &AppState,
    cache_key: &str,
    max_age: Duration,
) -> Option<(Bytes, u64)> {
    let cache_path = state.cache_dir.join(cache_key);
    if let Ok(metadata) = fs::metadata(&cache_path).await {
        if metadata.is_file() {
            if let Ok(modified) = metadata.modified() {
                if let Ok(elapsed) = modified.elapsed() {
                    if elapsed < max_age {
                        if let Ok(data) = fs::read(&cache_path).await {
                            let last_modified = fs::read_to_string(last_modified_path(&cache_path))
                                .await
                                .ok()
                                .and_then(|text| text.trim().parse().ok())
                                .unwrap_or(0);
                            return Some((Bytes::from(data), last_modified));
                        }
                    }
                }
            }
        }
    }
    None
}

async fn get_from_failed_cache(state: &AppState, cache_key: &str) -> Option<Instant> {
    let cache = state.failed_cache.read().await;
    cache.get(cache_key).copied()
}

async fn update_failed_cache(state: &AppState, cache_key: &str) {
    let mut cache = state.failed_cache.write().await;
    cache.insert(cache_key.to_string(), Instant::now());
}

/// Returns the body and upstream's `Last-Modified` in unix seconds (now, if absent).
async fn fetch_upstream(state: &AppState, server: &str, datafile: &str) -> Option<(Bytes, u64)> {
    let url = format!("https://{server}.grepolis.com/data/{datafile}");

    // Perform the HTTP GET request with custom headers
    let response = state.client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let last_modified = response
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .unwrap_or_else(SystemTime::now)
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs());
    let data = response.bytes().await.ok()?;
    Some((data, last_modified))
}

async fn update_ram_cache(state: &AppState, cache_key: &str, data: &Bytes, last_modified: u64) {
    let mut cache = state.cache.write().await;
    // If the cache exceeds MAX_FILES_IN_RAM_CACHE, remove the least recently used entry
    if cache.len() >= MAX_FILES_IN_RAM_CACHE {
        // Simple LRU implementation
        if let Some(oldest_key) = cache
            .iter()
            .min_by_key(|entry| entry.1.timestamp)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest_key);
        }
    }
    // Insert the new entry
    cache.insert(
        cache_key.to_string(),
        CacheEntry {
            data: data.clone(),
            timestamp: Instant::now(),
            last_modified,
        },
    );
}

async fn update_disk_cache(state: &AppState, cache_key: &str, data: &Bytes, last_modified: u64) {
    let cache_path = state.cache_dir.join(cache_key);
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).await.ok();
    }
    fs::write(last_modified_path(&cache_path), last_modified.to_string())
        .await
        .ok();
    fs::write(cache_path, data).await.ok();
}

/// The disk cache keeps each file's `Last-Modified` next to it, in `FILE.last-modified`.
fn last_modified_path(cache_path: &std::path::Path) -> PathBuf {
    let mut path = cache_path.as_os_str().to_owned();
    path.push(".last-modified");
    path.into()
}
