// youtube.rs — drop-in replacement with Redis-cached keys/values changed from Url -> String
// to avoid redis trait/version issues (cached + redis 0.32.x compatibility).

#[allow(unused_imports)]
use cached::proc_macro::io_cached;
#[allow(unused_imports)]
use cached::AsyncRedisCache;

use bytes::Bytes;
use feed_rs::model::Feed;
use google_youtube3::{
    api::{self, PlaylistItem},
    common, hyper, hyper_rustls, YouTube,
};
use http_body_util::combinators::BoxBody;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::fs;

use async_trait::async_trait;
use eyre::eyre;
use log::{debug, info, warn};
use regex::Regex;
use reqwest::Url;
use rss::{
    extension::itunes::{ITunesChannelExtensionBuilder, ITunesItemExtensionBuilder},
    Channel, ChannelBuilder, GuidBuilder, ImageBuilder, Item, ItemBuilder,
};
use rss::extension::itunes::ITunesCategory;
use tokio::process::Command;

use crate::{
    configs::{conf, Conf, ConfName},
    provider,
};

use super::MediaProvider;

pub struct YoutubeProvider;

enum IdType {
    Playlist(String),
    Channel(String),
}

#[async_trait]
impl MediaProvider for YoutubeProvider {
    async fn generate_rss_feed(&self, channel_url: Url) -> eyre::Result<String> {
        let youtube_api_key = conf().get(ConfName::YoutubeApiKey).ok();

        match youtube_api_key {
            Some(api_key) => {
                info!(
                    "starting youtube feed generation for {} with API key",
                    channel_url
                );
                let mut feed_builder = provider::build_default_rss_structure();

                let id = match channel_url.path() {
                    path if path.starts_with("/playlist") => {
                        let playlist_id = channel_url
                            .query_pairs()
                            .find(|(key, _)| key == "list")
                            .map(|(_, value)| value)
                            .ok_or_else(|| {
                                eyre::eyre!("Failed to parse playlist ID from URL: {}", channel_url)
                            })?;
                        IdType::Playlist(playlist_id.into())
                    }
                    path if path.starts_with("/channel/")
                        || path.starts_with("/user/")
                        || path.starts_with("/c/")
                        || path.starts_with("/@") =>
                    {
                        let url = find_yt_channel_url_with_c_id(&channel_url).await?;
                        let channel_id = url
                            .path_segments()
                            .ok_or_else(|| eyre!("could not parse channel id"))?
                            .last()
                            .ok_or_else(|| eyre!("could not parse channel id"))?;
                        IdType::Channel(channel_id.into())
                    }
                    _ => return Err(eyre!("unsupported youtube url")),
                };

                let mut video_items = fetch_from_api(id, api_key).await?;

                feed_builder.description(video_items.0.description);
                feed_builder.title(video_items.0.title);
                feed_builder.language(video_items.0.language.take());

                let mut image_builder = ImageBuilder::default();
                image_builder.url(
                    video_items
                        .0
                        .itunes_ext
                        .clone()
                        .and_then(|it| it.image)
                        .unwrap_or_default(),
                );
                feed_builder.image(Some(image_builder.build()));
                feed_builder.itunes_ext(video_items.0.itunes_ext.take());
                feed_builder.link(video_items.0.link);

                feed_builder.items(video_items.1);

                Ok(feed_builder.build().to_string())
            }
            None => {
                info!(
                    "starting youtube feed generation for {} using atom feed",
                    channel_url
                );
                let feed = match channel_url.path() {
                    path if path.starts_with("/playlist") => {
                        feed_url_for_yt_playlist(&channel_url).await
                    }
                    path if path.starts_with("/feeds/") => feed_url_for_yt_atom(&channel_url).await,
                    path if path.starts_with("/channel/") => {
                        feed_url_for_yt_channel(&channel_url).await
                    }
                    path if path.starts_with("/user/") => feed_url_for_yt_channel(&channel_url).await,
                    path if path.starts_with("/c/") => feed_url_for_yt_channel(&channel_url).await,
                    path if path.starts_with("/@") => feed_url_for_yt_channel(&channel_url).await,
                    _ => Err(eyre!("unsupported youtube url")),
                }?;

                let raw_atom_feed = reqwest::get(feed).await?.text().await?;
                let feed = feed_rs::parser::parse(&raw_atom_feed.into_bytes()[..])
                    .map_err(|e| eyre!("failed to parse atom feed: {e}"))?;

                let mut duration_map: HashMap<String, Option<usize>> = HashMap::default();
                for link in feed.clone().entries.iter().filter_map(|e| e.links.first()) {
                    let parsed = link.href.parse::<Url>()?;
                    duration_map.insert(
                        link.href.clone(),
                        get_youtube_video_duration_with_ytdlp(&parsed).await?,
                    );
                }

                Ok(convert_atom_to_rss(feed, duration_map))
            }
        }
    }

