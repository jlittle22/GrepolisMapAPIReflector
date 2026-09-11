#![warn(clippy::pedantic)]

use axum::{
    body::Body,
    extract::Path,
    http::{HeaderValue, Method, Response, StatusCode},
    response::IntoResponse,
    routing::get,
    Extension, Router,
};
use bytes::Bytes;
use lazy_static::lazy_static;
use regex::Regex;
use reqwest::{redirect::Policy, Client};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, sync::RwLock};
use tower_http::cors::CorsLayer;
use tracing::info;

const CACHE_EXPIRY: Duration = Duration::from_secs(15 * 60); // 15 minutes
const MAX_FILES_IN_RAM_CACHE: usize = 25;
lazy_static! {
    static ref SERVER_REGEX: Regex = Regex::new(r"^[a-zA-Z]{2}\d{1,3}$").unwrap();
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
        .route("/{server}/{datafile}", get(handle_request))
        .layer(
            CorsLayer::new()
                .allow_origin("https://map.grasstouchers.gg".parse::<HeaderValue>().unwrap())
                .allow_methods([Method::GET]),
        )
        .layer(Extension(app_state));

    // run our app with hyper, listening globally on port 3000
    // Cloud Run's default (gVisor) sandbox doesn't support binding the IPv6
    // wildcard address, so bind IPv4-only.
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
        Self {
            cache: RwLock::new(HashMap::new()),
            failed_cache: RwLock::new(HashMap::new()),
            client,
            cache_dir,
        }
    }
}

struct CacheEntry {
    data: Bytes,
    timestamp: Instant,
}

async fn handle_request(
    Path((server, datafile)): Path<(String, String)>,
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

    let cache_key = format!("{server}/{datafile}");

    // Check if there is a cached failure
    if let Some(failed_response) = get_from_failed_cache(&state, &cache_key).await {
        if failed_response.elapsed() < CACHE_EXPIRY {
            info!(result = "fail", reason = "cache", server, datafile);
            return StatusCode::BAD_GATEWAY.into_response();
        }
    }

    // Check if response is cached in RAM
    if let Some(data) = get_from_ram_cache(&state, &cache_key).await {
        info!(result = "success", reason = "ram cache", server, datafile);
        return (StatusCode::OK, data).into_response();
    }
    // Check if response is cached on disk
    if let Some(data) = get_from_disk_cache(&state, &cache_key).await {
        info!(result = "success", reason = "file cache", server, datafile);
        update_ram_cache(&state, &cache_key, &data).await;
        return (StatusCode::OK, data).into_response();
    }
    // Fetch from the external API
    if let Some(data) = fetch_and_cache(&state, &server, &datafile, &cache_key).await {
        info!(result = "success", reason = "upstream", server, datafile);
        (StatusCode::OK, data).into_response()
    } else {
        info!(result = "fail", reason = "upstream", server, datafile);
        StatusCode::BAD_GATEWAY.into_response()
    }
}

async fn get_from_ram_cache(state: &Arc<AppState>, cache_key: &str) -> Option<Bytes> {
    let cache = state.cache.read().await;
    if let Some(entry) = cache.get(cache_key) {
        if entry.timestamp.elapsed() < CACHE_EXPIRY {
            // Cache hit
            return Some(entry.data.clone());
        }
    }
    None
}

async fn get_from_disk_cache(state: &Arc<AppState>, cache_key: &str) -> Option<Bytes> {
    let cache_path = state.cache_dir.join(cache_key);
    if let Ok(metadata) = fs::metadata(&cache_path).await {
        if metadata.is_file() {
            if let Ok(modified) = metadata.modified() {
                if let Ok(elapsed) = modified.elapsed() {
                    if elapsed < CACHE_EXPIRY {
                        if let Ok(data) = fs::read(&cache_path).await {
                            return Some(Bytes::from(data));
                        }
                    }
                }
            }
        }
    }
    None
}

async fn get_from_failed_cache(state: &Arc<AppState>, cache_key: &str) -> Option<Instant> {
    let cache = state.failed_cache.read().await;
    cache.get(cache_key).copied()
}

async fn update_failed_cache(state: &Arc<AppState>, cache_key: &str) {
    let mut cache = state.failed_cache.write().await;
    cache.insert(cache_key.to_string(), Instant::now());
}

async fn fetch_and_cache(
    state: &Arc<AppState>,
    server: &str,
    datafile: &str,
    cache_key: &str,
) -> Option<Bytes> {
    let url = format!("https://{server}.grepolis.com/data/{datafile}");

    // Perform the HTTP GET request with custom headers
    let Ok(response) = state.client.get(&url).send().await else {
        update_failed_cache(state, cache_key).await;
        return None;
    };

    if !response.status().is_success() {
        update_failed_cache(state, cache_key).await;
        return None;
    }

    let Ok(data) = response.bytes().await else {
        update_failed_cache(state, cache_key).await;
        return None;
    };

    // Update caches
    update_disk_cache(state, cache_key, &data).await;
    update_ram_cache(state, cache_key, &data).await;
    Some(data)
}

async fn update_ram_cache(state: &Arc<AppState>, cache_key: &str, data: &Bytes) {
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
        },
    );
}

async fn update_disk_cache(state: &Arc<AppState>, cache_key: &str, data: &Bytes) {
    let cache_path = state.cache_dir.join(cache_key);
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).await.ok();
    }
    fs::write(cache_path, data).await.ok();
}
