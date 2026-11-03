//! Default video quality selection (720p where the source offers it).
//!
//! ## The constraint
//!
//! YouTube publishes a *muxed* (audio+video in one file) progressive stream
//! only at 360p -- format 18. Every 720p-and-up format in the default format
//! table is `video only` (DASH: separate video and audio tracks), and
//! `gpui-video-player` feeds a single URL to a GStreamer `playbin`, so a
//! video-only URL would play silently with no sound.
//!
//! The one shape that carries **both** tracks above 360p is YouTube's HLS
//! (`.m3u8`) stream: an HLS variant playlist is a single muxed stream.
//!
//! ## The client problem
//!
//! Which formats YouTube offers depends on the *player client* yt-dlp
//! impersonates, and this is not a detail you can skip:
//!
//! | session | m3u8 formats | highest muxed |
//! |---|---|---|
//! | default client, no cookies | 17 | 1080p |
//! | default client, **with cookies** | **0** | 360p (DASH only above that) |
//! | `web_embedded`, with cookies | 7 | 1080p |
//!
//! An authenticated session with the default client offers *no* HLS at all,
//! so a naive `b[height=720][protocol^=m3u8]` selector fails with
//! "Requested format is not available" -- even though 720p exists.
//!
//! The fix is to impersonate a client that offers HLS: `web_embedded` (and
//! `web_safari`) expose the HLS ladder even for signed-in requests. The app
//! therefore passes `--extractor-args youtube:player_client=web_embedded` on
//! the format probe.
//!
//! ## Why no manifest rewriting
//!
//! An earlier approach rewrote the HLS *master* to contain only the 720p
//! variant, because GStreamer's `hlsdemux` picks the variant itself and
//! offers no way to say "720p" (left alone it selected the lowest variant,
//! sometimes 240p). That is unnecessary here: yt-dlp's `b[height=720]`
//! selector already returns the **variant** playlist for the requested
//! height (`itag=300`, `RESOLUTION=1280x720`), not the master, so GStreamer
//! has exactly one variant to choose and decodes 1280x720.

use std::process::{Command, Stdio};

/// The default target height in pixels.
pub const DEFAULT_HEIGHT: u32 = 720;

/// The YouTube player client to impersonate when probing for a high-quality
/// muxed stream.
///
/// `web_embedded` is the only widely available client that still exposes the
/// HLS ladder when the request is authenticated; the default client drops to
/// DASH-only (video without audio) above 360p once cookies are present.
const PLAYER_CLIENT: &str = "web_embedded";

/// Extra yt-dlp arguments that make the high-quality HLS ladder reachable.
pub fn player_client_args() -> Vec<String> {
    vec![
        "--extractor-args".to_string(),
        format!("youtube:player_client={PLAYER_CLIENT}"),
    ]
}

/// A yt-dlp format selector for the highest muxed stream at exactly `height`.
///
/// `b` (best) matters: with `bv` (best video) yt-dlp is free to return a
/// video-only DASH format, which would play without sound.
pub fn height_selector(height: u32) -> String {
    format!("b[height={height}][protocol^=m3u8]")
}

/// Ask yt-dlp for a stream URL at `height`, preferring the HLS ladder.
///
/// Returns `None` when the source has no muxed stream at that height (a
/// 360p-only video, a live stream, an audio-only page).
///
/// Blocking: shells out to `yt-dlp`. Call from a background task.
pub fn find_stream_url(
    page: &str,
    height: u32,
    cookie_args: &[String],
    client_args: &[String],
) -> Option<String> {
    let url = run_yt_dlp(page, &height_selector(height), cookie_args, client_args)?;
    Some(url)
}

/// Run yt-dlp with `-g` and return the first URL it prints.
fn run_yt_dlp(
    page: &str,
    selector: &str,
    cookie_args: &[String],
    client_args: &[String],
) -> Option<String> {
    let mut command = Command::new("yt-dlp");
    command.args([
        "--no-playlist",
        "--no-warnings",
        "--no-progress",
        "-f",
        selector,
        "-g",
    ]);
    command.args(client_args);
    command.args(cookie_args);
    command.arg(page);

    let output = command
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("http"))
        .map(|line| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_selector_targets_exact_height_and_hls() {
        let selector = height_selector(720);
        assert!(selector.contains("height=720"));
        assert!(selector.contains("protocol^=m3u8"));
        // `b` (best), not `bv` (best video): `bv` may return a video-only
        // DASH format, which plays without sound.
        assert!(selector.starts_with("b["));
        assert!(!selector.starts_with("bv"));
    }

    #[test]
    fn player_client_is_the_one_that_keeps_hls_when_authenticated() {
        let args = player_client_args();
        assert_eq!(args[0], "--extractor-args");
        assert!(
            args[1].contains("web_embedded"),
            "must impersonate a client that exposes HLS with cookies: {args:?}"
        );
    }

}
