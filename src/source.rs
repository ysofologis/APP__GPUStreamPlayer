//! URL classification and resolution.
//!
//! The player must handle three very different kinds of input:
//!
//! 1. **Direct media** (a local file or an `http(s)` URL that GStreamer can
//!    decode directly: mp4, mp3, HLS `.m3u8`, DASH `.mpd`, Icecast, ...).
//! 2. **YouTube** (and other yt-dlp extractors), which need a resolver to turn a
//!    page URL into a real media URL.
//! 3. **Anything else** -- rejected with a clear message instead of a cryptic
//!    GStreamer failure.
//!
//! Resolution runs on a blocking thread because both `yt-dlp` and `ffprobe` are
//! subprocesses that can take seconds.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where a playable stream comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// A local file on disk.
    LocalFile(PathBuf),
    /// A remote URL that GStreamer can handle on its own.
    DirectUrl(String),
    /// A site page that needs yt-dlp to extract a real stream URL.
    Resolved { page: String, stream: String },
}

impl SourceKind {
    /// The URL handed to GStreamer.
    pub fn playable_url(&self) -> String {
        match self {
            SourceKind::LocalFile(path) => path_to_file_url(path),
            SourceKind::DirectUrl(url) => url.clone(),
            SourceKind::Resolved { stream, .. } => stream.clone(),
        }
    }

    /// Human-readable label for the UI.
    pub fn label(&self) -> String {
        match self {
            SourceKind::LocalFile(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            SourceKind::DirectUrl(url) => url.clone(),
            SourceKind::Resolved { page, .. } => page.clone(),
        }
    }
}

/// What we learned about a stream before handing it to the player.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// True when the stream carries a video track. Audio-only streams take a
    /// different GStreamer path in `gpui-video-player` (see `player.rs`).
    pub has_video: bool,
    /// Duration in seconds, when the container declares one (live streams do not).
    pub duration_secs: Option<f64>,
    /// Container/format description for the UI (`mp4`, `mp3`, `hls`, ...).
    pub format: String,
}

/// Everything the UI needs to start playback.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub kind: SourceKind,
    pub probe: Probe,
}

#[derive(Debug)]
pub enum ResolveError {
    Unsupported(String),
    MissingBinary(&'static str),
    Tool {
        tool: &'static str,
        message: String,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Unsupported(why) => write!(f, "unsupported input: {why}"),
            ResolveError::MissingBinary(bin) => {
                write!(f, "`{bin}` not found in PATH -- install it to play this source")
            }
            ResolveError::Tool { tool, message } => write!(f, "{tool} failed: {message}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Classify raw user input and resolve it into a playable source.
///
/// Blocking: shells out to `yt-dlp`/`ffprobe`. Call from a background task.
pub fn resolve(input: &str) -> Result<Resolved, ResolveError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ResolveError::Unsupported("empty input".into()));
    }

    let kind = classify(trimmed)?;

    // yt-dlp page URLs: ask the extractor for a stream we can actually play.
    let kind = match kind {
        SourceKind::Resolved { page, .. } => SourceKind::Resolved {
            stream: yt_dlp_stream(&page)?,
            page,
        },
        other => other,
    };

    let probe = probe_url(&kind.playable_url()).unwrap_or_default();

    Ok(Resolved { kind, probe })
}

fn classify(input: &str) -> Result<SourceKind, ResolveError> {
    // Local path (absolute, relative, or ~-prefixed).
    if let Some(path) = local_path(input) {
        return Ok(SourceKind::LocalFile(path));
    }

    let lower = input.to_ascii_lowercase();
    let has_scheme = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("rtsp://")
        || lower.starts_with("rtmp://")
        || lower.starts_with("icy://")
        || lower.starts_with("file://");

    if !has_scheme {
        return Err(ResolveError::Unsupported(format!(
            "`{input}` is neither an existing file nor a supported URL"
        )));
    }

    if is_extractor_site(&lower) {
        return Ok(SourceKind::Resolved {
            page: input.to_string(),
            stream: String::new(),
        });
    }

    // Direct media: extension is a strong signal, but a missing extension is
    // fine -- plenty of radio endpoints are extension-less.
    if looks_like_direct_media(&lower) {
        return Ok(SourceKind::DirectUrl(input.to_string()));
    }

    // Unknown http(s) URL: let yt-dlp try. It fails fast and clearly if the
    // site is not supported, which is a better error than GStreamer's.
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(SourceKind::Resolved {
            page: input.to_string(),
            stream: String::new(),
        });
    }

    Ok(SourceKind::DirectUrl(input.to_string()))
}

/// Resolve a local path, expanding a leading `~`.
fn local_path(input: &str) -> Option<PathBuf> {
    let candidate = if let Some(rest) = input.strip_prefix("~/") {
        let home = std::env::var_os("HOME")?;
        Path::new(&home).join(rest)
    } else {
        PathBuf::from(input)
    };

    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

const EXTRACTOR_SITES: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "music.youtube.com",
    "vimeo.com",
    "soundcloud.com",
    "dailymotion.com",
    "twitch.tv",
    "bandcamp.com",
    "bitchute.com",
    "rumble.com",
    "odysee.com",
    "nicovideo.jp",
    "bilibili.com",
];

