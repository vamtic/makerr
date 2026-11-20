//! HTTP plumbing shared by every fetch: a pooled `reqwest::Client` per settings
//! snapshot, with the exact header set the V1 app sent (User-Agent, Referer,
//! Origin, `plang` cookie) and HTTP/2 enabled.
//!
//! Clients are memoized by [`ClientCache`], which is owned by whoever performs
//! the requests. A client must **never** be shared between tokio runtimes: its
//! pooled connections carry driver tasks that only the runtime that created
//! them polls. A connection reused from a parked runtime accepts the request
//! bytes but never delivers a response, so every fetch hangs until its timeout
//! (this is exactly how a GUI preview could freeze the download pipeline).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Duration;

pub use reqwest::Client;

use crate::settings::Settings;

/// Builds the pooled client. HTTP/2 over ALPN is negotiated automatically
/// with rustls; connection pooling is shared across all requests, which
/// replaces the V1 app's per-thread request workers.
pub fn make_client(settings: &Settings) -> Client {
    client_builder(settings).build().expect("reqwest client")
}

/// The full client configuration. Split out so tests can build a byte-identical
/// client with one extra knob (e.g. `.no_proxy()` for a loopback server).
fn client_builder(settings: &Settings) -> reqwest::ClientBuilder {
    let timeout_secs = if settings.timeout.is_finite() && settings.timeout > 0.0 {
        settings.timeout
    } else {
        30.0
    };
    Client::builder()
        .user_agent(&settings.user_agent)
        .timeout(Duration::from_secs_f64(timeout_secs))
        .connect_timeout(Duration::from_secs_f64(timeout_secs))
        .pool_max_idle_per_host(settings.max_concurrent_tiles.max(8) as usize)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .tcp_nodelay(true)
        .http2_adaptive_window(true)
        // The site-identity headers are constants of the settings snapshot, so
        // they live on the client instead of being formatted into three fresh
        // Strings for every single tile request (thousands per run) and parsed
        // back into `HeaderValue`s by reqwest. `default_headers` clones the
        // already-parsed values per request, which is a refcount bump.
        .default_headers(site_headers(settings))
}

/// The identity headers every request carries. These mirror the sites' own
/// web front-ends, which some servers check for.
pub fn site_headers(settings: &Settings) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue, COOKIE, ORIGIN, REFERER};
    let mut headers = HeaderMap::with_capacity(3);
    // The values are derived from user-supplied settings, so a value that is
    // not a legal header (e.g. a non-ASCII domain) is simply dropped instead of
    // panicking — the request still works, the way it did before.
    if let Ok(v) = HeaderValue::from_str(&format!("https://{}/", settings.domain)) {
        headers.insert(REFERER, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("https://{}", settings.domain)) {
        headers.insert(ORIGIN, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("plang={}", settings.language.code())) {
        headers.insert(COOKIE, v);
    }
    headers
}

/// A memoized client owned by one runtime/worker.
///
/// Keeping the cache per owner instead of process-wide is what stops pooled
/// connections from leaking across tokio runtimes (see the module docs). The
/// GUI net worker owns one for canvas lists/previews, and the job worker's
/// pipeline builds its own; neither can borrow the other's sockets.
///
/// Within one owner the client is still reused, so the TLS/HTTP2 connection
/// pool survives between requests and a cached preview stays instant.
#[derive(Default)]
pub struct ClientCache {
    memo: Mutex<Option<(u64, Client)>>,
}

impl ClientCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The client for these settings, rebuilt only when the
    /// connection-relevant fields change.
    pub fn get(&self, settings: &Settings) -> Client {
        let key = client_fingerprint(settings);
        {
            let memo = self.memo.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((memo_key, client)) = memo.as_ref() {
                if *memo_key == key {
                    return client.clone();
                }
            }
        }
        let client = make_client(settings);
        *self.memo.lock().unwrap_or_else(|p| p.into_inner()) = Some((key, client.clone()));
        client
    }
}

