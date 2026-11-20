//! Canvas metadata (`/api/canvases`, `/api/me`) and per-day snapshot
//! history (`/history`, `/history/snapshots`) fetching and parsing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::adaptive::{AdaptiveLimiter, Outcome};
use crate::net::{self, FetchError};
use crate::settings::Settings;
use crate::site;

/// One canvas from the site's canvas list.
#[derive(Debug, Clone, PartialEq)]
pub struct CanvasInfo {
    pub id: String,
    pub name: String,
    /// Canonical edge length in pixels (site quirks already applied).
    pub size_px: i64,
    /// First palette color packed as `0xRRGGBB`. `None` means the site did not
    /// publish a palette; callers fall back to the V1 app's white default.
    pub bg_color: Option<u32>,
}

/// Fetches the canvas list for a domain.
pub async fn fetch_canvas_list(
    client: &net::Client,
    settings: &Settings,
    cancel: &CancellationToken,
) -> Result<Vec<CanvasInfo>, FetchError> {
    let endpoint = if site::uses_canvases_api(&settings.domain) {
        "canvases"
    } else {
        "me"
    };
    let url = format!("https://{}/api/{endpoint}", settings.domain);
    let outcome = net::get_bytes_with_retries_checked_outcome(
        client,
        settings,
        &url,
        "application/json, text/plain, */*",
        cancel,
        is_valid_canvas_json,
    )
    .await;

    let data = match outcome {
        net::GetOutcome::Data(bytes) => bytes,
        _ => {
            // Fallback: If api/canvases or api/me returns 403 or error (e.g.
            // PixelPlanet Cloudflare block), fetch the homepage HTML and
            // extract window.ssv. The fallback gets the same retry/validation
            // treatment so a truncated homepage is not parsed as "no canvases".
            let home_url = format!("https://{}/", settings.domain);
            let home = net::get_bytes_with_retries_checked_outcome(
                client,
                settings,
                &home_url,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                cancel,
                |html| extract_ssv_canvases(html).is_some(),
            )
            .await;
            match home {
                net::GetOutcome::Data(html) => extract_ssv_canvases(&html)
                    .ok_or_else(|| FetchError::InvalidData("invalid canvas HTML".into()))?,
                net::GetOutcome::Absent => return Err(FetchError::Status(404)),
                net::GetOutcome::Failed { throttled } => {
                    return Err(if cancel.is_cancelled() {
                        FetchError::Cancelled
                    } else {
                        FetchError::Unavailable { throttled }
                    });
                }
            }
        }
    };
    parse_canvas_list(&data, &settings.domain)
        .ok_or(FetchError::InvalidData("invalid canvas data".into()))
}

/// A canvas-list response is only usable when it parses as JSON and has the
/// `canvases` object both site families publish.
fn is_valid_canvas_json(data: &[u8]) -> bool {
    serde_json::from_slice::<Value>(data)
        .ok()
        .and_then(|doc| doc.get("canvases").and_then(Value::as_object).map(|_| ()))
        .is_some()
}

fn extract_ssv_canvases(html: &[u8]) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(html).ok()?;
    let marker = "window.ssv";
    let pos = s.find(marker)?;
    let rest = &s[pos + marker.len()..];
    let eq_pos = rest.find('=')?;
    let start = rest[eq_pos + 1..].find('{')? + eq_pos + 1;
    let after_brace = &rest[start..];

    // Find matching closing brace
    let mut depth = 0;
    let mut end = 0;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in after_brace.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            in_str = !in_str;
            continue;
        }
        if !in_str {
            if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
        }
    }
    if end > 0 {
        Some(after_brace.as_bytes()[..end].to_vec())
    } else {
        None
    }
}

