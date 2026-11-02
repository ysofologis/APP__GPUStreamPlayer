//! Track metadata for the info panel.
//!
//! Two sources, chosen by what the URL is:
//!
//! * **Extractor sites** (YouTube, SoundCloud, ...) -> `yt-dlp -J`, which gives
//!   title, channel, view count, upload date, thumbnail, tags, ...
//! * **Everything else** (local files, direct audio/video, HLS/DASH) -> `ffprobe`
//!   for stream facts (codec, resolution, bitrate, sample rate) plus container
//!   tags (title/artist/album when the file carries them).
//!
//! Artwork is normalised to a local file on disk so the UI can hand a plain path
//! to `gpui::img`, rather than depending on GPUI's URI image loading.
//!
//! All functions here block on subprocesses; call them from a background task.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::source::{self, SourceKind};

/// Everything the info panel can render. All fields are optional because no
/// single source type provides all of them.
#[derive(Debug, Clone, Default)]
pub struct TrackInfo {
    pub title: Option<String>,
    /// Channel for video sites, artist for music.
    pub author: Option<String>,
    pub description: Option<String>,

    /// Container/format label, e.g. `mp4`, `matroska,webm`, `hls`.
    pub container: Option<String>,
    /// Video codec, e.g. `h264`, `vp9`, `av1`.
    pub video_codec: Option<String>,
    /// Audio codec, e.g. `aac`, `opus`, `mp3`.
    pub audio_codec: Option<String>,
    pub resolution: Option<String>,
    pub bitrate: Option<String>,
    pub sample_rate: Option<String>,
    pub channels: Option<String>,

    pub duration: Duration,
    /// Where the info came from: `yt-dlp`, `ffprobe`, or a mix.
    pub provider: Option<String>,
    pub upload_date: Option<String>,
    pub view_count: Option<u64>,
    pub like_count: Option<u64>,
    pub categories: Vec<String>,
    pub tags: Vec<String>,

    /// Local file holding cover art / poster frame, ready for `img()`.
    pub artwork: Option<PathBuf>,
}

/// Fetch metadata for a resolved source.
pub fn fetch(kind: &SourceKind, probe_duration: Option<f64>) -> TrackInfo {
    let mut info = match kind {
        SourceKind::Resolved { page, stream: _ } => fetch_from_extractor(page),
        _ => TrackInfo::default(),
    };

    // Container/stream facts come from ffprobe either way; for extractor sites
    // it describes the resolved media file, which is what is actually playing.
    let playable = kind.playable_url();
    if let Some(technical) = probe_technical(&playable) {
        info.merge_technical(technical);
    }

    if info.duration.is_zero() {
        if let Some(seconds) = probe_duration {
            info.duration = Duration::from_secs_f64(seconds.max(0.0));
        }
    }

    // Local files and local media get a poster frame when there is no artwork.
    if info.artwork.is_none() {
        if let SourceKind::LocalFile(path) = kind {
            info.artwork = extract_poster(path, &playable);
        }
    }

    info
}

impl TrackInfo {
    fn merge_technical(&mut self, other: TrackInfo) {
        // Prefer extractor-provided text (nicer titles/names), fill the rest
        // from ffprobe.
        self.container = self.container.take().or(other.container);
        self.video_codec = other.video_codec;
        self.audio_codec = other.audio_codec;
        self.resolution = other.resolution;
        self.bitrate = self.bitrate.take().or(other.bitrate);
        self.sample_rate = other.sample_rate;
        self.channels = other.channels;
        if self.duration.is_zero() {
            self.duration = other.duration;
        }
        if self.provider.is_none() {
            self.provider = other.provider;
        }
    }
}

// ---------------------------------------------------------------------------
// yt-dlp
// ---------------------------------------------------------------------------

