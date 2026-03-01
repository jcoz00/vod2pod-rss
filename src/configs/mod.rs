use log::warn;
use serde::Serialize;

pub fn conf() -> impl Conf {
    EnvConf {}
}

pub trait Conf {
    fn get(&self, key: ConfName) -> eyre::Result<String>;
}

pub enum ConfName {
    RedisAddress,
    RedisPort,
    RedisUrl,
    Mp3Bitrate,
    YoutubeApiKey,
    YoutubeMaxResults,
    TwitchClientId,
    TwitchSecretKey,
    TranscodingEnabled,
    SubfolderPath,
    ValidUrlDomains,
    AudioCodec,
    PeerTubeValidHosts,
    YoutubeYtDlpExtraArgs,
    /// Minimum video duration (in seconds) to include in generated YouTube RSS feeds.
    /// Useful for excluding YouTube Shorts.
    YoutubeMinSeconds,
    /// Convenience flag to exclude YouTube Shorts (defaults to false). If true and
    /// `YOUTUBE_MIN_SECONDS` is not set (or is 0), a default minimum of 61 seconds is used.
    YoutubeExcludeShorts,
    // Apple / Podcast feed metadata
    PodcastLanguage,
    ItunesOwnerName,
    ItunesOwnerEmail,
    ItunesCategory,
    ItunesExplicit,
    ItunesType,

    // Podcasting 2.0 features
    PodcastChaptersEnabled,
    PodcastTranscriptsEnabled,

    /// Maximum number of items to include when generating a Rumble RSS feed (defaults to 300).
    RumbleMaxResults,
    /// Minimum video duration (in seconds) to include in generated Rumble RSS feeds.
    /// Useful for excluding short-form clips.
    RumbleMinSeconds,
    /// Additional arguments to pass to yt-dlp when enumerating a Rumble channel/playlist.
    /// Format: JSON array of strings.
    RumbleYtDlpListExtraArgs,
    /// Additional arguments to pass to yt-dlp when extracting a direct media URL from a Rumble video.
    /// Format: JSON array of strings.
    RumbleYtDlpGetUrlExtraArgs,
    CacheTTL,
    FfmpegTimeoutSeconds,
    /// Optional FFmpeg audio filter chain applied during transcoding.
    /// Example: dynaudnorm=f=150:g=12:m=10,alimiter=limit=0.98
    FfmpegAudioFilter,
}

struct EnvConf {}