    async fn get_stream_url(&self, media_url: &Url) -> eyre::Result<Url> {
        get_youtube_stream_url(media_url).await
    }

    fn domain_whitelist_regexes(&self) -> Vec<Regex> {
        let youtube_whitelist = vec![
            regex::Regex::new(r"^(https://)?.*\.youtube\.com/").unwrap(),
            regex::Regex::new(r"^(https://)?youtube\.com/").unwrap(),
            regex::Regex::new(r"^(https://)?youtu\.be/").unwrap(),
            regex::Regex::new(r"^(https://)?.*\.youtu\.be/").unwrap(),
            regex::Regex::new(r"^(https://)?.*\.googlevideo\.com/").unwrap(),
        ];

        #[cfg(not(test))]
        return youtube_whitelist;
        #[cfg(test)] // this will allow test to use localhost and still work
        return [
            youtube_whitelist,
            vec![regex::Regex::new(r"^http://127\.0\.0\.1:9870").unwrap()],
        ]
        .concat();
    }
}

async fn fetch_from_api(id: IdType, api_key: String) -> eyre::Result<(Channel, Vec<Item>)> {
    match id {
        IdType::Playlist(id) => {
            info!("fetching playlist {}", id);
            let mut playlist = fetch_playlist(id, &api_key).await?;

            let playlist_id = playlist.id.take().ok_or(eyre!("playlist has no id"))?;

            let mut rss_channel = build_channel_from_playlist(playlist);

            let max_fetched_items: usize =
                conf().get(ConfName::YoutubeMaxResults).unwrap().parse()?;
            let items = fetch_playlist_items(&playlist_id, &api_key, max_fetched_items).await?;

            let duration_map = create_duration_url_map(&items, &api_key).await?;

            // Filter out Shorts (or any videos below a minimum duration) if configured.
            // NOTE: best-effort; if we can't resolve duration for a video, we keep it.
            let min_seconds: u64 = conf()
                .get(ConfName::YoutubeMinSeconds)
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .unwrap_or(0);

            let items: Vec<PlaylistItem> = if min_seconds > 0 {
                items
                    .into_iter()
                    .filter(|it| {
                        let Some(snippet) = it.snippet.as_ref() else { return false };
                        let Some(res) = snippet.resource_id.as_ref() else { return true };
                        let Some(vid) = res.video_id.as_ref() else { return true };
                        duration_map
                            .get(vid)
                            .map(|v| duration_to_seconds(&v.duration) >= min_seconds)
                            .unwrap_or(true)
                    })
                    .collect()
            } else {
                items
            };

            apply_best_fit_itunes_category(&mut rss_channel, &duration_map);

            let rss_items = build_channel_items_from_playlist(items, duration_map);

            Ok((rss_channel, rss_items))
        }
        IdType::Channel(id) => {
            info!("fetching channel {}", id);
            let mut channel = fetch_channel(id, &api_key).await?;

            let upload_playlist = channel
                .content_details
                .take()
                .ok_or(eyre!("content_details is None"))?
                .related_playlists
                .ok_or(eyre!("related_playlists is None"))?
                .uploads
                .ok_or(eyre!("uploads is None"))?;

            let mut rss_channel = build_channel_from_yt_channel(channel);

            let max_fetched_items: usize =
                conf().get(ConfName::YoutubeMaxResults).unwrap().parse()?;
            let items = fetch_playlist_items(&upload_playlist, &api_key, max_fetched_items).await?;

            let duration_map = create_duration_url_map(&items, &api_key).await?;

            let min_seconds: u64 = conf()
                .get(ConfName::YoutubeMinSeconds)
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .unwrap_or(0);

            let items: Vec<PlaylistItem> = if min_seconds > 0 {
                items
                    .into_iter()
                    .filter(|it| {
                        let Some(snippet) = it.snippet.as_ref() else { return false };
                        let Some(res) = snippet.resource_id.as_ref() else { return true };
                        let Some(vid) = res.video_id.as_ref() else { return true };
                        duration_map
                            .get(vid)
                            .map(|v| duration_to_seconds(&v.duration) >= min_seconds)
                            .unwrap_or(true)
                    })
                    .collect()
            } else {
                items
            };

            apply_best_fit_itunes_category(&mut rss_channel, &duration_map);

            let rss_items = build_channel_items_from_playlist(items, duration_map);

            Ok((rss_channel, rss_items))
        }
    }
}

