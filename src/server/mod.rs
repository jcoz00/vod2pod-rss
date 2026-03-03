use std::{collections::HashMap, net::TcpListener, time::Instant};

use actix_web::{
    dev::Server, guard, http, middleware, web, App, HttpRequest, HttpResponse, HttpServer,
};
use log::{debug, error, info, warn};
use regex::Regex;
use serde::Deserialize;
use url::Url;
use tokio::process::Command;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use image::{imageops, DynamicImage, GenericImageView};

use crate::{
    configs::{conf, Conf, ConfName},
    provider::{self, MediaProvider},
    rss_transcodizer,
    transcoder::{FfmpegParameters, Transcoder},
};

pub fn spawn_server(listener: TcpListener) -> eyre::Result<Server> {
    let root = conf().get(ConfName::SubfolderPath).unwrap();
    Ok(HttpServer::new(move || {
        App::new()
            .wrap(middleware::NormalizePath::new(
                middleware::TrailingSlash::MergeOnly,
            ))
            .service(
                web::scope(&root)
                    .service(
                        web::resource("transcode_media/to.mp3")
                            .name("transcode_mp3")
                            .guard(guard::Any(guard::Get()).or(guard::Head()))
                            .to(transcode_to_mp3),
                    )
                    .service(
                        //this is an old URL used in old vod2pod versions that did not work with
                        //itunes kept for backwards compatiility
                        web::resource("transcode_media/to_mp3")
                            .name("transcode_mp3_obsolete")
                            .guard(guard::Any(guard::Get()).or(guard::Head()))
                            .to(transcode_to_mp3),
                    )
                    .route("transcodize_rss", web::get().to(transcodize_rss))
                    .route("transcodize_rss", web::head().to(transcodize_rss))
                    .service(
                        web::resource("yt/chapters/{video_id}.json")
                            .name("yt_chapters")
                            .guard(guard::Any(guard::Get()).or(guard::Head()))
                            .to(yt_chapters),
                    )
                    .service(
                        web::resource("yt/transcripts/{video_id}.vtt")
                            .name("yt_transcript")
                            .guard(guard::Any(guard::Get()).or(guard::Head()))
                            .to(yt_transcript),
                    )
                    .service(
                        web::resource("yt/art/square.jpg")
                            .name("yt_square_art")
                            .guard(guard::Any(guard::Get()).or(guard::Head()))
                            .to(yt_square_art),
                    )
                    .service(
                        web::resource("health")
                            .name("health")
                            .route(web::get().to(health)),
                    )
                    .route("/", web::get().to(index))
                    .route("", web::get().to(index)),
            )
    })
    .listen(listener)?
    .run())
}

async fn health() -> HttpResponse {
    HttpResponse::Ok().finish()
}