impl Conf for EnvConf {
    fn get(&self, key: ConfName) -> eyre::Result<String> {
        match key {
            ConfName::RedisAddress => {
                Ok(std::env::var("REDIS_ADDRESS").unwrap_or_else(|_| "localhost".to_string()))
            }
            ConfName::RedisPort => {
                Ok(std::env::var("REDIS_PORT").unwrap_or_else(|_| "6379".to_string()))
            }
            ConfName::RedisUrl => {
                let redis_address = conf().get(ConfName::RedisAddress).unwrap();
                let redis_port = conf().get(ConfName::RedisPort).unwrap();
                Ok(format!("redis://{redis_address}:{redis_port}/"))
            }
            ConfName::Mp3Bitrate => {
                Ok(std::env::var("MP3_BITRATE").unwrap_or_else(|_| "192".to_string()))
            }
            ConfName::TwitchClientId => std::env::var("TWITCH_CLIENT_ID")
                .map_err(|e| eyre::eyre!(e))
                .and_then(|s| {
                    if s.is_empty() {
                        Err(eyre::eyre!("no TwitchClientId api key"))
                    } else {
                        Ok(s)
                    }
                }),
            ConfName::TwitchSecretKey => std::env::var("TWITCH_SECRET")
                .map_err(|e| eyre::eyre!(e))
                .and_then(|s| {
                    if s.is_empty() {
                        Err(eyre::eyre!("no TwitchSecretKey api key"))
                    } else {
                        Ok(s)
                    }
                }),
            ConfName::YoutubeApiKey => std::env::var("YT_API_KEY")
                .map_err(|e| eyre::eyre!(e))
                .and_then(|s| {
                    if s.is_empty() {
                        Err(eyre::eyre!("no youtube api key"))
                    } else {
                        Ok(s)
                    }
                }),
            ConfName::TranscodingEnabled => {
                Ok(std::env::var("TRANSCODE").unwrap_or_else(|_| "False".to_string()))
            }
            ConfName::SubfolderPath => {
                let mut folder = std::env::var("SUBFOLDER").unwrap_or("".to_string());
                if !folder.starts_with('/') {
                    folder.insert(0, '/');
                }
                while folder.ends_with('/') {
                    folder.pop();
                }
                Ok(folder)
            }
            ConfName::ValidUrlDomains => {
                Ok(std::env::var("VALID_URL_DOMAINS").unwrap_or_else(|_| "".to_string()))
            }
            ConfName::AudioCodec => Ok(std::env::var("AUDIO_CODEC")
                .map(|c| match c.as_str() {
                    "MP3" => c,
                    "OPUS" => c,
                    "OGG" => "OGG_VORBIS".to_string(),
                    "VORBIS" => "OGG_VORBIS".to_string(),
                    "OGG_VORBIS" => c,
                    _ => {
                        warn!("Unrecognized codec \"{c}\". Defaulting to MP3.");
                        "MP3".to_string()
                    }
                })
                .unwrap_or_else(|_| "MP3".to_string())),
            ConfName::PeerTubeValidHosts => {
                Ok(std::env::var("PEERTUBE_VALID_DOMAINS").unwrap_or_else(|_| "".to_string()))
            }
            ConfName::YoutubeMaxResults => {
                Ok(std::env::var("YOUTUBE_MAX_RESULTS").unwrap_or_else(|_| "300".to_string()))
            }
            ConfName::YoutubeYtDlpExtraArgs => {
                Ok(std::env::var("YOUTUBE_YT_DLP_GET_URL_EXTRA_ARGS")
                    .unwrap_or_else(|_| "[]".to_string()))
            }
            ConfName::YoutubeMinSeconds => {
                Ok(std::env::var("YOUTUBE_MIN_SECONDS").unwrap_or_else(|_| "0".to_string()))
            }
            ConfName::YoutubeExcludeShorts => {
                Ok(std::env::var("YOUTUBE_EXCLUDE_SHORTS").unwrap_or_else(|_| "false".to_string()))
            }
            ConfName::PodcastLanguage => Ok(std::env::var("PODCAST_LANGUAGE").unwrap_or_else(|_| "en".to_string())),
            ConfName::ItunesOwnerName => Ok(std::env::var("ITUNES_OWNER_NAME").unwrap_or_default()),
            ConfName::ItunesOwnerEmail => Ok(std::env::var("ITUNES_OWNER_EMAIL").unwrap_or_default()),
            ConfName::ItunesCategory => Ok(std::env::var("ITUNES_CATEGORY").unwrap_or_else(|_| "News".to_string())),
            ConfName::ItunesExplicit => Ok(std::env::var("ITUNES_EXPLICIT").unwrap_or_else(|_| "false".to_string())),
            ConfName::ItunesType => Ok(std::env::var("ITUNES_TYPE").unwrap_or_else(|_| "episodic".to_string())),
            ConfName::PodcastChaptersEnabled => Ok(std::env::var("PODCAST_CHAPTERS").unwrap_or_else(|_| "false".to_string())),
            ConfName::PodcastTranscriptsEnabled => Ok(std::env::var("PODCAST_TRANSCRIPTS").unwrap_or_else(|_| "false".to_string())),

            ConfName::RumbleMaxResults => {
                Ok(std::env::var("RUMBLE_MAX_RESULTS").unwrap_or_else(|_| "300".to_string()))
            }
            ConfName::RumbleMinSeconds => {
                Ok(std::env::var("RUMBLE_MIN_SECONDS").unwrap_or_else(|_| "0".to_string()))
            }
            ConfName::RumbleYtDlpListExtraArgs => {
                Ok(std::env::var("RUMBLE_YT_DLP_LIST_EXTRA_ARGS").unwrap_or_else(|_| "[]".to_string()))
            }
            ConfName::RumbleYtDlpGetUrlExtraArgs => {
                Ok(std::env::var("RUMBLE_YT_DLP_GET_URL_EXTRA_ARGS").unwrap_or_else(|_| "[]".to_string()))
            }
            ConfName::CacheTTL => {
                Ok(std::env::var("CACHE_TTL").unwrap_or_else(|_| "600".to_string()))
            }
            ConfName::FfmpegTimeoutSeconds => {
                Ok(std::env::var("FFMPEG_TIMEOUT_SECONDS").unwrap_or_else(|_| "300".to_string()))
            }
            ConfName::FfmpegAudioFilter => {
                Ok(std::env::var("FFMPEG_AUDIO_FILTER").unwrap_or_else(|_| "".to_string()))
            }
        }
    }
}

#[derive(Serialize, Clone, Copy)]
pub enum AudioCodec {
    MP3,
    Opus,
    OGGVorbis,
}

impl AudioCodec {
    pub fn get_ffmpeg_codec_str(&self) -> &'static str {
        match self {
            AudioCodec::MP3 => "libmp3lame",
            AudioCodec::Opus => {
                warn!("seeking is not supported with OPUS codec  ");
                "libopus"
            }
            AudioCodec::OGGVorbis => {
                warn!("seeking is not supported with OGG_VORBIS codec ... ");
                "libvorbis"
            }
        }
    }

    pub fn get_extension_str(&self) -> &'static str {
        match self {
            AudioCodec::MP3 => "mp3",
            AudioCodec::Opus => "webm",
            AudioCodec::OGGVorbis => "webm",
        }
    }

    pub fn get_mime_type_str(&self) -> &'static str {
        match self {
            AudioCodec::MP3 => "audio/mpeg",
            AudioCodec::Opus => "audio/webm",
            AudioCodec::OGGVorbis => "audio/webm",
        }
    }
}

impl From<String> for AudioCodec {
    fn from(value: String) -> Self {
        match value.as_str() {
            "MP3" => AudioCodec::MP3,
            "OPUS" => AudioCodec::Opus,
            "OGG_VORBIS" => AudioCodec::OGGVorbis,
            _ => AudioCodec::MP3,
        }
    }
}

impl Default for AudioCodec {
    fn default() -> Self {
        Self::MP3
    }
}
