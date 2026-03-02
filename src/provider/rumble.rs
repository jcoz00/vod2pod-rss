// rumble.rs — drop-in replacement (adds async Command, fixes URL type consistency, removes unused imports)

use std::io::Cursor;

use chrono::{TimeZone, Utc};
use eyre::eyre;
use log::{debug, info, warn};
use regex::Regex;
use rss::{ChannelBuilder, Guid, ItemBuilder};
use serde_json::Value;
use tokio::process::Command;

use reqwest::Url;

use crate::provider::MediaProvider;

#[derive(Default)]
pub struct RumbleProvider;

impl RumbleProvider {
    fn channel_url_regexes() -> Vec<Regex> {
        vec![
            Regex::new(r"^https?://(www\.)?rumble\.com/(c|user)/[A-Za-z0-9._-]+(?:(?:/|\?).*)?$")
                .unwrap(),
        ]
    }

    fn max_results() -> usize {
        std::env::var("RUMBLE_MAX_RESULTS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(300)
    }

    fn min_seconds() -> u64 {
        std::env::var("RUMBLE_MIN_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    }

    fn list_extra_args() -> Vec<String> {
        let raw = std::env::var("RUMBLE_YT_DLP_LIST_EXTRA_ARGS")
            .unwrap_or_else(|_| "[]".to_string());
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    fn get_url_extra_args() -> Vec<String> {
        let raw = std::env::var("RUMBLE_YT_DLP_GET_URL_EXTRA_ARGS")
            .unwrap_or_else(|_| "[]".to_string());
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    fn val_str(v: &Value, key: &str) -> Option<String> {
        v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
    }

    fn val_u64(v: &Value, key: &str) -> Option<u64> {
        v.get(key).and_then(|x| x.as_u64())
    }

    async fn ytdlp_list_entries(url: &str, max_results: usize) -> eyre::Result<Vec<Value>> {
        let mut cmd = Command::new("yt-dlp");
        cmd.arg("--no-warnings")
            .arg("--flat-playlist")
            .arg("--dump-json")
            .arg("--playlist-end")
            .arg(max_results.to_string());

        for a in Self::list_extra_args() {
            cmd.arg(a);
        }
        cmd.arg(url);

        debug!("rumble yt-dlp list cmd: {:?}", cmd);
        let out = cmd.output().await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(eyre!("yt-dlp list failed: {stderr}"));
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut entries = Vec::new();
        for line in stdout.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(v) => entries.push(v),
                Err(e) => warn!("failed parsing yt-dlp json line: {e}"),
            }
        }
        Ok(entries)
    }

    async fn build_rss(url: &str) -> eyre::Result<String> {
        let max_results = Self::max_results();
        let min_seconds = Self::min_seconds();

        info!("starting rumble feed generation for {url}");
        let entries = Self::ytdlp_list_entries(url, max_results).await?;
        if entries.is_empty() {
            return Err(eyre!(
                "No items returned for Rumble URL (expected /c/<name> or /user/<name>)"
            ));
        }

        let first = &entries[0];
        let channel_title = Self::val_str(first, "uploader")
            .or_else(|| Self::val_str(first, "channel"))
            .or_else(|| Self::val_str(first, "uploader_id"))
            .unwrap_or_else(|| "Rumble".to_string());

        let mut rss_items = Vec::new();
        for e in entries {
            if let Some(d) = Self::val_u64(&e, "duration") {
                if d < min_seconds {
                    continue;
                }
            } else if min_seconds > 0 {
                // If duration is missing and filtering is enabled, skip conservatively.
                continue;
            }

            let link = Self::val_str(&e, "webpage_url")
                .or_else(|| Self::val_str(&e, "url"))
                .or_else(|| Self::val_str(&e, "original_url"));
            let Some(link) = link else { continue };

            let title = Self::val_str(&e, "title").unwrap_or_else(|| link.clone());

            let pub_date = Self::val_u64(&e, "timestamp")
                .and_then(|ts| Utc.timestamp_opt(ts as i64, 0).single())
                .map(|dt| dt.to_rfc2822());

            let guid = Self::val_str(&e, "id").map(|id| Guid {
                value: id,
                permalink: false,
            });

            let mut b = ItemBuilder::default();
            b.title(Some(title)).link(Some(link)).guid(guid);

            if let Some(pd) = pub_date {
                b.pub_date(Some(pd));
            }
            if let Some(desc) = Self::val_str(&e, "description") {
                b.description(Some(desc));
            }

            rss_items.push(b.build());
        }

        let channel = ChannelBuilder::default()
            .title(channel_title)
            .link(url.to_string())
            .items(rss_items)
            .build();

        let mut buf = Vec::new();
        channel
            .write_to(Cursor::new(&mut buf))
            .map_err(|e| eyre!("failed writing rss: {e}"))?;

        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    async fn extract_direct_url(video_url: &Url) -> eyre::Result<Url> {
        let mut cmd = Command::new("yt-dlp");

        // Use -g to print the direct URL.
        // Prefer audio, but allow fallback.
        cmd.arg("-g")
            .arg("--no-warnings")
            .arg("-f")
            .arg("bestaudio/best")
            .arg("--no-playlist");

        for a in Self::get_url_extra_args() {
            cmd.arg(a);
        }

        cmd.arg(video_url.as_str());
        debug!("rumble yt-dlp get-url cmd: {:?}", cmd);

        let out = cmd.output().await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(eyre!("yt-dlp get-url failed: {stderr}"));
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .ok_or_else(|| eyre!("yt-dlp did not return a URL"))?;

        Ok(Url::parse(line.trim())?)
    }
}

#[async_trait::async_trait]
impl MediaProvider for RumbleProvider {
    fn domain_whitelist_regexes(&self) -> Vec<Regex> {
        Self::channel_url_regexes()
    }

    async fn generate_rss_feed(&self, url: Url) -> eyre::Result<String> {
        Self::build_rss(url.as_str()).await
    }

    async fn get_stream_url(&self, media_url: &Url) -> eyre::Result<Url> {
        Self::extract_direct_url(media_url).await
    }
}