macro_rules! get_thumb {
    ($snippet:ident) => {
        $snippet.thumbnails.and_then(|thumbs| {
            thumbs
                .maxres
                .or(thumbs.high)
                .or(thumbs.medium)
                .or(thumbs.standard)
                .or(thumbs.default)
        })
    };
}

fn apply_best_fit_itunes_category(channel: &mut Channel, infos: &HashMap<String, VideoExtraInfo>) {
    // Pick the most common YouTube categoryId among the fetched videos and map to the closest
    // Apple Podcasts category. This is an approximation (YouTube categories don't match 1:1).
    let mut counts: HashMap<(String, Option<String>), usize> = HashMap::new();
    for info in infos.values() {
        if let Some(ref yt_id) = info.yt_category_id {
            if let Some((primary, secondary)) = map_yt_category_to_itunes(yt_id) {
                *counts.entry((primary, secondary)).or_insert(0) += 1;
            }
        }
    }

    let best = counts.into_iter().max_by_key(|(_, c)| *c).map(|(k, _)| k);
    let Some((primary, secondary)) = best else {
        return;
    };

    if let Some(ref mut itunes) = channel.itunes_ext {
        let mut cat = ITunesCategory::default();
        cat.set_text(&primary);

        // rss 2.0.12 models subcategory as Option<Box<ITunesCategory>> (one nested subcategory)
        if let Some(sub) = secondary {
            let mut subcat = ITunesCategory::default();
            subcat.set_text(&sub);
            cat.set_subcategory(Some(Box::new(subcat)));
        }

        itunes.set_categories(vec![cat]);
    }
}

fn map_yt_category_to_itunes(yt_category_id: &str) -> Option<(String, Option<String>)> {
    // YouTube video category IDs (common set) → Apple Podcasts categories (best-fit).
    // Ref: YouTube "videoCategories" list (IDs like 10 Music, 20 Gaming, etc.).
    // Apple only uses the first category/subcategory.
    match yt_category_id {
        "1" => Some(("TV & Film".to_string(), Some("Film Reviews".to_string()))), // Film & Animation
        "2" => Some(("Leisure".to_string(), Some("Automotive".to_string()))), // Autos & Vehicles
        "10" => Some(("Music".to_string(), None)),
        "15" => Some(("Kids & Family".to_string(), Some("Pets & Animals".to_string()))),
        "17" => Some(("Sports".to_string(), None)),
        "19" => Some(("Society & Culture".to_string(), Some("Places & Travel".to_string()))),
        "20" => Some(("Leisure".to_string(), Some("Video Games".to_string()))),
        "21" => Some(("Society & Culture".to_string(), Some("Personal Journals".to_string()))),
        "22" => Some(("Society & Culture".to_string(), None)),
        "23" => Some(("Comedy".to_string(), None)),
        "24" => Some(("TV & Film".to_string(), Some("After Shows".to_string()))),
        "25" => Some(("News".to_string(), None)),
        "26" => Some(("Arts".to_string(), Some("Fashion & Beauty".to_string()))),
        "27" => Some(("Education".to_string(), None)),
        "28" => Some(("Technology".to_string(), None)),
        "29" => Some(("Society & Culture".to_string(), Some("Philanthropy".to_string()))),
        _ => None,
    }
}

fn build_channel_from_yt_channel(channel: api::Channel) -> Channel {
    let mut channel_builder = ChannelBuilder::default();
    let mut itunes_channel_builder = ITunesChannelExtensionBuilder::default();

    if let Some(mut snippet) = channel.snippet {
        channel_builder.description(snippet.description.take().unwrap_or_default());
        channel_builder.title(snippet.title.take().unwrap_or_default());
        channel_builder.language(snippet.default_language.take());
        if let Some(mut thumb) = get_thumb!(snippet) {
            itunes_channel_builder.image(thumb.url.take());
        }
        itunes_channel_builder.explicit(Some("no".to_owned()));
    }

    channel_builder.link(format!(
        "https://www.youtube.com/channel/{}",
        channel.id.unwrap_or_default()
    ));

    channel_builder.itunes_ext(Some(itunes_channel_builder.build()));
    channel_builder.build()
}