fn client_fingerprint(settings: &Settings) -> u64 {
    let mut hasher = DefaultHasher::new();
    settings.user_agent.hash(&mut hasher);
    settings.timeout.to_bits().hash(&mut hasher);
    settings.max_concurrent_tiles.hash(&mut hasher);
    // The site-identity headers (Referer/Origin/Cookie) are baked into the
    // client, so a domain or language switch has to rebuild it — otherwise the
    // next run would keep sending the previous site's headers.
    settings.domain.hash(&mut hasher);
    settings.language.code().hash(&mut hasher);
    hasher.finish()
}

/// Adds the per-request `Accept` header. The site-identity headers (Referer,
/// Origin, `plang` cookie) are baked into the client by [`make_client`], so no
/// request pays for them.
pub fn apply_headers(
    rb: reqwest::RequestBuilder,
    _settings: &Settings,
    accept: &str,
) -> reqwest::RequestBuilder {
    rb.header("Accept", accept)
}

/// Performs `GET url` with the standard tile headers.
pub async fn get_bytes(
    client: &Client,
    settings: &Settings,
    url: &str,
    accept: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<u8>, FetchError> {
    let req = apply_headers(client.get(url), settings, accept);
    tokio::select! {
        _ = cancel.cancelled() => Err(FetchError::Cancelled),
        resp = req.send() => {
            let resp = resp?;
            let status = resp.status();
            if !status.is_success() {
                let code = status.as_u16();
                // 429/503 carry the server's own pacing hint; keeping it lets
                // the retry loop wait as long as the server asked instead of
                // hammering it with the generic exponential backoff.
                if code == 429 || code == 503 {
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok());
                    return Err(FetchError::RateLimited { code, retry_after });
                }
                return Err(FetchError::Status(code));
            }
            Ok(resp.bytes().await?.to_vec())
        }
    }
}

/// Outcome of a retried `GET`, distinguishing "the tile does not exist" from
/// "the request failed". A 404/410 is legitimate history state (a day or a
/// tile may simply be absent) and is worth remembering; a network/server error
/// is not, because retrying the same tile later may succeed.
#[derive(Debug)]
pub enum GetOutcome {
    Data(Vec<u8>),
    Absent,
    /// The request failed after every retry. `throttled` records whether the
    /// server ever pushed back (429/503), which is what the adaptive limiter
    /// uses to shrink its permit pool.
    Failed {
        throttled: bool,
    },
}