fn is_extractor_site(url: &str) -> bool {
    // Match on host, not on the whole string, so `...?u=youtube.com` does not
    // trigger yt-dlp.
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("www.");

    EXTRACTOR_SITES
        .iter()
        .any(|site| host == *site || host.ends_with(&format!(".{site}")))
}

/// Extensions and URL shapes GStreamer decodes without a resolver.
fn looks_like_direct_media(url: &str) -> bool {
    const MEDIA_EXT: &[&str] = &[
        ".mp4", ".m4v", ".mkv", ".webm", ".mov", ".avi", ".ts", ".m2ts", ".flv", ".ogv", ".3gp",
        // audio
        ".mp3", ".m4a", ".aac", ".flac", ".ogg", ".oga", ".opus", ".wav", ".wma", ".aiff", ".alac",
        // manifests
        ".m3u8", ".mpd", ".ism", ".m3u",
    ];

    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if MEDIA_EXT.iter().any(|ext| path.ends_with(ext)) {
        return true;
    }

    // Radio/stream endpoints that advertise themselves via the path.
    path.contains("/stream")
        || path.contains("/live")
        || path.contains("/radio")
        || path.contains("/icecast")
        || path.ends_with("/;")
}

/// Ask yt-dlp for a single file that already contains both video and audio.
///
/// Preference order matters:
/// 1. `b[protocol^=http]` -- a progressive (muxed) HTTP file. One URL, one
///    pipeline, no merging. This is the only shape `gpui-video-player` can feed
///    to `playbin` unchanged.
/// 2. `b[protocol^=m3u8]` -- a muxed HLS variant, when YouTube offers one.
/// 3. fall back to yt-dlp's default `b`, then a separate audio-only pass for
///    audio-only pages.
fn yt_dlp_stream(page: &str) -> Result<String, ResolveError> {
    if !binary_exists("yt-dlp") {
        return Err(ResolveError::MissingBinary("yt-dlp"));
    }

    // Progressive/muxed first; `-g` prints URLs only.
    for selector in ["b[protocol^=http]", "b[protocol^=m3u8]", "b"] {
        if let Ok(url) = run_yt_dlp(page, selector) {
            return Ok(url);
        }
    }

    // Audio-only fallback (radio shows, music pages).
    for selector in ["ba[protocol^=http]", "ba"] {
        if let Ok(url) = run_yt_dlp(page, selector) {
            return Ok(url);
        }
    }

    let message = run_yt_dlp_stderr(page);
    Err(ResolveError::Tool {
        tool: "yt-dlp",
        message: if message.is_empty() {
            "no playable format found for this URL".to_string()
        } else {
            message
        },
    })
}

/// Extra arguments for every `yt-dlp` invocation.
///
/// YouTube serves a "Sign in to confirm you're not a bot" page to
/// rate-limited / datacenter IPs. Cookies are the documented way around it:
/// export them from a signed-in browser (Netscape format) and point the app
/// at the file.
///
/// Resolution order:
///   1. `STREAM_PLAYER_COOKIES` -- explicit path to a Netscape cookies file.
///   2. `STREAM_PLAYER_COOKIES_FROM_BROWSER` -- browser name, e.g. `chromium`.
///   3. `~/.config/stream-player/cookies.txt` -- conventional location.
pub fn yt_dlp_cookie_args() -> Vec<String> {
    let mut args = Vec::new();

    if let Some(path) = std::env::var_os("STREAM_PLAYER_COOKIES") {
        let path = PathBuf::from(path);
        if path.is_file() {
            args.push("--cookies".into());
            args.push(path.to_string_lossy().into_owned());
            return args;
        }
    }

    if let Some(browser) = std::env::var_os("STREAM_PLAYER_COOKIES_FROM_BROWSER") {
        args.push("--cookies-from-browser".into());
        args.push(browser.to_string_lossy().into_owned());
        return args;
    }

    if let Some(dir) = config_dir() {
        let path = dir.join("cookies.txt");
        if path.is_file() {
            args.push("--cookies".into());
            args.push(path.to_string_lossy().into_owned());
        }
    }

    args
}

/// `~/.config/stream-player`, or `$XDG_CONFIG_HOME/stream-player`.
fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("stream-player"))
}