async fn fetch_channel(id: String, api_key: &str) -> eyre::Result<api::Channel> {
    let hub = get_youtube_hub();
    let channel_request = hub
        .channels()
        .list(&vec!["snippet".into(), "contentDetails".into()])
        .clear_scopes()
        .max_results(1)
        .add_id(&id)
        .param("key", api_key);

    let result = channel_request.doit().await?;
    let channel = result
        .1
        .items
        .ok_or(eyre!("youtube returned no channel with id {:?}", id))?
        .first()
        .ok_or(eyre!("youtube returned no channel with id {:?}", id))?
        .clone();

    Ok(channel)
}

#[derive(Debug, Clone)]
struct VideoExtraInfo {
    duration: iso8601_duration::Duration,
    yt_category_id: Option<String>,
}

// iso8601_duration::Duration uses f32 fields (not Option).
fn iso_duration_to_hms(d: &iso8601_duration::Duration) -> (u64, u64, u64) {
    let mut h = if d.hour.is_finite() { d.hour.floor() as i64 } else { 0 };
    let mut m = if d.minute.is_finite() { d.minute.floor() as i64 } else { 0 };
    let mut s = if d.second.is_finite() { d.second.floor() as i64 } else { 0 };

    // normalize overflow
    if s >= 60 {
        m += s / 60;
        s %= 60;
    }
    if m >= 60 {
        h += m / 60;
        m %= 60;
    }

    (h.max(0) as u64, m.max(0) as u64, s.max(0) as u64)
}

fn duration_to_seconds(d: &iso8601_duration::Duration) -> u64 {
    let (h, m, s) = iso_duration_to_hms(d);
    h * 3600 + m * 60 + s
}

async fn create_duration_url_map(
    items: &[PlaylistItem],
    api_key: &str,
) -> Result<HashMap<String, VideoExtraInfo>, eyre::Error> {
    let ids_batches = items.chunks(50).map(|c| {
        c.iter()
            .filter_map(|i| i.snippet.clone()?.resource_id?.video_id)
    });

    let hub = get_youtube_hub();

    let videos_requests: Vec<_> = ids_batches
        .map(|batch| {
            let mut req = hub
                .videos()
                .list(&vec!["contentDetails".to_owned(), "snippet".to_owned()])
                .clear_scopes()
                .param("key", api_key);

            for video_id in batch {
                req = req.add_id(&video_id);
            }

            req.doit()
        })
        .collect();

    info!(
        "fetching video info for {} videos in {} batches",
        items.len(),
        videos_requests.len()
    );

    let video_infos = futures::future::join_all(videos_requests)
        .await
        .into_iter()
        .filter_map(Result::ok)
        .flat_map(|(_, resp)| resp.items.unwrap_or_default())
        .filter_map(|v| {
            Some((
                v.id?,
                VideoExtraInfo {
                    duration: iso8601_duration::Duration::parse(&v.content_details?.duration?)
                        .ok()?,
                    yt_category_id: v.snippet.and_then(|s| s.category_id),
                },
            ))
        })
        .collect::<HashMap<_, _>>();

    Ok(video_infos)
}

fn build_channel_items_from_playlist(
    items: Vec<PlaylistItem>,
    videos_infos: HashMap<String, VideoExtraInfo>,
) -> Vec<Item> {
    items
        .into_iter()
        .filter_map(|item| {
            let mut snippet = item.snippet?;
            let title = snippet.title.take().unwrap_or_default();
            let description = snippet.description.take().unwrap_or_default();
            let video_id = snippet.resource_id.take()?.video_id?;
            let url = Url::parse(&format!("https://www.youtube.com/watch?v={video_id}")).ok()?;

            let mut item_builder = ItemBuilder::default();
            item_builder.title(Some(title));
            item_builder.description(Some(description.clone()));
            item_builder.link(Some(url.to_string()));
            item_builder.guid(Some(GuidBuilder::default().value(url.to_string()).build()));
            item_builder.pub_date(
                snippet
                    .published_at
                    .map(|pub_date| pub_date.to_rfc2822().to_string()),
            );
            item_builder.author(snippet.channel_title.take());

            let video_infos = videos_infos.get(&video_id).or_else(|| {
                warn!("no duration found for {:?}", &video_id);
                None
            })?;

            let (h, m, s) = iso_duration_to_hms(&video_infos.duration);
            let duration_str = format!("{:02}:{:02}:{:02}", h, m, s);

            let itunes_item_extension = ITunesItemExtensionBuilder::default()
                .summary(Some(description))
                .duration(Some(duration_str))
                .image(get_thumb!(snippet).and_then(|t| t.url))
                .build();

            item_builder.itunes_ext(Some(itunes_item_extension));
            Some(item_builder.build())
        })
        .collect()
}