/// True for a PNG payload. Tile endpoints occasionally answer HTTP 200 with
/// an empty body or an HTML error page; treating that as a successful download
/// would poison the disk cache and leave a permanent hole in every frame.
pub fn is_png(data: &[u8]) -> bool {
    data.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// `GET` with retries, also rejecting payloads that arrive but are unusable.
///
/// An empty body is never accepted: it is indistinguishable from "no data was
/// received" and is retried like a transient failure. `is_valid` lets the
/// caller reject endpoint-specific garbage (non-PNG tile bytes, malformed
/// history JSON, an HTML error page) so the retry loop can ask again instead
/// of caching the bad response.
///
/// HTTP 404/410 are treated as *absent* content and are not retried (unlike
/// the V1 implementation, which re-requested them pointlessly): a missing tile
/// is legitimate history state.
pub async fn get_bytes_with_retries_checked_outcome<F>(
    client: &Client,
    settings: &Settings,
    url: &str,
    accept: &str,
    cancel: &tokio_util::sync::CancellationToken,
    mut is_valid: F,
) -> GetOutcome
where
    F: FnMut(&[u8]) -> bool,
{
    let mut throttled = false;
    for attempt in 0..=settings.max_retries {
        if cancel.is_cancelled() {
            return GetOutcome::Failed { throttled: false };
        }
        match get_bytes(client, settings, url, accept, cancel).await {
            Ok(data) if !data.is_empty() && is_valid(&data) => return GetOutcome::Data(data),
            Ok(_) => {
                if attempt < settings.max_retries {
                    // A 200 with unusable bytes is usually a transient edge
                    // response, not a rate limit: use the short linear backoff.
                    tokio::time::sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await;
                }
            }
            Err(FetchError::Cancelled) => return GetOutcome::Failed { throttled: false },
            Err(FetchError::Status(404)) | Err(FetchError::Status(410)) => {
                return GetOutcome::Absent;
            }
            Err(err) => {
                if matches!(err, FetchError::RateLimited { .. }) {
                    throttled = true;
                }
                if attempt < settings.max_retries {
                    // Rate limiting (429) and overload (503) need a much
                    // longer pause than a transient network hiccup: PixelPlanet
                    // answers a burst of history requests with 503s and keeps
                    // doing so while the burst continues. When the server sends
                    // `Retry-After`, that wins.
                    let backoff_ms = match err {
                        FetchError::RateLimited { retry_after, .. } => {
                            throttled = true;
                            let ours = (500u64 << attempt.min(5)).min(8_000);
                            let theirs = retry_after.unwrap_or(0).saturating_mul(1_000).min(60_000);
                            ours.max(theirs)
                        }
                        // Brief linear backoff; small enough to keep big-area
                        // runs moving, long enough to survive bursts.
                        _ => 250 * (attempt as u64 + 1),
                    };
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }
    GetOutcome::Failed { throttled }
}

/// `GET` with retries, preserving the *reason* a tile was not fetched.
pub async fn get_bytes_with_retries_outcome(
    client: &Client,
    settings: &Settings,
    url: &str,
    accept: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> GetOutcome {
    get_bytes_with_retries_checked_outcome(client, settings, url, accept, cancel, |_| true).await
}

/// `GET` with retries. A 404/410 and a persistent failure both map to `None`
/// for callers that only care whether bytes arrived.
pub async fn get_bytes_with_retries(
    client: &Client,
    settings: &Settings,
    url: &str,
    accept: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<Vec<u8>> {
    match get_bytes_with_retries_outcome(client, settings, url, accept, cancel).await {
        GetOutcome::Data(data) => Some(data),
        GetOutcome::Absent | GetOutcome::Failed { .. } => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("cancelled")]
    Cancelled,
    #[error("HTTP {0}")]
    Status(u16),
    /// HTTP 429/503 (throttling) with the server's optional `Retry-After`.
    #[error("HTTP {code} (throttled)")]
    RateLimited { code: u16, retry_after: Option<u64> },
    /// The server kept failing (or throttling) every attempt, so the data is
    /// missing for now. Distinct from `Absent`: a missing day/tile is history,
    /// this is a transient outage worth reporting. `throttled` records whether
    /// the failure was server pushback (429/503) rather than a plain outage,
    /// so the adaptive limiter knows which signal it received.
    #[error("request failed after retries")]
    Unavailable { throttled: bool },
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("{0}")]
    InvalidData(String),
}

/// A downloaded-and-cached tile fetch used by both the pipeline and the GUI
/// preview: consult the disk cache first, then the network, recording stats.
#[allow(clippy::too_many_arguments)] // flat params mirror the V1 tile downloader call site
pub async fn fetch_tile_cached(
    client: &Client,
    settings: &Settings,
    storage_base: &str,
    canvas_id: &str,
    date: &str,
    tile_x: i64,
    tile_y: i64,
    timestamp: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<Vec<u8>> {
    let key = crate::cache::cache_key(storage_base, canvas_id, date, tile_x, tile_y, timestamp);
    match crate::cache::load_entry(&key) {
        crate::cache::CacheStatus::Data(data) => {
            crate::cache::record_cache_hit();
            return Some(data);
        }
        // A cached 404 is definitive: skip the network entirely.
        crate::cache::CacheStatus::Absent => return None,
        crate::cache::CacheStatus::Missing => {}
    }
    let ts = crate::cache::normalize_ts(timestamp);
    let url = crate::tileurl::build(
        storage_base,
        canvas_id,
        date,
        if ts == "base" { "" } else { ts },
        tile_x,
        tile_y,
    );
    match get_bytes_with_retries_checked_outcome(
        client,
        settings,
        &url,
        "image/*, */*",
        cancel,
        is_png,
    )
    .await
    {
        GetOutcome::Data(data) => {
            crate::cache::save_entry(&key, &data);
            crate::cache::record_network_fetch(data.len() as u64);
            Some(data)
        }
        GetOutcome::Absent => {
            crate::cache::mark_absent_entry(&key);
            None
        }
        GetOutcome::Failed { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Language;

    /// The identity headers used to be formatted per request; they now live on
    /// the client, so this pins the exact values the sites expect.
    #[test]
    fn site_headers_carry_referer_origin_and_language_cookie() {
        let settings = Settings {
            domain: "pixmap.fun".into(),
            language: Language::Hu,
            ..Settings::default()
        };
        let headers = site_headers(&settings);
        assert_eq!(headers["referer"], "https://pixmap.fun/");
        assert_eq!(headers["origin"], "https://pixmap.fun");
        assert_eq!(headers["cookie"], "plang=hu");
        assert_eq!(headers.len(), 3);
    }

    /// The client memo must notice a domain/language change, otherwise the next
    /// run would keep sending the previous site's identity headers.
    #[test]
    fn client_fingerprint_tracks_the_identity_headers() {
        let base = Settings::default();
        let mut other_domain = base.clone();
        other_domain.domain = "pixmap.fun".into();
        let mut other_language = base.clone();
        other_language.language = Language::Hu;

        assert_eq!(client_fingerprint(&base), client_fingerprint(&base.clone()));
        assert_ne!(client_fingerprint(&base), client_fingerprint(&other_domain));
        assert_ne!(
            client_fingerprint(&base),
            client_fingerprint(&other_language)
        );
    }

    /// Serves one canned HTTP/1.1 reply on a loopback socket and returns the
    /// request head it received.
    fn serve_once(listener: std::net::TcpListener) -> String {
        use std::io::{Read, Write};
        let (mut sock, _) = listener.accept().expect("accept");
        let mut head = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = sock.read(&mut chunk).expect("read request");
            if n == 0 {
                break;
            }
            head.extend_from_slice(&chunk[..n]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let _ =
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
        let _ = sock.flush();
        String::from_utf8_lossy(&head).into_owned()
    }

    /// End-to-end proof that the browser-style headers actually reach the wire
    /// on a plain tile GET (the client carries them, not `apply_headers`).
    #[tokio::test]
    async fn get_bytes_sends_the_site_identity_headers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || serve_once(listener));

        let settings = Settings {
            domain: "pixmap.fun".into(),
            language: Language::Hu,
            ..Settings::default()
        };
        let client = client_builder(&settings)
            .no_proxy()
            .build()
            .expect("client");
        let cancel = tokio_util::sync::CancellationToken::new();
        let url = format!("http://{addr}/history/chunk/0/20000103/0000/1_2.png");
        let body = get_bytes(&client, &settings, &url, "image/*, */*", &cancel)
            .await
            .expect("GET succeeds");
        assert_eq!(body, b"hi");

        let head = server.join().expect("server thread").to_ascii_lowercase();
        for needle in [
            "referer: https://pixmap.fun/",
            "origin: https://pixmap.fun",
            "cookie: plang=hu",
            "accept: image/*, */*",
            "user-agent: timelapsemaker",
        ] {
            assert!(head.contains(needle), "missing `{needle}` in:\n{head}");
        }
    }

    /// A 503 burst (PixelPlanet's rate limit) is retried and eventually
    /// succeeds instead of being reported as missing data.
    #[tokio::test]
    async fn service_unavailable_is_retried() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            for attempt in 0..3 {
                let (mut sock, _) = listener.accept().expect("accept");
                // Drain the request head first: replying and closing with
                // unread request bytes makes the kernel send an RST, which
                // surfaces as a network error instead of an HTTP status.
                let mut head = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let n = sock.read(&mut chunk).expect("read request");
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&chunk[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                if attempt < 2 {
                    let _ = sock.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else {
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
                    );
                }
                let _ = sock.flush();
            }
        });

        let settings = Settings {
            max_retries: 3,
            ..Settings::default()
        };
        let client = client_builder(&settings)
            .no_proxy()
            .build()
            .expect("client");
        let cancel = tokio_util::sync::CancellationToken::new();
        let url = format!("http://{addr}/history?day=20000110&id=0");
        let out =
            get_bytes_with_retries_outcome(&client, &settings, &url, "application/json", &cancel)
                .await;
        match out {
            GetOutcome::Data(data) => assert_eq!(data, b"hi"),
            other => panic!("expected data after retries, got {other:?}"),
        }
        server.join().expect("server thread");
    }

    /// An HTTP 200 with no body (or an HTML error page) is not data: it must be
    /// retried instead of being cached as a tile or parsed as an empty day.
    #[tokio::test]
    async fn empty_or_invalid_payload_is_retried() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let bodies: [&[u8]; 2] = [b"", b"\x89PNG\r\n\x1a\nrest"];
            for body in bodies {
                let (mut sock, _) = listener.accept().expect("accept");
                let mut head = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let n = sock.read(&mut chunk).expect("read request");
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&chunk[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes());
                let _ = sock.write_all(body);
                let _ = sock.flush();
            }
        });

        let settings = Settings {
            max_retries: 2,
            ..Settings::default()
        };
        let client = client_builder(&settings)
            .no_proxy()
            .build()
            .expect("client");
        let cancel = tokio_util::sync::CancellationToken::new();
        let url = format!("http://{addr}/tile.png");
        let outcome = get_bytes_with_retries_checked_outcome(
            &client, &settings, &url, "image/*", &cancel, is_png,
        )
        .await;
        match outcome {
            GetOutcome::Data(data) => assert!(is_png(&data)),
            other => panic!("expected data after retrying the empty body, got {other:?}"),
        }
        server.join().expect("server thread");
    }

    /// A 404 is *absent* content: one request, no retries, `None`.
    #[tokio::test]
    async fn missing_tiles_are_not_retried() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            use std::io::Write;
            let (mut sock, _) = listener.accept().expect("accept");
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            let _ = sock.flush();
            // The client must not come back for a second attempt.
            listener.set_nonblocking(true).expect("nonblocking");
            listener.accept().is_err()
        });

        let settings = Settings {
            max_retries: 5,
            ..Settings::default()
        };
        let client = client_builder(&settings)
            .no_proxy()
            .build()
            .expect("client");
        let cancel = tokio_util::sync::CancellationToken::new();
        let url = format!("http://{addr}/missing.png");
        assert_eq!(
            get_bytes_with_retries(&client, &settings, &url, "image/*", &cancel).await,
            None
        );
        assert!(server.join().expect("server thread"), "no retry expected");
    }

    /// A 429 marks the failure as server pushback so the adaptive limiter can
    /// shrink its pool, and the run does not report it as missing content.
    #[tokio::test]
    async fn rate_limit_is_reported_as_throttled() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut sock, _) = listener.accept().expect("accept");
            // Drain the request head: replying and closing with unread request
            // bytes makes the kernel send an RST, which surfaces as a network
            // error instead of the HTTP status.
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = sock.read(&mut chunk).expect("read request");
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&chunk[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = sock.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 0\r\n\r\n",
            );
            let _ = sock.flush();
        });

        let settings = Settings {
            max_retries: 0,
            ..Settings::default()
        };
        let client = client_builder(&settings)
            .no_proxy()
            .build()
            .expect("client");
        let cancel = tokio_util::sync::CancellationToken::new();
        let url = format!("http://{addr}/history");
        let outcome =
            get_bytes_with_retries_outcome(&client, &settings, &url, "application/json", &cancel)
                .await;
        // The 429 must never look like missing history.
        match outcome {
            GetOutcome::Failed { throttled: true } => {}
            other => panic!("expected a throttled failure, got {other:?}"),
        }

        // ...and feeding it to the limiter must halve the pool, which is the
        // whole point of the classification.
        let limiter = crate::adaptive::AdaptiveLimiter::new(8, 2, 12);
        limiter.report(crate::adaptive::Outcome::Throttled);
        assert_eq!(limiter.limit(), 4);
        server.join().expect("server thread");
    }
}
