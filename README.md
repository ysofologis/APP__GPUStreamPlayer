# stream-player

A GPUI desktop stream player for Linux. Paste a URL, press Open.

Plays:

| Source | Example | Resolved by |
|---|---|---|
| YouTube / Vimeo / SoundCloud / Twitch / ~10 more | `https://youtu.be/...` | `yt-dlp` |
| Direct audio (MP3, M4A, OGG, FLAC, Opus) | `https://host/track.mp3` | GStreamer |
| Icecast / Shoutcast / radio | `https://host:8000/stream` | GStreamer |
| HLS | `https://host/live/index.m3u8` | GStreamer (`hlsdemux`) |
| DASH | `https://host/manifest.mpd` | GStreamer (`dashdemux`) |
| Local files | `/path/movie.mkv` | GStreamer |

## YouTube bot check

YouTube rate-limits datacenter IPs and answers with *"Sign in to confirm you're
not a bot"*. When that happens, the player says so explicitly instead of dumping
yt-dlp's raw stderr. The fix is to give yt-dlp cookies from a browser where you
are signed in to YouTube:

```sh
# Export cookies in Netscape format, then:
mkdir -p ~/.config/stream-player
cp /path/to/cookies.txt ~/.config/stream-player/cookies.txt

# or point at a browser profile directly:
export STREAM_PLAYER_COOKIES_FROM_BROWSER=chromium

# or an explicit file:
export STREAM_PLAYER_COOKIES=/path/to/cookies.txt
```

Resolution order: `STREAM_PLAYER_COOKIES` → `STREAM_PLAYER_COOKIES_FROM_BROWSER` →
`~/.config/stream-player/cookies.txt`.

## Requirements

**Build:** Rust ≥ 1.85, `pkg-config`, GStreamer 1.14+ development headers
(`gstreamer`, `gstreamer-app`, `gstreamer-video`), Vulkan or GL drivers.

```sh
# Arch / Manjaro
sudo pacman -S --needed rust pkgconf gstreamer gst-plugins-{base,good,bad,ugly} gst-libav

# Debian / Ubuntu
sudo apt install build-essential pkg-config libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-{base,good,bad,ugly} gstreamer1.0-libav
```

**Runtime:** `yt-dlp` (YouTube and other extractor sites), `ffprobe`
(audio-only detection). Both are optional — the app degrades gracefully and
tells you what is missing.

```sh
sudo pacman -S yt-dlp ffmpeg     # or: pipx install yt-dlp
```

## Build and run

```sh
cargo build --release
./target/release/stream-player                      # then paste a URL
./target/release/stream-player "https://youtu.be/X" # or start immediately
```

## Controls

| Input | Action |
|---|---|
| `Space` | Play / pause |
| `←` / `→` | Seek ∓ 10 s |
| `↑` / `↓` | Volume ± 5 % |
| `M` | Mute |
| `F` | Fullscreen |
| Click the URL bar | Focus the field (paste with `Ctrl+V`) |
| `Enter` in the URL field | Open / play |
| Click the timeline | Seek to that position |

Buttons: `« 60`, `‹ 10`, play/pause, `10 ›`, `60 »`, speed `−`/`+` (click the
value to reset to 1×), mute, volume `−`/`+`, fullscreen.

The player shortcuts are suppressed while the URL field has focus, so typing
`left` or `m` in the field does not seek or mute.

### Info panel

The right-hand panel can be **dragged wider or narrower** by the splitter
between the video and the panel. Clicking any value in the panel copies it to
the clipboard (the source URL, channel, views, likes, format, codecs, ...),
and a badge confirms what was copied.

## Architecture

```
src/
├── main.rs      entry point, dependency checks, CLI arg
├── text_input.rs  editable single-line URL field (GPUI has no text widget)
├── source.rs    URL classification + resolution (yt-dlp, ffprobe), file:// URLs
├── metadata.rs  track info for the side panel (yt-dlp -J / ffprobe / artwork)
├── player.rs    playback engine: video backend + audio backend, transport state
└── ui.rs        GPUI view: layout, transport bar, keyboard, seek bar, info panel
```

### The info panel

The right-hand panel shows what can be learned about the current source:

| Field | Source |
|---|---|
| Cover art | `yt-dlp` thumbnail (downloaded + cached), or an embedded cover / poster frame extracted with `ffmpeg` for local files |
| Title, channel/artist, description | `yt-dlp -J`, or container tags for local files |
| Format, resolution, codecs, bitrate, sample rate, channels | `ffprobe` |
| Upload date, views, likes, categories, tags | `yt-dlp -J` |
| Provenance (`yt-dlp` / `ffprobe`) | which tool supplied the facts |

Metadata is fetched **after** playback starts, on a background thread, so a slow
extractor never delays the first frame. Artwork is cached under
`$XDG_CACHE_HOME/stream-player/`, so it survives CDN URL expiry and needs no
network on replay.

Fields that a given source cannot provide are omitted rather than shown as
`N/A`, and the panel scrolls independently of the video.

### Why two playback backends

`gpui-video-player` builds its pipeline as:

```
playbin video-sink="videoscale ! videoconvert ! appsink caps=video/x-raw,format=NV12 ..."
```

and then reads the appsink pad's caps for width/height/framerate, returning
`Error::Caps` when they are absent. **An MP3, an Icecast stream, or any
audio-only source has no video pad, so `Video::new` always fails for them.**

`player.rs` therefore has two paths behind one `Engine` enum:

- `Engine::Video` — `gpui-video-player` for anything with a video track.
- `Engine::Audio` — an owned `playbin` with `autoaudiosink`, exposing the same
  transport surface (position, duration, seek, volume, speed).

`source.rs` decides which one to use via `ffprobe` (`has_video`). If the probe
is wrong or missing, `Engine::open` falls back to the other backend rather than
failing.

## Things worth knowing

**Fonts can dominate startup time.** GPUI's text stack calls
`fontdb::Database::load_system_fonts()` at startup, which enumerates every font
fontconfig knows about. A machine with a very large font collection (this repo
was developed on one with 14 GB / 9,231 files under `~/.local/share/fonts`)
spends minutes in page faults before the first frame. If startup is slow:

```sh
# Scope fontconfig to a small set of directories
cat > ~/.config/fontconfig/fonts.conf <<'EOF'
<?xml version="1.0"?><!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <dir>/usr/share/fonts</dir>
  <cachedir>/var/cache/fontconfig</cachedir>
</fontconfig>
EOF
```

Or launch with `FONTCONFIG_FILE=/path/to/minimal.conf`.

**Keep the binary on local disk.** `target/release/stream-player` is ~26 MB, but
a debug build is ~500 MB; launching it from a slow network/HDD mount adds
noticeable startup latency from demand paging.

**Livestreams disable seeking** on purpose — `AudioSession` refuses to seek when
the pipeline reports no duration, because a flush-seek on a live source stalls
the stream.

**Panel layout gotcha (GPUI).** In a flex column, a child that overflows gets
`flex_shrink: 1` by default and collapses to zero height *before* a sibling
scroll container can scroll. Fixed-height blocks in the panel (artwork, 1px
dividers) therefore need `flex_none()`; without it the artwork silently
disappears while the rest of the panel renders normally.

**Known limitations**

- YouTube is resolved to a single **progressive (muxed)** file via
  `yt-dlp -f 'b[protocol^=http]'`. This keeps one URL → one pipeline, but caps
  quality at what YouTube serves muxed (usually 360–720p). DASH audio+video
  remuxing would need `yt-dlp` to download/merge and hand us a local file.
- Subtitles are parsed by the underlying crate but not rendered by this app
  (`gpui-video-player` keeps subtitle text private in 0.1.3).
- No playlist handling; one source at a time.

## Dependency workaround

`vendor/xattr` is a copy of `xattr` 0.2.3 with one line changed. It is pulled in
transitively via `gpui → gpui_http_client → zed-async-tar`, and 0.2.3 does not
compile against libc ≥ 0.2.182 (`ENOATTR` was removed from the Linux bindings).
We can't downgrade libc (rustix 1.1.6 requires ≥ 0.2.182) or bump xattr
(`zed-async-tar` pins `0.2`). The patch defines `ENOATTR` as `ENODATA` on Linux,
which is what it always aliased. See `Cargo.toml [patch.crates-io]`.

`.cargo/config.toml` pins `PKG_CONFIG=/usr/bin/pkg-config` with the system `.pc`
paths, because a Homebrew `pkg-config` earlier in `PATH` will not find a
distro-packaged GStreamer.

## Tests

```sh
cargo test
```

Six unit tests cover URL classification: YouTube routing, direct media,
manifests, query-string host confusion, garbage rejection, and `file://`
percent-encoding.