async fn fetch_playlist_items(
    playlist_id: &String,
    api_key: &String,
    max_fetched_items: usize,
) -> eyre::Result<Vec<PlaylistItem>> {
    let hub = get_youtube_hub();
    let max_consecutive_requests = (max_fetched_items / 50) + 1;
    let mut fetched_playlist_items: Vec<PlaylistItem> = Vec::with_capacity(max_fetched_items);
    let mut request_count = 0;
    let mut next_page_token: Option<String> = None;

    debug!("fetching items from playlist {}", playlist_id);

    loop {
        let remaining_items = max_fetched_items - fetched_playlist_items.len();
        let items_to_fetch = if remaining_items > 50 { 50 } else { remaining_items };

        let mut playlist_items_request = hub
            .playlist_items()
            .list(&vec!["snippet".into()])
            .clear_scopes()
            .playlist_id(playlist_id)
            .param("key", api_key)
            .max_results(items_to_fetch.try_into()?);

        if let Some(ref next_page_token) = next_page_token {
            playlist_items_request = playlist_items_request.page_token(next_page_token.as_str());
        }

        let response = playlist_items_request.doit().await?;

        fetched_playlist_items.extend(
            response
                .1
                .items
                .ok_or(eyre!("playlist object has no items field"))?,
        );
        next_page_token = response.1.next_page_token;

        if next_page_token.is_none() || request_count == max_consecutive_requests {
            info!(
                "fetched {} items, max items reached or no more items to fetch",
                fetched_playlist_items.len()
            );
            break;
        }
        request_count += 1;
    }

    info!(
        "fetched {} items, in {} requests",
        fetched_playlist_items.len(),
        request_count
    );

    fetched_playlist_items.sort_by_key(|i| i.snippet.as_ref().and_then(|s| s.published_at));
    Ok(fetched_playlist_items)
}

fn build_channel_from_playlist(playlist: api::Playlist) -> Channel {
    let mut channel_builder = ChannelBuilder::default();
    let mut itunes_channel_builder = ITunesChannelExtensionBuilder::default();

    if let Some(mut snippet) = playlist.snippet {
        channel_builder.description(snippet.description.take().unwrap_or_default());
        channel_builder.title(snippet.title.take().unwrap_or_default());
        channel_builder.language(snippet.default_language.take());

        channel_builder.link(format!(
            "https://www.youtube.com/playlist?list={}",
            playlist.id.unwrap_or_default()
        ));

        if let Some(mut thumb) = get_thumb!(snippet) {
            itunes_channel_builder.image(thumb.url.take());
        }
    }

    channel_builder.itunes_ext(Some(itunes_channel_builder.build()));
    channel_builder.build()
}

async fn fetch_playlist(id: String, api_key: &String) -> Result<api::Playlist, eyre::Error> {
    let hub = get_youtube_hub();
    let playlist_request = hub
        .playlists()
        .list(&vec!["snippet".into()])
        .clear_scopes()
        .add_id(&id)
        .param("key", api_key);

    let result = playlist_request.doit().await?;
    let playlist = result
        .1
        .items
        .ok_or(eyre!("youtube returned no playlist with id {:?}", id))?
        .first()
        .ok_or(eyre!("youtube returned no playlist with id {:?}", id))?
        .clone();

    Ok(playlist)
}

fn get_youtube_hub() -> YouTube<hyper_rustls::HttpsConnector<HttpConnector>> {
    #[derive(Clone)]
    struct NoAuth;

    impl common::GetToken for NoAuth {
        fn get_token<'a>(
            &'a self,
            _scopes: &'a [&str],
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            Option<String>,
                            Box<dyn std::error::Error + Send + Sync + 'static>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                // API-key only (we use `.clear_scopes()` on requests), so no OAuth token is needed.
                Ok(None)
            })
        }
    }

    type Body = BoxBody<Bytes, hyper::Error>;

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("native root certs")
        .https_only()
        .enable_http1()
        .build();

    let client: Client<_, Body> = Client::builder(TokioExecutor::new()).build(connector);

    YouTube::new(client, NoAuth)
}

// =========================
// Redis-cached wrappers
// =========================