async fn index(req: HttpRequest) -> HttpResponse {
    if let (Some(user_agent), Some(remote_addr), Some(referer)) = (
        req.headers().get("User-Agent"),
        req.connection_info().peer_addr(),
        req.headers().get("Referer"),
    ) {
        info!(
            "serving homepage - User-Agent: {}, Remote Address: {}, Referer: {}",
            user_agent.to_str().unwrap(),
            remote_addr.to_string(),
            referer.to_str().unwrap()
        );
    }

    let html = std::fs::read_to_string("./templates/index.html").unwrap();
    HttpResponse::Ok().content_type("text/html").body(html)
}
async fn transcodize_rss(
    req: HttpRequest,
    query: web::Query<HashMap<String, String>>,
) -> HttpResponse {
    if req.method() == http::Method::HEAD {
        return HttpResponse::Ok().finish();
    }

    let start_time = Instant::now();

    let should_transcode = match conf().get(ConfName::TranscodingEnabled) {
        Ok(value) => !value.eq_ignore_ascii_case("false"),
        Err(_) => true,
    };

    if !should_transcode {
        warn!("transcoding is disabled");
    }
    let url = if let Some(x) = query.get("url") {
        x
    } else {
        error!("no url provided");
        return HttpResponse::BadRequest().finish();
    };

    let transcode_service_url = req.url_for("transcode_mp3", [""]).unwrap();

    // Base URL for Podcasting 2.0 assets endpoints (chapters/transcripts).
    // This resolves to the same scheme/host/subfolder as the current request.
    let podcast_assets_base_url = req.url_for("health", &[] as &[&str]).ok();

    let parsed_url = match Url::parse(url) {
        Ok(x) => x,
        Err(e) => return HttpResponse::BadRequest().body(e.to_string()),
    };

    let provider = provider::from(&parsed_url);

    if !provider
        .domain_whitelist_regexes()
        .iter()
        .any(|r| r.is_match(parsed_url.as_ref()))
    {
        error!("supplied url ({parsed_url}) not in whitelist (whitelist is needed to prevent SSRF attack)");
        return HttpResponse::Forbidden().body("scheme and host not in whitelist");
    }

    //check cache
    let Ok(mut redis) = crate::get_redis_client().await else {
        error!("could not get redis client");
        return HttpResponse::InternalServerError().finish();
    };

    let cached_rss: Option<String> = redis::cmd("GET")
        .arg(&parsed_url.to_string())
        .query_async(&mut redis)
        .await
        .unwrap_or_default();

    if let Some(cached_rss) = cached_rss {
        info!("serving cached rss feed for {parsed_url}");
        return HttpResponse::Ok()
            .content_type("application/xml")
            .body(cached_rss);
    }

    //generate rss feed
    let raw_rss = match provider.generate_rss_feed(parsed_url.clone()).await {
        Ok(raw_rss) => raw_rss,
        Err(e) => {
            error!("could not generate rss feed for {parsed_url}:\n{e}");
            return HttpResponse::Conflict().finish();
        }
    };

    // rewrite urls in feed
    let injected_feed = rss_transcodizer::inject_vod2pod_customizations(
        raw_rss,
        should_transcode.then_some(transcode_service_url),
        podcast_assets_base_url,
    );

    let body = match injected_feed {
        Ok(body) => body,
        Err(e) => {
            error!("could not inject vod2pod customizations into generated feed");
            error!("{e}");
            return HttpResponse::Conflict().finish();
        }
    };

    //set cache to env var CACHE_TTL (or default 600 seconds)
    let cache_ttl: u64 = match conf().get(ConfName::CacheTTL) {
        Ok(value) => value.parse().unwrap_or(600),
        Err(_) => 600,
    };
    let _: () = redis::cmd("SET")
        .arg(&parsed_url.to_string())
        .arg(&body)
        .arg("EX")
        .arg(cache_ttl)
        .query_async(&mut redis)
        .await
        .unwrap_or_default();

    let end_time = Instant::now();
    let duration = end_time - start_time;
    debug!("rss generation took {} seconds", duration.as_secs_f32());

    HttpResponse::Ok()
        .content_type("application/xml")
        .body(body)
}

#[derive(Deserialize)]
struct VideoIdPath {
    video_id: String,
}