/// Parses a canvas-list payload. Returns `None` when the payload has no
/// usable `canvases` object.
pub fn parse_canvas_list(data: &[u8], domain: &str) -> Option<Vec<CanvasInfo>> {
    let doc: Value = serde_json::from_slice(data).ok()?;
    let canvases = doc.get("canvases")?.as_object()?;

    let mut out = Vec::new();
    for (id, canvas) in canvases {
        // Skip the 3D canvases, same as the GUI always did: their history
        // tiles are not 2D chunk PNGs.
        let name = site::canvas_name(canvas).unwrap_or_else(|| id.clone());
        if name.to_ascii_lowercase().contains("3d") {
            continue;
        }
        let raw_size = site::canvas_raw_size(canvas).unwrap_or(0);
        let size_px = site::canvas_size_pixels(domain, raw_size);
        let bg_color = site::canvas_palette(canvas)
            .first()
            .map(|c| ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32);
        out.push(CanvasInfo {
            id: id.clone(),
            name,
            size_px,
            bg_color,
        });
    }
    Some(out)
}

/// Looks one canvas up by id.
pub fn find_canvas<'a>(canvases: &'a [CanvasInfo], canvas_id: &str) -> Option<&'a CanvasInfo> {
    canvases.iter().find(|c| c.id == canvas_id)
}

/// Fetches the snapshot times ("hhmm") for one day.
///
/// A 404/410 means the day has no published history (e.g. before the canvas
/// existed) and yields an empty list. Any other persistent failure is an
/// error: silently turning an outage into "this day had no snapshots" makes a
/// throttled run look like it worked while whole days are missing.
pub async fn fetch_day_timestamps(
    client: &net::Client,
    settings: &Settings,
    date: &str,
    canvas_id: &str,
    cancel: &CancellationToken,
) -> Result<Vec<String>, FetchError> {
    let url = site::history_url(&settings.domain, date, canvas_id);
    let snapshots_format = site::uses_snapshots_history_api(&settings.domain);
    match net::get_bytes_with_retries_checked_outcome(
        client,
        settings,
        &url,
        "application/json, text/plain, */*",
        cancel,
        |bytes| is_valid_history_json(bytes, snapshots_format),
    )
    .await
    {
        net::GetOutcome::Data(bytes) => Ok(parse_history(&bytes, snapshots_format)),
        net::GetOutcome::Absent => Ok(Vec::new()),
        net::GetOutcome::Failed { throttled } => {
            if cancel.is_cancelled() {
                Err(FetchError::Cancelled)
            } else {
                Err(FetchError::Unavailable { throttled })
            }
        }
    }
}

/// Validates the envelope before parsing. A malformed body used to be turned
/// into an empty timestamp list, which silently dropped a whole day from the
/// timeline; now it is retried and, if it stays broken, reported as a failure.
fn is_valid_history_json(data: &[u8], snapshots_format: bool) -> bool {
    let Ok(doc) = serde_json::from_slice::<Value>(data) else {
        return false;
    };
    if snapshots_format {
        doc.get("snapshots").is_some_and(Value::is_array)
    } else {
        doc.is_array()
    }
}

/// Parses a history payload into sorted "hhmm" snapshot times, dropping
/// "0000" (that base frame is fetched separately as the day base).
pub fn parse_history(data: &[u8], snapshots_format: bool) -> Vec<String> {
    let mut ts: Vec<String> = Vec::new();
    if let Ok(doc) = serde_json::from_slice::<Value>(data) {
        if snapshots_format {
            // PixMap: {"snapshots":[{"ts":...,"hhmm":"0029"},...]} — but also
            // tolerate plain "0029" strings like the reference PixMap
            // downloader does.
            if let Some(arr) = doc.get("snapshots").and_then(Value::as_array) {
                for v in arr {
                    let t = v
                        .as_object()
                        .and_then(|o| o.get("hhmm"))
                        .and_then(Value::as_str)
                        .or_else(|| v.as_str());
                    if let Some(t) = t {
                        if !t.is_empty() && t != "0000" {
                            ts.push(t.to_string());
                        }
                    }
                }
            }
        } else {
            // PixelPlanet family: a flat array of "hhmm" strings.
            if let Some(arr) = doc.as_array() {
                for v in arr {
                    if let Some(t) = v.as_str() {
                        if !t.is_empty() && t != "0000" {
                            ts.push(t.to_string());
                        }
                    }
                }
            }
        }
    }
    ts.sort();
    ts.dedup();
    ts
}