#[io_cached(
    map_error = r##"|e| eyre::Error::new(e)"##,
    ty = "AsyncRedisCache<String, String>",
    create = r##" {
        AsyncRedisCache::new("cached_yt_stream_url=", std::time::Duration::from_secs(18000))
            .set_refresh(false)
            .set_connection_string(&conf().get(ConfName::RedisUrl).unwrap())
            .build()
            .await
            .expect("get_youtube_stream_url cache")
} "##
)]
async fn get_youtube_stream_url_cached(url: String) -> eyre::Result<String> {
    let url = Url::parse(&url)?;
    debug!("getting stream_url for yt video: {}", url);

    // yt-dlp tries to write to ~/.cache by default; inside the container we often run
    // as a non-root user with a read-only rootfs. Force a writable cache dir.
    let cache_dir = "/tmp/yt-dlp-cache";
    let _ = fs::create_dir_all(cache_dir).await;

    let extra_args: Vec<String> =
        serde_json::from_str(conf().get(ConfName::YoutubeYtDlpExtraArgs)?.as_str()).map_err(|_| {
            eyre!(r#"failed to parse YOUTUBE_YT_DLP_GET_URL_EXTRA_ARGS allowed syntax is ["arg1", "arg2", "arg3", ...]"#)
        })?;
    
    let mut command = Command::new("yt-dlp");
    command
        .arg("-f")
        .arg("bestaudio/best")
        .arg("--get-url")
        .arg("--cache-dir")
        .arg(cache_dir)
        .arg("--no-playlist")
        .arg(url.as_str());

    for arg in extra_args {
        command.arg(arg);
    }

    let output = command.output().await?;

    // If yt-dlp failed, DO NOT cache an empty string.
    if !output.status.success() {
        let stderr = std::str::from_utf8(&output.stderr).unwrap_or_default();
        let stdout = std::str::from_utf8(&output.stdout).unwrap_or_default();
        warn!(
            "yt-dlp --get-url failed (status={}): stdout='{}' stderr='{}'",
            output.status,
            stdout.trim(),
            stderr.trim()
        );
        return Err(eyre!("yt-dlp --get-url failed: {}", stderr.trim()));
    }

    // yt-dlp can return multiple URLs (newline-separated). Grab the first non-empty line.
    let stdout = std::str::from_utf8(&output.stdout).unwrap_or_default();
    let first = stdout.lines().map(|l| l.trim()).find(|l| !l.is_empty());

    match first {
        Some(line) => Ok(line.to_string()),
        None => {
            let stderr = std::str::from_utf8(&output.stderr).unwrap_or_default();
            warn!("yt-dlp returned empty stdout for --get-url; stderr='{}'", stderr.trim());
            Err(eyre!("yt-dlp returned empty stdout for --get-url"))
        }
    }
}

async fn get_youtube_stream_url(url: &Url) -> eyre::Result<Url> {
    let raw = get_youtube_stream_url_cached(url.as_str().to_string()).await?;
    Url::parse(raw.trim()).map_err(|e| {
        warn!("error while parsing stream url from cached yt-dlp output: {e}; raw={raw}");
        eyre::eyre!(e)
    })
}

async fn feed_url_for_yt_playlist(url: &Url) -> eyre::Result<Url> {
    let playlist_id = url
        .query_pairs()
        .find(|(key, _)| key == "list")
        .map(|(_, value)| value)
        .ok_or_else(|| eyre::eyre!("Failed to parse playlist ID from URL: {}", url))?;

    let mut feed_url = Url::parse("https://www.youtube.com/feeds/videos.xml").unwrap();
    feed_url
        .query_pairs_mut()
        .append_pair("playlist_id", &playlist_id);

    Ok(feed_url)
}

async fn feed_url_for_yt_atom(url: &Url) -> eyre::Result<Url> {
    Ok(url.clone())
}

async fn feed_url_for_yt_channel(url: &Url) -> eyre::Result<Url> {
    info!("trying to convert youtube channel url {}", url);
    if url.to_string().contains("feeds/videos.xml") {
        return Ok(url.to_owned());
    }
    let url_with_channel_id = find_yt_channel_url_with_c_id(url).await?;
    let channel_id = url_with_channel_id
        .path_segments()
        .ok_or_else(|| eyre!("could not parse channel id"))?
        .last()
        .ok_or_else(|| eyre!("could not parse channel id"))?;

    let mut feed_url = Url::parse("https://www.youtube.com/feeds/videos.xml")?;
    feed_url
        .query_pairs_mut()
        .append_pair("channel_id", channel_id);
    info!("converted to {feed_url}");
    Ok(feed_url)
}

#[cfg_attr(
    not(test),
    io_cached(
        map_error = r##"|e| eyre::Error::new(e)"##,
        ty = "AsyncRedisCache<String, String>",
        create = r##" {
        AsyncRedisCache::new("youtube_channel_username_to_id=", std::time::Duration::from_secs(9999999))
            .set_refresh(false)
            .set_connection_string(&conf().get(ConfName::RedisUrl).unwrap())
            .build()
            .await
            .expect("youtube_channel_username_to_id cache")
} "##
    )
)]
async fn find_yt_channel_url_with_c_id_cached(url: String) -> eyre::Result<String> {
    let url = Url::parse(&url)?;
    info!("conversion not in cache, using yt-dlp for conversion...");

    let cache_dir = "/tmp/yt-dlp-cache";
    let _ = fs::create_dir_all(cache_dir).await;

    let output = Command::new("yt-dlp")
        .arg("--cache-dir")
        .arg(cache_dir)
        .arg("--playlist-items")
        .arg("0")
        .arg("-O")
        .arg("playlist:channel_url")
        .arg(url.to_string())
        .output()
        .await?;

    let stdout = std::str::from_utf8(&output.stdout).unwrap_or_default();
    Ok(stdout.trim().to_string())
}