fn fetch_from_extractor(page: &str) -> TrackInfo {
    if !source::binary_exists("yt-dlp") {
        return TrackInfo::default();
    }

    let mut command = Command::new("yt-dlp");
    command.args(["--no-playlist", "--no-warnings", "-J"]);
    command.args(source::yt_dlp_cookie_args());
    command.arg(page);
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    let Ok(output) = output else {
        return TrackInfo::default();
    };
    if !output.status.success() {
        return TrackInfo::default();
    }

    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return TrackInfo::default();
    };

    let text = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_string);

    let mut info = TrackInfo {
        title: text("title"),
        author: text("channel").or_else(|| text("uploader")),
        description: text("description").map(truncate_description),
        provider: Some("yt-dlp".into()),
        duration: json
            .get("duration")
            .and_then(|v| v.as_f64())
            .map(|s| Duration::from_secs_f64(s.max(0.0)))
            .unwrap_or_default(),
        view_count: json.get("view_count").and_then(|v| v.as_u64()),
        like_count: json.get("like_count").and_then(|v| v.as_u64()),
        ..Default::default()
    };

    if let Some(date) = text("upload_date") {
        info.upload_date = format_upload_date(&date);
    }

    // Categories and tags: prefer a single category over the full tag soup.
    if let Some(cat) = json.get("categories").and_then(|v| v.as_array()) {
        info.categories = cat
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect();
    }
    if let Some(tags) = json.get("tags").and_then(|v| v.as_array()) {
        info.tags = tags
            .iter()
            .filter_map(|v| v.as_str())
            .take(12)
            .map(str::to_string)
            .collect();
    }

    // Download the thumbnail so the panel does not depend on GPUI's HTTP image
    // loading (and so it keeps working once the CDN URL expires).
    if let Some(thumb) = text("thumbnail") {
        info.artwork = download_artwork(&thumb, page);
    }

    info
}

/// `20141110` -> `2014-11-10`.
fn format_upload_date(raw: &str) -> Option<String> {
    if raw.len() == 8 {
        let (y, m, d) = (&raw[0..4], &raw[4..6], &raw[6..8]);
        if raw.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("{y}-{m}-{d}"));
        }
    }
    Some(raw.to_string())
}

fn truncate_description(text: String) -> String {
    // Collapse the newline-heavy descriptions YouTube returns.
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let max = 420;
    if single_line.chars().count() <= max {
        single_line
    } else {
        let cut: String = single_line.chars().take(max).collect();
        format!("{}…", cut.trim_end())
    }
}

// ---------------------------------------------------------------------------
// ffprobe
// ---------------------------------------------------------------------------

