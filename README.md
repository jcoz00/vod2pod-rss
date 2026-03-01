# vod2pod-rss (fork)

Convert **YouTube**, **Twitch**, **Rumble**, and existing **RSS/Atom** feeds into a podcast RSS feed (optionally transcoding audio to MP3). Designed for always‑on self‑hosting behind a reverse proxy (Traefik, Nginx, etc.).

## What’s included in this fork

### Providers
- **YouTube** channels/playlists (via YouTube Data API when configured; falls back to yt-dlp when needed)
- **Twitch** channels
- **Rumble** channels (e.g. `https://rumble.com/c/<channel>`) *(new)*
- **Existing RSS/Atom** feeds (pass-through with optional transcoding)

### Apple Podcasts-friendly RSS
This fork outputs RSS 2.0 feeds with:
- iTunes + PodcastIndex namespaces (`itunes:` + `podcast:`)
- Professional show/episode metadata (title, description, image, author/owner, category, explicit, etc.)
- Optional **chapters** (`<podcast:chapters>`) and **transcripts** (`<podcast:transcript>`) when enabled *(new)*

### Quality-of-life features
- **Shorts filtering (YouTube)** via environment variables *(new)*  
  Exclude Shorts by applying a minimum duration threshold.
- **Audio processing during transcoding** *(new)*  
  Optional FFmpeg filter chain for loudness leveling / dynamic normalization.
- **Rumble compatibility** *(new)*  
  Rumble feed generation and fetching of audio URLs using yt-dlp; plus optional FFmpeg header injection for Rumble.
- **Optional yt-dlp auto-update sidecar** *(new)*  
  Updates yt-dlp during off‑peak window and **skips restart when active streaming is detected**.

---

## Quickstart (Docker Compose)

1) Copy the example compose file and edit env vars:
- `docker-compose.yml`
- `.env` (recommended)

2) Run:
```bash
docker compose up -d
```

### URLs
- Web UI: `https://<host>/`
- RSS (cached): `https://<host>/rss?url=<encoded_source_url>`
- RSS (forced transcode): `https://<host>/transcodize_rss?url=<encoded_source_url>`
- Media stream (generated in feed): `/podcast/...` and `/transcode_media/...`

Supported source URLs include:
- YouTube: `https://www.youtube.com/@channel` / `https://www.youtube.com/c/channelname`
- Twitch: `https://www.twitch.tv/channelname`
- Rumble: `https://rumble.com/c/channelname`
- RSS/Atom feed URL

---

## Configuration

### Core
| Variable | Default | Notes |
|---|---:|---|
| `TRANSCODE` | `true` | Set `false` to disable transcoding (feeds only). |
| `MP3_BITRATE` | `192` | Bitrate for MP3 output when transcoding. |
| `SUBFOLDER` | `/` | Root path when behind a reverse proxy. |
| `VALID_URL_DOMAINS` | (built-in) | Comma-separated allowlist for input URLs. |
| `ALLOWED_MEDIA_HOSTS` | (built-in) | Comma-separated allowlist for media hostnames (enclosure URLs). |
| `CACHE_TTL` | `600` | Cache TTL for generated feeds (seconds). |

### YouTube
| Variable | Default | Notes |
|---|---:|---|
| `YT_API_KEY` | *(empty)* | Enables higher result limits + richer metadata. |
| `YOUTUBE_MAX_RESULTS` | `15` | Max items to fetch (higher with API key). |
| `YOUTUBE_MIN_SECONDS` | `0` | Minimum duration in seconds (filters short videos). |
| `YOUTUBE_EXCLUDE_SHORTS` | `false` | If `true`, forces `YOUTUBE_MIN_SECONDS` to at least **61s** (unless you set a higher min). |
| `YOUTUBE_YT_DLP_GET_URL_EXTRA_ARGS` | `[]` | JSON array of extra args passed to yt-dlp when extracting YouTube audio URLs. |

**Tip (exclude Shorts):**
- Set `YOUTUBE_EXCLUDE_SHORTS=true` (easy mode), or
- Set `YOUTUBE_MIN_SECONDS=180` (or any threshold you prefer).