fn run_yt_dlp(page: &str, selector: &str) -> Result<String, ResolveError> {
    let mut command = Command::new("yt-dlp");
    command.args([
        "--no-playlist",
        "--no-warnings",
        "--no-progress",
        "-f",
        selector,
        "-g",
    ]);
    command.args(yt_dlp_cookie_args());
    command.arg(page);

    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|_| ResolveError::MissingBinary("yt-dlp"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ResolveError::Tool {
            tool: "yt-dlp",
            message: explain_yt_dlp_error(&stderr),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("http"))
        .map(|line| line.to_string())
        .ok_or_else(|| ResolveError::Tool {
            tool: "yt-dlp",
            message: "extractor returned no URL".into(),
        })
}

/// Turn yt-dlp's raw stderr into something actionable.
///
/// The bot check is by far the most common failure for a self-hosted player:
/// YouTube rate-limits datacenter IPs, and the raw message points at the
/// wiki without saying what to do here.
fn explain_yt_dlp_error(stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("sign in to confirm") || lower.contains("not a bot") {
        return "YouTube is asking this IP to prove it is not a bot (rate limit).\n\n\
            Fix: export cookies from a browser where you are signed in to YouTube and\n\
            save them to ~/.config/stream-player/cookies.txt (Netscape format),\n\
            or set STREAM_PLAYER_COOKIES_FROM_BROWSER=chromium.\n\n\
            See: https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp"
            .to_string();
    }
    if lower.contains("private video") {
        return "This video is private.".to_string();
    }
    if lower.contains("video unavailable") || lower.contains("removed") {
        return "This video is unavailable or has been removed.".to_string();
    }
    if lower.contains("unsupported url") {
        return "yt-dlp does not support this URL.".to_string();
    }
    if stderr.is_empty() {
        return "no playable format found for this URL".to_string();
    }
    stderr.to_string()
}

fn run_yt_dlp_stderr(page: &str) -> String {
    let mut command = Command::new("yt-dlp");
    command.args(["--no-playlist", "--no-warnings", "-f", "b", "--simulate"]);
    command.args(yt_dlp_cookie_args());
    command.arg(page);
    command
        .stdin(Stdio::null())
        .output()
        .map(|out| {
            let stderr = String::from_utf8_lossy(&out.stderr);
            stderr
                .lines()
                .filter(|line| line.starts_with("ERROR"))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default()
}

/// Inspect a stream with ffprobe.
///
/// This is what lets the player support audio-only URLs at all:
/// `gpui-video-player`'s `Video::new` hard-requires a video pad with NV12 caps,
/// so an MP3 or Icecast stream must be played through a pipeline that we build
/// ourselves (see `player.rs`).
pub fn probe_url(url: &str) -> Option<Probe> {
    if !binary_exists("ffprobe") {
        return None;
    }

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type:format=duration,format_name",
            "-of",
            "default=noprint_wrappers=1",
            url,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut probe = Probe::default();

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "codec_type" => {
                if value.trim() == "video" {
                    probe.has_video = true;
                }
            }
            "duration" => {
                probe.duration_secs = value.trim().parse::<f64>().ok();
            }
            "format_name" => {
                probe.format = value.trim().to_string();
            }
            _ => {}
        }
    }

    // MP3/ICE streams report no duration; that is fine, the seek bar is simply
    // disabled until the pipeline reports one.
    Some(probe)
}

pub fn binary_exists(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
}

/// Convert a filesystem path into a `file://` URL GStreamer accepts.
fn path_to_file_url(path: &Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut url = String::from("file://");
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::RootDir => {}
            Component::Normal(part) => {
                url.push('/');
                url.push_str(&percent_encode(&part.to_string_lossy()));
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {}
        }
    }
    url
}

/// Minimal percent-encoder for the characters that break `playbin` URIs.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_youtube_as_extractor() {
        let kind = classify("https://www.youtube.com/watch?v=abc").unwrap();
        assert!(matches!(kind, SourceKind::Resolved { .. }));
    }

    #[test]
    fn classifies_direct_mp3() {
        let kind = classify("https://example.com/track.mp3").unwrap();
        assert!(matches!(kind, SourceKind::DirectUrl(_)));
    }

    #[test]
    fn classifies_hls_manifest() {
        let kind = classify("https://example.com/live/index.m3u8").unwrap();
        assert!(matches!(kind, SourceKind::DirectUrl(_)));
    }

    #[test]
    fn host_matching_ignores_query_string() {
        // A plain media URL that merely mentions youtube in a query param must
        // not be routed through yt-dlp.
        let kind = classify("https://cdn.example.com/a.mp4?ref=youtube.com").unwrap();
        assert!(matches!(kind, SourceKind::DirectUrl(_)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(classify("not a url and not a file").is_err());
    }

    #[test]
    fn explains_bot_check_actionably() {
        let msg = explain_yt_dlp_error("ERROR: [youtube] abc: Sign in to confirm you're not a bot.");
        assert!(msg.contains("cookies"), "should mention cookies: {msg}");
        assert!(msg.contains("STREAM_PLAYER_COOKIES_FROM_BROWSER"), "should name the env var: {msg}");
    }

    #[test]
    fn explains_other_errors_plainly() {
        assert_eq!(explain_yt_dlp_error("ERROR: [youtube] x: Private video."), "This video is private.");
        assert_eq!(explain_yt_dlp_error("ERROR: Unsupported URL"), "yt-dlp does not support this URL.");
        assert_eq!(explain_yt_dlp_error(""), "no playable format found for this URL");
        assert_eq!(explain_yt_dlp_error("some other problem"), "some other problem");
    }

    #[test]
    fn file_url_encodes_spaces() {
        let url = path_to_file_url(Path::new("/tmp/a b.mp4"));
        assert!(url.contains("%20"), "got {url}");
        assert!(!url.contains(' '));
    }
}