/// Starting point for the adaptive `/history` limiter.
///
/// The history endpoints are rate limited aggressively: a burst of ~50
/// requests to PixelPlanet returns mostly 503. The limiter starts here and may
/// grow a little while the server is happy, but never to the tile level. A day
/// that still fails after the burst is retried sequentially by
/// [`retry_failed_days`], so the higher starting point cannot create holes in
/// the timeline.
pub const HISTORY_SEED_CONCURRENCY: usize = 12;

/// Streams per-day snapshot times in date order.
///
/// Every day is requested concurrently (bounded by the adaptive history
/// limiter), but the stream yields days chronologically: callers can start
/// working on the first days while the rest of the range is still downloading.
/// Out-of-order arrivals wait in a small reorder buffer.
pub struct DayTimestampsStream {
    rx: mpsc::UnboundedReceiver<(usize, Result<Vec<String>, FetchError>)>,
    dates: Vec<chrono::NaiveDate>,
    buffer: BTreeMap<usize, Result<Vec<String>, FetchError>>,
    next: usize,
    total: usize,
    settled: usize,
    handles: Vec<tokio::task::JoinHandle<()>>,
    on_progress: Box<dyn FnMut(usize, usize)>,
}

impl DayTimestampsStream {
    pub fn total(&self) -> usize {
        self.total
    }

    pub fn settled(&self) -> usize {
        self.settled
    }

    /// The next day in chronological order, or `None` when every day has been
    /// yielded (or the tasks were aborted).
    pub async fn next(&mut self) -> Option<(chrono::NaiveDate, Result<Vec<String>, FetchError>)> {
        while !self.buffer.contains_key(&self.next) && self.settled < self.total {
            match self.rx.recv().await {
                Some((idx, result)) => {
                    self.settled += 1;
                    self.buffer.insert(idx, result);
                    (self.on_progress)(self.settled, self.total);
                }
                None => break,
            }
        }
        let result = self.buffer.remove(&self.next)?;
        let date = self.dates[self.next];
        self.next += 1;
        Some((date, result))
    }
}