async fn find_yt_channel_url_with_c_id(url: &Url) -> eyre::Result<Url> {
    let raw = find_yt_channel_url_with_c_id_cached(url.as_str().to_string()).await?;
    Ok(Url::parse(raw.trim())?)
}

fn convert_atom_to_rss(feed: Feed, duration_map: HashMap<String, Option<usize>>) -> String {
    let mut feed_builder = provider::build_default_rss_structure();
    feed_builder.description(feed.description.map(|d| d.content).unwrap_or_default());
    feed_builder.title(feed.title.map(|d| d.content).unwrap_or_default());
    feed_builder.language(feed.language);

    let mut image_builder = ImageBuilder::default();
    image_builder.url(feed.icon.clone().map(|d| d.uri).unwrap_or_default());
    feed_builder.image(Some(image_builder.build()));
    feed_builder.link(
        feed.links
            .clone()
            .first()
            .map(|d| d.clone().href)
            .unwrap_or_default(),
    );

    let mut itunes_ext_builder = ITunesChannelExtensionBuilder::default();
    itunes_ext_builder.image(feed.icon.map(|d| d.uri));
    feed_builder.itunes_ext(Some(itunes_ext_builder.build()));

    let items = feed
        .entries
        .into_iter()
        .map(|entry| {
            let mut item_builder = ItemBuilder::default();
            item_builder.title(entry.title.map(|d| d.content));
            item_builder.description(
                entry
                    .media
                    .first()
                    .and_then(|d| Some(d.clone().description?.content)),
            );

            let link = entry.links.first().map(|d| d.clone().href);
            item_builder.link(link.clone());

            let mut itunes_item_builder = ITunesItemExtensionBuilder::default();
            let media = entry.media.first();
            itunes_item_builder.image(
                media
                    .and_then(|m| m.thumbnails.first())
                    .map(|t| t.clone().image.uri),
            );

            let duration = (|| -> Option<_> {
                duration_map.get(&link?).map(|s| {
                    s.map(|a| format!("{:02}:{:02}:{:02}", a / 3600, a / 60 % 60, a % 60))
                })
            })()
            .flatten();

            itunes_item_builder.duration(duration);
            item_builder.itunes_ext(Some(itunes_item_builder.build()));
            item_builder.guid(Some(GuidBuilder::default().value(entry.id).build()));
            item_builder.build()
        })
        .collect::<Vec<Item>>();

    feed_builder.items(items);
    feed_builder.build().to_string()
}

#[cfg_attr(
    not(test),
    io_cached(
        map_error = r##"|e| eyre::Error::new(e)"##,
        ty = "AsyncRedisCache<String, Option<usize>>",
        create = r##" {
        AsyncRedisCache::new("cached_yt_video_duration=", std::time::Duration::from_secs(86400))
            .set_refresh(false)
            .set_connection_string(&conf().get(ConfName::RedisUrl).unwrap())
            .build()
            .await
            .expect("youtube_duration cache")
} "##
    )
)]
async fn get_youtube_video_duration_with_ytdlp_cached(url: String) -> eyre::Result<Option<usize>> {
    let url = Url::parse(&url)?;
    debug!("getting duration for yt video: {}", url);

    let cache_dir = "/tmp/yt-dlp-cache";
    let _ = fs::create_dir_all(cache_dir).await;

    let output = Command::new("yt-dlp")
        .arg("--cache-dir")
        .arg(cache_dir)
        .arg("--get-duration")
        .arg(url.to_string())
        .output()
        .await;

    if let Ok(x) = output {
        let duration_str = std::str::from_utf8(&x.stdout).unwrap_or_default().trim().to_string();
        Ok(Some(
            parse_duration(&duration_str)
                .unwrap_or_default()
                .as_secs()
                .try_into()
                .unwrap_or(0),
        ))
    } else {
        warn!("could not parse youtube video duration");
        Ok(Some(0))
    }
}