async fn yt_chapters(req: HttpRequest, path: web::Path<VideoIdPath>) -> HttpResponse {
    // Apple Podcasts (and others) will frequently HEAD these URLs before GET.
    // If we reply with Content-Length: 0, Apple tends to treat chapters as missing.
    // So on HEAD we return the would-be payload size via Content-Length.
    let is_head = req.method() == http::Method::HEAD;

    let video_id = path.into_inner().video_id;
    let key = format!("yt:chapters:{video_id}");

    let Ok(mut redis) = crate::get_redis_client().await else {
        return HttpResponse::InternalServerError().finish();
    };

    let cached: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap_or_default();

    if let Some(body) = cached {
        if is_head {
            return HttpResponse::Ok()
                .content_type("application/json+chapters")
                .insert_header((http::header::CONTENT_LENGTH, body.as_bytes().len().to_string()))
                .finish();
        }
        return HttpResponse::Ok()
            .content_type("application/json+chapters")
            .body(body);
    }

    // On-demand extraction using yt-dlp JSON.
    let watch_url = format!("https://www.youtube.com/watch?v={video_id}");
    let out = Command::new("yt-dlp")
        .arg("-j")
        .arg("--no-playlist")
        .arg(&watch_url)
        .output()
        .await;

    let json = match out {
        Ok(x) if x.status.success() => {
            let stdout = String::from_utf8_lossy(&x.stdout);
            serde_json::from_str::<JsonValue>(stdout.trim()).ok()
        }
        Ok(x) => {
            let stderr = String::from_utf8_lossy(&x.stderr);
            warn!("yt-dlp -j failed for {watch_url}: {stderr}");
            None
        }
        Err(e) => {
            warn!("yt-dlp -j spawn failed for {watch_url}: {e}");
            None
        }
    };

    // Convert to Podcasting 2.0 JSON Chapters.
    let chapters = json
        .as_ref()
        .and_then(|j| j.get("chapters"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    // Apple Podcasts' implementation appears to be stricter than some other clients.
    // Use HH:MM:SS timestamps for broad compatibility.
    fn format_timestamp(sec: f64) -> String {
        let total = if sec.is_finite() && sec > 0.0 { sec } else { 0.0 };
        let whole = total.floor() as u64;
        let h = whole / 3600;
        let m = (whole % 3600) / 60;
        let s = whole % 60;
        format!("{:02}:{:02}:{:02}", h, m, s)
    }

    let mut out_chapters = Vec::new();
    for c in chapters {
        let start = c
            .get("start_time")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let title = c
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        out_chapters.push(serde_json::json!({
            "startTime": format_timestamp(start),
            "title": title,
        }));
    }

    let payload = serde_json::json!({
        "version": "1.2.0",
        "chapters": out_chapters,
    })
    .to_string();

    // Cache (30 days). Even empty chapters are cached to keep the system stateless.
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg(&payload)
        .arg("EX")
        .arg(60_u64 * 60 * 24 * 30)
        .query_async(&mut redis)
        .await
        .unwrap_or_default();

    if is_head {
        return HttpResponse::Ok()
            .content_type("application/json+chapters")
            .insert_header((http::header::CONTENT_LENGTH, payload.as_bytes().len().to_string()))
            .finish();
    }

    HttpResponse::Ok()
        .content_type("application/json+chapters")
        .body(payload)
}

async fn yt_transcript(req: HttpRequest, path: web::Path<VideoIdPath>) -> HttpResponse {
    // Apple Podcasts probes with HEAD; provide Content-Length for the would-be body.
    let is_head = req.method() == http::Method::HEAD;

    let video_id = path.into_inner().video_id;
    let key = format!("yt:transcript:{video_id}:en");

    let Ok(mut redis) = crate::get_redis_client().await else {
        return HttpResponse::InternalServerError().finish();
    };

    let cached: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap_or_default();

    if let Some(body) = cached {
        if is_head {
            return HttpResponse::Ok()
                .content_type("text/vtt")
                .insert_header((http::header::CONTENT_LENGTH, body.as_bytes().len().to_string()))
                .finish();
        }
        return HttpResponse::Ok().content_type("text/vtt").body(body);
    }

    let watch_url = format!("https://www.youtube.com/watch?v={video_id}");
    // yt-dlp writes caches by default; force a writable cache dir.
    let cache_dir = "/tmp/yt-dlp-cache";
    let _ = tokio::fs::create_dir_all(cache_dir).await;

    let out = Command::new("yt-dlp")
        .arg("--cache-dir")
        .arg(cache_dir)
        .arg("-j")
        .arg("--no-playlist")
        .arg(&watch_url)
        .output()
        .await;

    let json = match out {
        Ok(x) if x.status.success() => {
            let stdout = String::from_utf8_lossy(&x.stdout);
            serde_json::from_str::<JsonValue>(stdout.trim()).ok()
        }
        Ok(x) => {
            let stderr = String::from_utf8_lossy(&x.stderr);
            warn!("yt-dlp -j failed for {watch_url}: {stderr}");
            None
        }
        Err(e) => {
            warn!("yt-dlp -j spawn failed for {watch_url}: {e}");
            None
        }
    };

    // Try to find a VTT subtitle URL (prefer human subtitles, fallback to auto captions).
    let vtt_url = pick_best_vtt_url(json.as_ref());

    let mut vtt_body = None;
    if let Some(url) = vtt_url {
        match reqwest::get(url).await {
            Ok(resp) if resp.status().is_success() => {
                vtt_body = resp.text().await.ok();
            }
            Ok(resp) => {
                warn!("caption fetch failed status {} for {watch_url}", resp.status());
            }
            Err(e) => {
                warn!("caption fetch error for {watch_url}: {e}");
            }
        }
    }

    // If we can't fetch a transcript, return an empty but valid VTT.
    let payload = vtt_body.unwrap_or_else(|| "WEBVTT\n\n".to_string());

    // Cache (30 days).
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg(&payload)
        .arg("EX")
        .arg(60_u64 * 60 * 24 * 30)
        .query_async(&mut redis)
        .await
        .unwrap_or_default();

    if is_head {
        return HttpResponse::Ok()
            .content_type("text/vtt")
            .insert_header((http::header::CONTENT_LENGTH, payload.as_bytes().len().to_string()))
            .finish();
    }

    HttpResponse::Ok().content_type("text/vtt").body(payload)
}

fn pick_best_vtt_url(json: Option<&JsonValue>) -> Option<String> {
    let j = json?;

    // Helper: select a vtt url from a subtitles map.
    fn select(map: &JsonValue) -> Option<String> {
        let obj = map.as_object()?;
        // Prefer English-ish tracks.
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        let preferred = keys
            .iter()
            .find(|k| k.starts_with("en"))
            .copied()
            .or_else(|| keys.first().copied())?;

        let arr = obj.get(preferred)?.as_array()?;
        // Prefer VTT ext
        for f in arr {
            if f.get("ext").and_then(|e| e.as_str()) == Some("vtt") {
                if let Some(u) = f.get("url").and_then(|u| u.as_str()) {
                    return Some(u.to_string());
                }
            }
        }
        // Fallback to any
        for f in arr {
            if let Some(u) = f.get("url").and_then(|u| u.as_str()) {
                return Some(u.to_string());
            }
        }
        None
    }

    // Prefer human subtitles.
    if let Some(subs) = j.get("subtitles") {
        if let Some(u) = select(subs) {
            return Some(u);
        }
    }
    // Fallback to auto captions.
    if let Some(auto) = j.get("automatic_captions") {
        if let Some(u) = select(auto) {
            return Some(u);
        }
    }
    None
}

#[derive(Deserialize)]
struct ArtQuery {
    src: String,
}

async fn yt_square_art(req: HttpRequest, query: web::Query<ArtQuery>) -> HttpResponse {
    // Apple Podcasts probes episode artwork with HEAD; include Content-Length.
    let is_head = req.method() == http::Method::HEAD;
    if req.method() == http::Method::HEAD {
        return HttpResponse::Ok().content_type("image/jpeg").finish();
    }

    // SSRF protection: only allow known image hosts used by YouTube.
    let Ok(src) = Url::parse(&query.src) else {
        return HttpResponse::BadRequest().body("invalid src url");
    };

    if src.scheme() != "http" && src.scheme() != "https" {
        return HttpResponse::BadRequest().body("invalid scheme");
    }

    let host = src.host_str().unwrap_or_default();
    let allowed = [
        "ytimg.com",
        "i.ytimg.com",
        "img.youtube.com",
        "googleusercontent.com",
        "lh3.googleusercontent.com",
    ];
    if !allowed.iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))) {
        return HttpResponse::Forbidden().body("host not allowed");
    }

    // Cache key by sha256(src)
    let mut hasher = Sha256::new();
    hasher.update(src.as_str().as_bytes());
    let digest = hasher.finalize();
    let key = format!("img:square:{}", hex::encode(digest));

    let Ok(mut redis) = crate::get_redis_client().await else {
        return HttpResponse::InternalServerError().finish();
    };

    let cached: Option<Vec<u8>> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap_or_default();

    if let Some(bytes) = cached {
        if is_head {
            return HttpResponse::Ok()
                .content_type("image/jpeg")
                .insert_header((http::header::CONTENT_LENGTH, bytes.len().to_string()))
                .finish();
        }
        return HttpResponse::Ok().content_type("image/jpeg").body(bytes);
    }

    // Fetch the source image
    let resp = match reqwest::get(src.clone()).await {
        Ok(r) => r,
        Err(e) => return HttpResponse::BadGateway().body(format!("fetch failed: {e}")),
    };

    if !resp.status().is_success() {
        return HttpResponse::BadGateway().body(format!("fetch status {}", resp.status()));
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return HttpResponse::BadGateway().body(format!("fetch body failed: {e}")),
    };

    let img = match image::load_from_memory(&bytes) {
        Ok(i) => i,
        Err(e) => return HttpResponse::UnsupportedMediaType().body(format!("decode failed: {e}")),
    };

    let jpg = match render_square_jpeg(img) {
        Ok(b) => b,
        Err(e) => return HttpResponse::InternalServerError().body(format!("render failed: {e}")),
    };

    // Cache 30 days
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg(&jpg)
        .arg("EX")
        .arg(60_u64 * 60 * 24 * 30)
        .query_async(&mut redis)
        .await
        .unwrap_or_default();

    if is_head {
        return HttpResponse::Ok()
            .content_type("image/jpeg")
            .insert_header((http::header::CONTENT_LENGTH, jpg.len().to_string()))
            .finish();
    }

    HttpResponse::Ok().content_type("image/jpeg").body(jpg)
}