impl Drop for DayTimestampsStream {
    fn drop(&mut self) {
        // A cancelled/abandoned run must not leave history requests running in
        // the background for the next run to trip over.
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// Starts all day requests and returns their ordered stream.
///
/// `on_progress` is called after every day settles with `(done, total)` — in
/// arrival order, so a slow day cannot freeze the progress display.
pub fn stream_range_timestamps(
    client: net::Client,
    settings: Settings,
    dates: Vec<chrono::NaiveDate>,
    canvas_id: String,
    cancel: CancellationToken,
    on_progress: Box<dyn FnMut(usize, usize)>,
) -> DayTimestampsStream {
    let limiter = Arc::new(AdaptiveLimiter::for_history(
        settings.adaptive_concurrency,
        settings.max_connections,
    ));
    let total = dates.len();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut handles = Vec::with_capacity(total);
    for (idx, date) in dates.iter().copied().enumerate() {
        let client = client.clone();
        let settings = settings.clone();
        let cancel = cancel.clone();
        let canvas_id = canvas_id.clone();
        let limiter = Arc::clone(&limiter);
        let tx = tx.clone();
        handles.push(tokio::spawn(async move {
            let _permit = limiter.acquire().await;
            let date_str = date.format("%Y%m%d").to_string();
            let result =
                fetch_day_timestamps(&client, &settings, &date_str, &canvas_id, &cancel).await;
            limiter.report(match &result {
                Ok(_) => Outcome::Ok,
                Err(FetchError::Unavailable { throttled: true }) => Outcome::Throttled,
                Err(_) => Outcome::Failed,
            });
            let _ = tx.send((idx, result));
        }));
    }
    drop(tx);
    DayTimestampsStream {
        rx,
        dates,
        buffer: BTreeMap::new(),
        next: 0,
        total,
        settled: 0,
        handles,
        on_progress,
    }
}

/// Retries days that failed during the concurrent burst.
///
/// A burst of `/history` requests can exhaust every per-request retry while the
/// server is still shedding load, even though the same day succeeds moments
/// later. Retrying those few days one at a time after the burst has drained
/// recovers them without dropping frames from the timeline. Successful days are
/// never requested again.
pub async fn retry_failed_days(
    client: &net::Client,
    settings: &Settings,
    failed: Vec<chrono::NaiveDate>,
    canvas_id: &str,
    cancel: &CancellationToken,
) -> (
    BTreeMap<chrono::NaiveDate, Vec<String>>,
    Vec<chrono::NaiveDate>,
) {
    let mut recovered = BTreeMap::new();
    let mut still_failed = Vec::new();
    if failed.is_empty() || cancel.is_cancelled() {
        return (recovered, failed);
    }

    // Give the burst a moment to leave the server before the fallback starts.
    tokio::time::sleep(Duration::from_millis(250)).await;

    for date in failed {
        if cancel.is_cancelled() {
            still_failed.push(date);
            continue;
        }
        let date_str = date.format("%Y%m%d").to_string();
        match fetch_day_timestamps(client, settings, &date_str, canvas_id, cancel).await {
            Ok(ts) => {
                recovered.insert(date, ts);
            }
            Err(FetchError::Cancelled) => {
                still_failed.push(date);
                break;
            }
            Err(_) => still_failed.push(date),
        }
    }
    (recovered, still_failed)
}

/// Result of fetching a whole date range.
pub struct RangeTimestamps {
    /// Snapshot times per day. Failed days are *absent* from this map rather
    /// than mapped to an empty list, so "no snapshots" stays distinguishable
    /// from "could not fetch".
    pub per_day: BTreeMap<chrono::NaiveDate, Vec<String>>,
    /// Days whose history endpoint failed after every retry. Callers should
    /// report these instead of silently producing a timeline with holes.
    pub failed_days: Vec<chrono::NaiveDate>,
}

impl RangeTimestamps {
    pub fn get(&self, date: &chrono::NaiveDate) -> Option<&Vec<String>> {
        self.per_day.get(date)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&chrono::NaiveDate, &Vec<String>)> {
        self.per_day.iter()
    }
}

/// Collects the whole range. Kept for the CLI's `plan` command, embedders and
/// tests; the pipeline itself consumes [`DayTimestampsStream`] directly.
pub async fn fetch_range_timestamps<F: FnMut(usize, usize) + 'static>(
    client: &net::Client,
    settings: &Settings,
    dates: &[chrono::NaiveDate],
    canvas_id: &str,
    cancel: &CancellationToken,
    on_progress: F,
) -> RangeTimestamps {
    let mut stream = stream_range_timestamps(
        client.clone(),
        settings.clone(),
        dates.to_vec(),
        canvas_id.to_string(),
        cancel.clone(),
        Box::new(on_progress),
    );

    let mut per_day = BTreeMap::new();
    let mut failed_days = Vec::new();
    while let Some((date, result)) = stream.next().await {
        match result {
            Ok(ts) => {
                per_day.insert(date, ts);
            }
            Err(FetchError::Cancelled) => {
                failed_days.push(date);
                break;
            }
            Err(_) => failed_days.push(date),
        }
    }
    drop(stream);

    let (recovered, still_failed) =
        retry_failed_days(client, settings, failed_days, canvas_id, cancel).await;
    per_day.extend(recovered);
    RangeTimestamps {
        per_day,
        failed_days: still_failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pixmap_snapshots_object() {
        let payload = br#"{"snapshots":[
            {"ts":1726012800,"hhmm":"0000"},
            {"ts":1726014540,"hhmm":"0029"},
            {"ts":1726015440,"hhmm":"0044"}]}"#;
        let ts = parse_history(payload, true);
        assert_eq!(ts, vec!["0029", "0044"]);
    }

    #[test]
    fn parses_pixmap_plain_string_array_too() {
        let payload = br#"{"snapshots":["0000","0031","0120"]}"#;
        assert_eq!(parse_history(payload, true), vec!["0031", "0120"]);
    }