fn probe_technical(url: &str) -> Option<TrackInfo> {
    if !source::binary_exists("ffprobe") {
        return None;
    }

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration,bit_rate,format_name,size:format_tags=title,artist,album,date,genre:stream=codec_name,codec_type,width,height,sample_rate,channels,bit_rate,display_aspect_ratio",
            "-of",
            "json",
            url,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let mut info = TrackInfo {
        provider: Some("ffprobe".into()),
        ..Default::default()
    };

    if let Some(format) = json.get("format") {
        info.container = format
            .get("format_name")
            .and_then(|v| v.as_str())
            .map(|s| s.split(',').next().unwrap_or(s).to_string());

        if let Some(duration) = format.get("duration").and_then(|v| v.as_str()) {
            if let Ok(secs) = duration.parse::<f64>() {
                info.duration = Duration::from_secs_f64(secs.max(0.0));
            }
        }

        if let Some(bitrate) = format.get("bit_rate").and_then(|v| v.as_str()) {
            info.bitrate = parse_u64(bitrate).map(format_bitrate);
        }

        if let Some(tags) = format.get("tags") {
            // Container tags are usually capitalised differently per format.
            let tag = |name: &str| {
                tags.as_object().and_then(|map| {
                    map.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(name))
                        .and_then(|(_, v)| v.as_str())
                        .map(str::to_string)
                })
            };
            info.title = tag("title");
            info.author = tag("artist");
        }
    }

    if let Some(streams) = json.get("streams").and_then(|v| v.as_array()) {
        for stream in streams {
            let codec_type = stream.get("codec_type").and_then(|v| v.as_str()).unwrap_or("");
            let codec = stream
                .get("codec_name")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            match codec_type {
                "video" if info.video_codec.is_none() => {
                    info.video_codec = codec;
                    let width = stream.get("width").and_then(|v| v.as_u64());
                    let height = stream.get("height").and_then(|v| v.as_u64());
                    if let (Some(w), Some(h)) = (width, height) {
                        if w > 0 && h > 0 {
                            info.resolution = Some(format!("{w}×{h}"));
                        }
                    }
                }
                "audio" if info.audio_codec.is_none() => {
                    info.audio_codec = codec;
                    if let Some(rate) = stream.get("sample_rate").and_then(|v| v.as_str()) {
                        info.sample_rate = parse_u64(rate)
                            .map(|hz| format!("{:.1} kHz", hz as f64 / 1000.0));
                    }
                    if let Some(channels) = stream.get("channels").and_then(|v| v.as_u64()) {
                        info.channels = Some(match channels {
                            1 => "mono".to_string(),
                            2 => "stereo".to_string(),
                            n => format!("{n} ch"),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    Some(info)
}

fn parse_u64(raw: &str) -> Option<u64> {
    raw.trim().parse::<f64>().ok().map(|v| v as u64)
}

fn format_bitrate(bits_per_sec: u64) -> String {
    let kbps = bits_per_sec as f64 / 1000.0;
    if kbps >= 1000.0 {
        format!("{:.1} Mbps", kbps / 1000.0)
    } else {
        format!("{kbps:.0} kbps")
    }
}

// ---------------------------------------------------------------------------
// artwork
// ---------------------------------------------------------------------------

/// Download a remote thumbnail into the cache directory.
fn download_artwork(url: &str, salt: &str) -> Option<PathBuf> {
    let path = cache_path(salt, "img")?;

    // Accept webp/png/jpg: the extension only needs to be one `image` can
    // sniff; GPUI's loader sniffs the format from the bytes.
    let status = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "15",
            "-o",
            path.to_str()?,
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;

    if status.success() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        Some(path)
    } else {
        let _ = std::fs::remove_file(&path);
        None
    }
}

/// Pull an embedded cover, or a representative frame, out of a local file.
///
/// `-ss 5` before `-i` is a fast (keyframe) seek; for audio files with an
/// attached picture the seek is ignored and the picture is returned.
fn extract_poster(local_path: &Path, _playable_url: &str) -> Option<PathBuf> {
    if !source::binary_exists("ffmpeg") {
        return None;
    }

    let path = cache_path(&local_path.to_string_lossy(), "cover")?;
    if path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Some(path); // already extracted
    }

    let status = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-loglevel",
            "error",
            "-ss",
            "5",
            "-i",
            &local_path.to_string_lossy(),
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            "scale='min(640,iw)':-2",
            &path.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;

    if status.success() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        Some(path)
    } else {
        let _ = std::fs::remove_file(&path);
        None
    }
}

/// Stable, collision-resistant cache path for a source.
fn cache_path(salt: &str, prefix: &str) -> Option<PathBuf> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).ok()?;

    let mut hasher = DefaultHasher::new();
    salt.hash(&mut hasher);
    let name = format!("{prefix}-{:016x}.img", hasher.finish());
    Some(dir.join(name))
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("stream-player"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_upload_date() {
        assert_eq!(format_upload_date("20141110"), Some("2014-11-10".into()));
        assert_eq!(format_upload_date("unknown"), Some("unknown".into()));
    }

    #[test]
    fn formats_bitrates() {
        assert_eq!(format_bitrate(128_000), "128 kbps");
        assert_eq!(format_bitrate(5_200_000), "5.2 Mbps");
    }

    #[test]
    fn collapses_description_whitespace() {
        let out = truncate_description("line one\n\nline\ttwo".into());
        assert_eq!(out, "line one line two");
    }

    #[test]
    fn truncates_long_description() {
        let long = "x".repeat(500);
        let out = truncate_description(long);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 421);
    }
}
