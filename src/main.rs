//! stream-player — a GPUI stream player for Linux.
//!
//! Supports YouTube (via `yt-dlp`), direct audio streams (MP3/Icecast/OGG),
//! HLS/DASH manifests, and local media files.

mod player;
mod source;
mod ui;

use gpui::{App, Application};

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("stream_player=info,warn"),
    )
    .init();

    if !source::binary_exists("yt-dlp") {
        log::warn!("`yt-dlp` not found: YouTube and other site URLs will not resolve");
    }
    if !source::binary_exists("ffprobe") {
        log::warn!("`ffprobe` not found: audio-only detection is disabled");
    }

    // `stream-player [URL]` -- the URL is optional; without it the toolbar
    // accepts a paste.
    let initial_url = std::env::args().nth(1).filter(|arg| !arg.starts_with('-'));

    Application::new().run(move |cx: &mut App| {
        ui::launch(cx, initial_url);
    });
}