fn render_square_jpeg(img: DynamicImage) -> eyre::Result<Vec<u8>> {
    // 1) Center-crop to square
    let (w, h) = img.dimensions();
    let side = w.min(h);
    let x0 = (w - side) / 2;
    let y0 = (h - side) / 2;
    let cropped = img.crop_imm(x0, y0, side, side);

    // 2) Resize to Apple-safe size (3000x3000, RGB, no alpha)
    let resized = imageops::resize(&cropped.to_rgb8(), 3000, 3000, imageops::FilterType::Lanczos3);

    // 3) Encode JPEG
    let mut out = Vec::with_capacity(512 * 1024);
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
    encoder.encode_image(&DynamicImage::ImageRgb8(resized))?;
    Ok(out)
}

#[derive(Deserialize)]
struct TranscodizeQuery {
    url: Url,
    bitrate: usize,
    duration: usize,
}

fn parse_range_header(
    content_range_str: &str,
    bytes_count: usize,
) -> eyre::Result<(usize, usize, usize)> {
    let re = Regex::new(r"(?P<start>[0-9]{1,20})-?(?P<end>[0-9]{1,20})?")?;
    let captures = if let Some(x) = re.captures_iter(content_range_str).next() {
        x
    } else {
        return Err(eyre::eyre!("content range regex failed"));
    };

    let mut start = 0;
    if let Some(x) = captures.name("start") {
        start = x.as_str().parse()?;
    }

    if bytes_count == 0 {
        error!("The requested Rage header with a length of 0 is invalid: {content_range_str}");
        return Err(eyre::eyre!(
            "The requested Rage header with a length of 0 is invalid: {content_range_str}"
        ));
    }
    let mut end = bytes_count - 1;
    if let Some(x) = captures.name("end") {
        end = x.as_str().parse()?;
    }

    if end == start {
        return Err(eyre::eyre!(
            "The requested Rage header with a length of 0 is invalid: {content_range_str}"
        ));
    }

    let expected = (end + 1) - start;

    Ok((start, end, expected))
}