async fn get_youtube_video_duration_with_ytdlp(url: &Url) -> eyre::Result<Option<usize>> {
    get_youtube_video_duration_with_ytdlp_cached(url.as_str().to_string()).await
}

fn parse_duration(duration_str: &str) -> Result<Duration, String> {
    let duration_parts: Vec<&str> = duration_str.split(':').rev().collect();

    let seconds: u64 = match duration_parts.first() {
        Some(sec_str) => sec_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    let minutes: u64 = match duration_parts.get(1) {
        Some(min_str) => min_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    let hours: u64 = match duration_parts.get(2) {
        Some(hour_str) => hour_str.parse().map_err(|_| "Invalid format".to_string())?,
        None => 0,
    };

    Ok(Duration::from_secs(hours * 3600 + minutes * 60 + seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_build_items_for_playlist_requires_api_key() {
        let id = "UUXuqSBlHAE6Xw-yeJA0Tunw".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        let items = fetch_playlist_items(&playlist.id.unwrap(), &api_key, 300)
            .await
            .unwrap();

        assert!(!items.is_empty())
    }

    #[tokio::test]
    async fn test_less_than_50_items_requires_api_key() {
        let id = "UUXuqSBlHAE6Xw-yeJA0Tunw".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        let items = fetch_playlist_items(&playlist.id.unwrap(), &api_key, 13)
            .await
            .unwrap();

        assert!(!items.is_empty());
        assert_eq!(items.len(), 13)
    }

    #[tokio::test]
    async fn test_less_than_300_items_requires_api_key() {
        let id = "UUXuqSBlHAE6Xw-yeJA0Tunw".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        let items = fetch_playlist_items(&playlist.id.unwrap(), &api_key, 50)
            .await
            .unwrap();

        assert!(!items.is_empty());
        assert_eq!(items.len(), 50)
    }

    #[tokio::test]
    async fn test_more_than_300_items_requires_api_key() {
        let id = "UUXuqSBlHAE6Xw-yeJA0Tunw".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        let items = fetch_playlist_items(&playlist.id.unwrap(), &api_key, 600)
            .await
            .unwrap();

        assert!(!items.is_empty());
        assert_eq!(items.len(), 600)
    }

    #[tokio::test]
    async fn test_build_channel_for_playlist_requires_api_key() {
        let id = "PLJmimp-uZX42T7ONp1FLXQDJrRxZ-_1Ct".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let playlist = fetch_playlist(id, &api_key).await.unwrap();

        let channel = build_channel_from_playlist(playlist);

        assert!(!channel.description.is_empty());
        assert!(!channel.title.is_empty());
        assert!(channel.itunes_ext.unwrap().image.is_some());
    }

    #[tokio::test]
    async fn test_fetch_playlist_requires_api_key() {
        let id = "PLJmimp-uZX42T7ONp1FLXQDJrRxZ-_1Ct".to_string();
        let api_key = conf().get(ConfName::YoutubeApiKey).unwrap();

        let result = fetch_playlist(id, &api_key).await;
        assert!(result.is_ok());

        if let Ok(playlist) = result {
            assert!(playlist.id.is_some());
            assert!(playlist.snippet.is_some());
        }
    }

    #[tokio::test]
    async fn test_fetch_youtube_channel_by_name_requires_api_key() {
        let provider = YoutubeProvider;
        let Ok(_api_key) = conf().get(ConfName::YoutubeApiKey) else {
            panic!("to run this test you need to set an api key for youtube.");
        };

        let result = provider
            .generate_rss_feed(Url::parse("https://www.youtube.com/@LegalEagle").unwrap())
            .await;
        assert!(result.is_ok());

        let channel = rss::Channel::read_from(result.unwrap().as_bytes()).unwrap();
        assert!(channel.items.len() > 50);
        for item in &channel.items {
            assert!(item.title.is_some());
            assert!(item.description.is_some());
        }
    }
}