    #[test]
    fn parses_flat_string_array() {
        let payload = br#"["0000","0300","1200"]"#;
        assert_eq!(parse_history(payload, false), vec!["0300", "1200"]);
    }

    #[test]
    fn sorts_and_dedups() {
        let payload = br#"["1200","0300","1200","0900"]"#;
        assert_eq!(parse_history(payload, false), vec!["0300", "0900", "1200"]);
    }

    #[test]
    fn garbage_yields_empty() {
        assert!(parse_history(b"not json", false).is_empty());
        assert!(parse_history(b"{}", false).is_empty());
        assert!(parse_history(b"{}", true).is_empty());
    }

    #[test]
    fn history_envelope_is_validated_before_retry_gives_up() {
        assert!(is_valid_history_json(br#"["0300","1200"]"#, false));
        assert!(is_valid_history_json(br#"{"snapshots":[]}"#, true));
        assert!(!is_valid_history_json(b"not json", false));
        assert!(!is_valid_history_json(b"[]", true));
        assert!(!is_valid_history_json(br#"{"snapshots":"0300"}"#, true));
    }

    #[tokio::test]
    async fn day_stream_yields_days_in_chronological_order() {
        use chrono::NaiveDate;

        let dates: Vec<NaiveDate> = (1..=3)
            .map(|d| NaiveDate::from_ymd_opt(2000, 1, d).unwrap())
            .collect();
        let (tx, rx) = mpsc::unbounded_channel();
        let progress = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&progress);
        let mut stream = DayTimestampsStream {
            rx,
            dates: dates.clone(),
            buffer: BTreeMap::new(),
            next: 0,
            total: 3,
            settled: 0,
            handles: Vec::new(),
            on_progress: Box::new(move |done, total| {
                seen.lock().unwrap().push((done, total));
            }),
        };

        // Day 1 settles before day 0; the stream must still yield day 0 first.
        tx.send((1, Ok(vec!["0300".into()]))).unwrap();
        tx.send((0, Ok(Vec::new()))).unwrap();
        assert_eq!(stream.next().await.unwrap().0, dates[0]);
        assert_eq!(stream.next().await.unwrap().0, dates[1]);

        tx.send((2, Err(FetchError::Unavailable { throttled: true })))
            .unwrap();
        let (date, result) = stream.next().await.unwrap();
        assert_eq!(date, dates[2]);
        assert!(matches!(
            result,
            Err(FetchError::Unavailable { throttled: true })
        ));
        assert_eq!(*progress.lock().unwrap(), vec![(1, 3), (2, 3), (3, 3)]);
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn parses_canvas_list_with_site_quirks() {
        let pixmap = br#"{"canvases":{
            "0":{"id":0,"name":"Earth","size":256},
            "2":{"id":2,"name":"Moon","size":64},
            "17":{"id":17,"name":"Minimap3d","size":16}},
            "default":0}"#;
        let list = parse_canvas_list(pixmap, "pixmap.fun").unwrap();
        // "Minimap3d" contains "3d" and is filtered out.
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "0");
        assert_eq!(list[0].size_px, 65536);
        assert_eq!(list[0].bg_color, None);
        assert_eq!(list[1].id, "2");
        assert_eq!(list[1].size_px, 16384);
        assert_eq!(list[1].bg_color, None);

        let ppf = br#"{"canvases":{"0":{"title":"Earth","size":65536,"colors":[[255,255,255]]}}}"#;
        let list = parse_canvas_list(ppf, "pixelplanet.fun").unwrap();
        assert_eq!(list[0].name, "Earth");
        assert_eq!(list[0].size_px, 65536);
        assert_eq!(list[0].bg_color, Some(0xffffff));
    }

    #[test]
    fn invalid_canvas_payload_is_none() {
        assert!(parse_canvas_list(b"{}", "pixelplanet.fun").is_none());
        assert!(parse_canvas_list(b"garbage", "pixelplanet.fun").is_none());
    }
}