async fn transcode_to_mp3(req: HttpRequest, query: web::Query<TranscodizeQuery>) -> HttpResponse {
    let stream_url = &query.url;
    let bitrate = query.bitrate;
    let duration_secs = query.duration;
    let total_streamable_bytes = (duration_secs * bitrate * 1000) / 8;
    info!("processing transcode at {bitrate}k for {stream_url}");

    if let Ok(value) = conf().get(ConfName::TranscodingEnabled) {
        if value.eq_ignore_ascii_case("false") {
            return HttpResponse::Forbidden().finish();
        }
    }

    let provider = provider::from(stream_url);

    if !provider
        .domain_whitelist_regexes()
        .iter()
        .any(|r| r.is_match(stream_url.as_ref()))
    {
        error!("supplied url ({stream_url}) not in whitelist (whitelist is needed to prevent SSRF attack)");
        return HttpResponse::Forbidden().body("scheme and host not in whitelist");
    }

    // Range header parsing
    const DEFAULT_CONTENT_RANGE: &str = "0-";
    let content_range_str = match req.headers().get("Range") {
        Some(x) => x.to_str().unwrap_or_default(),
        None => DEFAULT_CONTENT_RANGE,
    };

    debug!("received content range {content_range_str}");

    let (start_bytes, end_bytes, expected_bytes) =
        match parse_range_header(content_range_str, total_streamable_bytes) {
            Ok((start, end, expected)) => (start, end, expected),
            Err(e) => return HttpResponse::BadRequest().body(e.to_string()),
        };

    debug!("requested content-range: bytes {start_bytes}-{end_bytes}/{total_streamable_bytes}");

    if start_bytes > end_bytes || start_bytes > total_streamable_bytes {
        return HttpResponse::RangeNotSatisfiable().finish();
    }

    let seek_secs =
        ((start_bytes as f32) / (total_streamable_bytes as f32)) * (duration_secs as f32);
    debug!("choosen seek_time: {seek_secs}");

    let timeout_in_seconds = conf()
        .get(ConfName::FfmpegTimeoutSeconds)
        .unwrap()
        .parse()
        .unwrap();
    debug!("choosen timeout in seconds: {timeout_in_seconds}");

    let codec = conf().get(ConfName::AudioCodec).unwrap().into();
    let ffmpeg_paramenters = FfmpegParameters {
        seek_time: seek_secs,
        url: stream_url.clone(),
        audio_codec: codec,
        bitrate_kbit: bitrate,
        max_rate_kbit: bitrate * 30,
        expected_bytes_count: expected_bytes,
        timeout_in_seconds: timeout_in_seconds,
    };
    debug!("seconds: {duration_secs}, bitrate: {bitrate}");

    if req.method() == http::Method::HEAD {
        return HttpResponse::Ok()
            .insert_header(("Accept-Ranges", "bytes"))
            .insert_header((
                "Content-Range",
                format!("bytes {start_bytes}-{end_bytes}/{total_streamable_bytes}"),
            ))
            .content_type(codec.get_mime_type_str())
            .finish();
    }

    match Transcoder::new(&ffmpeg_paramenters).await {
        Ok(transcoder) => {
            let stream = transcoder.get_transcode_stream();

            let mut response_builder = if ffmpeg_paramenters.seek_time <= 0.1 {
                HttpResponse::Ok()
            } else {
                HttpResponse::PartialContent()
            };

            response_builder
                .insert_header(("Accept-Ranges", "bytes"))
                .insert_header((
                    "Content-Range",
                    format!("bytes {start_bytes}-{end_bytes}/{total_streamable_bytes}"),
                ))
                .content_type(codec.get_mime_type_str())
                .no_chunking((expected_bytes).try_into().unwrap())
                .streaming(stream)
        }
        Err(e) => HttpResponse::ServiceUnavailable().body(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_start_and_end_start_to_end() {
        let content_range_str = "bytes=0-99";
        let bytes_count = 100;
        let (start, end, expected) = parse_range_header(content_range_str, bytes_count).unwrap();
        assert_eq!((start, end, expected), (0, 99, 100));
    }

    #[test]
    fn test_get_start_and_end_middle1_to_middle2() {
        let content_range_str = "bytes=50-199";
        let bytes_count = 200;
        let (start, end, expected) = parse_range_header(content_range_str, bytes_count).unwrap();
        assert_eq!((start, end, expected), (50, 199, 150));
    }

    #[test]
    fn test_get_start_and_end_middle_to_undefined() {
        let content_range_str = "bytes=100-";
        let bytes_count = 200;
        let (start, end, expected) = parse_range_header(content_range_str, bytes_count).unwrap();
        assert_eq!((start, end, expected), (100, 199, 100));
    }

    #[test]
    fn test_get_start_and_end_start_to_undefined() {
        let content_range_str = "bytes=0-";
        let bytes_count = 200;
        let (start, end, expected) = parse_range_header(content_range_str, bytes_count).unwrap();
        assert_eq!((start, end, expected), (0, 199, 200));
    }
}