> Note: yt-dlp’s `--match-filter` does *not* reliably remove Shorts from the **feed list** because the feed list is built from the YouTube API/video metadata. Filtering needs to happen at the feed-generation layer (this fork does that with `YOUTUBE_MIN_SECONDS`/`YOUTUBE_EXCLUDE_SHORTS`).

### Rumble
| Variable | Default | Notes |
|---|---:|---|
| `RUMBLE_MAX_RESULTS` | `25` | Max items per Rumble channel. |
| `RUMBLE_MIN_SECONDS` | `0` | Minimum duration in seconds (filters short items). |
| `RUMBLE_YT_DLP_GET_URL_EXTRA_ARGS` | `[]` | JSON array of extra args passed to yt-dlp when extracting Rumble audio URLs. |

### Audio processing (FFmpeg)
When `TRANSCODE=true`, you can apply a filter chain:
| Variable | Default | Notes |
|---|---:|---|
| `FFMPEG_AUDIO_FILTER` | *(empty)* | FFmpeg `-af` filter string (applied during transcode). |

Example (gentle leveling for spoken content):
```bash
FFMPEG_AUDIO_FILTER=dynaudnorm=f=150:g=12:m=10,alimiter=limit=0.98
```

### Apple Podcasts / RSS metadata
These populate `<channel>` and `<item>` tags using Apple’s RSS expectations.

| Variable | Notes |
|---|---|
| `PODCAST_TITLE` | Show title (`<title>` / `<itunes:title>`). |
| `PODCAST_DESCRIPTION` | Show description (`<description>` / `<itunes:summary>`). |
| `PODCAST_IMAGE_URL` | Artwork URL (`<itunes:image href="...">`). |
| `PODCAST_LANGUAGE` | ISO 639 language code (`en`, `en-us`, etc.). |
| `PODCAST_CATEGORY` | Apple category string (e.g. `Society &amp; Culture`). |
| `PODCAST_AUTHOR` | `<itunes:author>`. |
| `PODCAST_OWNER_NAME` / `PODCAST_OWNER_EMAIL` | `<itunes:owner>`. |
| `PODCAST_EXPLICIT` | `true`/`false` (`<itunes:explicit>`). |
| `PODCAST_TYPE` | `episodic` (default) or `serial`. |
| `PODCAST_WEBSITE` | `<link>` for the show. |
| `PODCAST_COPYRIGHT` | `<copyright>`. |
| `PODCAST_GENERATOR` | `<generator>`. |

### Chapters + transcripts (optional)
| Variable | Default | Notes |
|---|---:|---|
| `PODCAST_CHAPTERS` | `false` | If `true`, emits `<podcast:chapters>` links when chapter data is available. |
| `PODCAST_TRANSCRIPTS` | `false` | If `true`, emits `<podcast:transcript>` links when transcript data is available. |

---

## yt-dlp PO Token provider (YouTube 403 mitigation)
If you’re using a PO token provider (bgutil), pass yt-dlp extractor args (example):
```yaml
YOUTUBE_YT_DLP_GET_URL_EXTRA_ARGS: >-
  ["--extractor-args","youtube:po_token_provider=bgutil:http;base_url=http://127.0.0.1:4416;player_client=android_vr,web,mweb"]
```
Run the provider in the same network namespace as the main container or ensure `base_url` is reachable from within `vod2pod`.

---

## yt-dlp auto-update (recommended: sidecar)
This fork includes a helper script and a compose service that:
- Runs a daily check (e.g. noon) to see if yt-dlp updates
- Schedules restart in a maintenance window (e.g. 04:10–05:30)
- Skips restart if a stream was active recently (guard window)

See `docker-compose.yml` and `scripts/yt-dlp-updater.sh` for the reference implementation and knobs.

---

## Notes / limitations
- Apple Podcasts will display up to 2,000 valid episodes from a feed.
- Chapters and transcripts require upstream metadata availability (or future enhancement to generate them).
- Rumble support depends on yt-dlp extraction and may require additional headers/args for some content.

---

## License
Upstream is MIT; this fork remains MIT.
