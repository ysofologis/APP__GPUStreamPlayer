# stream-player

A native desktop stream player for Linux, built with [GPUI](https://github.com/zed-industries/zed)
(Zed's GPU-accelerated UI framework) and [GStreamer](https://gstreamer.freedesktop.org/).

Paste a URL or pick a file, and it plays. It handles the three input shapes
that a "stream player" actually has to deal with — web video sites, direct
audio streams, and local media — behind one transport UI.

```
┌──────────────────────────────────────────────────────────────────────┐
│ https://www.youtube.com/watch?v=…          [ Open ]  [ Play ]       │
│                                                                      │
│ ┌────────────────────────────┐  ┌────────────────────────────────┐ │
│ │                            │  │ [cover art]                    │ │
│ │        video               │  │ Title                          │ │
│ │                            │  │ Channel                        │ │
│ │                            │  │ STREAM  FORMAT / RESOLUTION /  │ │
│ │                            │  │         CODECS / BITRATE /    │ │
│ │                            │  │         AUDIO                 │ │
│ │                            │  │ ABOUT   UPLOADED / VIEWS /     │ │
│ │                            │  │         LIKES / SOURCE         │ │
│ └────────────────────────────┘  │ TAGS / DESCRIPTION             │ │
│ ━━━━━━━━━━━━━━●────────────────│ └────────────────────────────────┘ │
│ 0:42  «60 ‹10 ▶ 10› 60»   0:42/10:34   1.00× 🔊 80% vol− vol+ ⛶    │
└──────────────────────────────────────────────────────────────────────┘
```

---

## Table of contents

- [Scope](#scope)
- [How it works](#how-it-works)
- [Video quality](#video-quality)
- [Dependencies](#dependencies)
- [Build and run](#build-and-run)
- [Usage](#usage)
- [YouTube and the bot check](#youtube-and-the-bot-check)
- [Contributing](#contributing)
- [Known limitations](#known-limitations)
- [Troubleshooting](#troubleshooting)
- [Repository layout](#repository-layout)
- [Development](#development)

---

## Scope

### What it plays

| Source | Example | Resolved by |
|---|---|---|
| YouTube, Vimeo, SoundCloud, Twitch, Bandcamp, Dailymotion, … | `https://youtu.be/…` | `yt-dlp` |
| Direct audio — MP3, M4A, AAC, FLAC, OGG, Opus, WAV | `https://host/track.mp3` | GStreamer |
| Icecast / Shoutcast / radio endpoints | `https://host:8000/stream` | GStreamer |
| HLS | `https://host/live/index.m3u8` | GStreamer (`hlsdemux`) |
| DASH | `https://host/manifest.mpd` | GStreamer (`dashdemux`) |
| Local files — video or audio | `/path/movie.mkv` | GStreamer |

### What it does

- Editable URL field with real text editing (caret, selection, IME, clipboard).
- `Open` — platform file dialog, plays the chosen file immediately.
- `Play` — resolves the URL and plays it. `Enter` in the field does the same.
- Full transport: play/pause, ±10 s / ±60 s seek, click-to-seek timeline,
  volume, mute, playback speed (0.25×–4×), fullscreen.
- Keyboard shortcuts for all of the above.
- A right-hand **info panel** with cover art, title/author, stream facts
  (format, resolution, codecs, bitrate, sample rate, channels), provenance
  data (upload date, views, likes), tags and description.
- Click any value in the panel to copy it.
- Drag the splitter to resize the panel.
- One source at a time; the pipeline is torn down when you load another.

### What it deliberately does not do

- **No playlist / queue.** One source at a time.
- **No DASH audio+video merging for YouTube.** See
  [Known limitations](#known-limitations).
- **No subtitle rendering.** The underlying crate parses subtitle *text* but
  keeps it private (`pub(crate)`), so there is nothing to display yet.
- **No library, favourites, or persistent state.** Nothing is written to disk
  except the artwork cache and (optionally) a cookies file.
- **No transcoding, recording, or PiP.**
- **No macOS/Windows support.** Linux-only; the rendering path differs per
  platform upstream and this has only been exercised on Linux.

---

## How it works

### 1. Classification and resolution — `src/source.rs`

Input is classified before anything touches GStreamer:

```
user input
   │
   ├─ existing local path ──────────────────────────► SourceKind::LocalFile
   ├─ host matches a known extractor site ──────────► SourceKind::Resolved
   │   (youtube.com, youtu.be, vimeo.com, soundcloud.com, twitch.tv, …)
   └─ everything else with a URL scheme ────────────► SourceKind::DirectUrl
```

Host matching is done on the **host**, not the whole string, so
`https://cdn.example.com/a.mp4?ref=youtube.com` is treated as direct media
rather than being sent to `yt-dlp`.

For extractor sites, `yt-dlp` is asked for a single muxed file:

```
yt-dlp -f 'b[protocol^=http]' -g <url>     # progressive HTTP, audio+video
yt-dlp -f 'b[protocol^=m3u8]' -g <url>     # muxed HLS variant
yt-dlp -f 'b'              -g <url>        # whatever yt-dlp picks
yt-dlp -f 'ba[protocol^=http]' -g <url>    # audio-only fallback
```

One URL → one pipeline is the design constraint; see
[Known limitations](#known-limitations) for what it costs.

The resolved URL is then probed with `ffprobe` to learn whether it has a video
track (`has_video`), its duration, and its container format.

### 2. Playback — `src/player.rs`

There are **two backends** behind one `Engine` enum, and the reason is a
hard limitation of the upstream crate.

`gpui-video-player` builds this pipeline internally:

```
playbin video-sink="videoscale ! videoconvert ! appsink caps=video/x-raw,format=NV12,..."
```

and then reads the appsink pad's *current* caps for `width` / `height` /
`framerate`, returning `Error::Caps` when they are absent:

```rust
// gpui-video-player 0.1.3, src/video.rs
let caps = cleanup!(pad.current_caps().ok_or(Error::Caps))?;
let width = cleanup!(s.get::<i32>("width").map_err(|_| Error::Caps))?;
```

**An MP3, an Icecast stream, or any audio-only source has no video pad, so
`Video::new` always fails for them.** There is no option to omit the video sink.

So:

| Backend | Used for | Implementation |
|---|---|---|
| `Engine::Video` | anything with a video track | `gpui-video-player` — GPU rendering via the sprite atlas for free |
| `Engine::Audio` | audio-only sources | an owned `playbin` + `autoaudiosink`, with the same transport surface |

Both expose the identical transport API (`play_pause`, `seek`, `position`,
`duration`, `set_volume`, `set_speed`, …), so `ui.rs` never branches on which
one is live.

`Engine::open` picks the backend from the ffprobe result, and **falls back to
the other one** if the probe was wrong or unavailable — so a mis-detected
stream degrades to working rather than to an error.

The audio backend also watches the pipeline bus on its own thread to surface
GStreamer errors in the log and to detect end-of-stream.

### 3. Metadata — `src/metadata.rs`

Runs **after playback has already started**, on a background thread, so a slow
extractor (`yt-dlp -J` can take several seconds) never delays the first frame.

| Facts | Source |
|---|---|
| Title, channel/artist, description, upload date, views, likes, categories, tags | `yt-dlp -J` |
| Container tags (title/artist/album) for local files | ffprobe format tags |
| Format, resolution, codecs, bitrate, sample rate, channels | `ffprobe` |
| Cover art | `yt-dlp` thumbnail (downloaded, cached) **or** an embedded cover / poster frame extracted with `ffmpeg` for local files |

Artwork is normalised to a local file under `$XDG_CACHE_HOME/stream-player/`,
which means it survives YouTube's CDN URLs expiring and needs no network on
replay.

Fields a source cannot provide are **omitted** rather than rendered as `N/A`.

### 4. The UI — `src/ui.rs` and `src/text_input.rs`

GPUI ships no text-input widget. `src/text_input.rs` is a single-line editable
field built the way GPUI's own `examples/input.rs` does it:

- `TextInput` owns the content, selection and IME marked range and implements
  `EntityInputHandler`, so the platform can drive editing (typing, paste,
  cut, IME composition).
- `TextElement` is a custom `Element` that shapes the text, paints the
  selection and caret, and registers the input handler via
  `Window::handle_input`.
- Editing key bindings are scoped to the `"TextInput"` key context, so they do
  not swallow the player's global shortcuts — and the player's own shortcuts
  are suppressed while the field has focus, so typing `left` or `m` in the
  field does not seek or mute.

---

## Video quality

The player asks for **720p** by default and falls back to the best muxed
stream when the source does not offer that height.

Getting above 360p on YouTube is not a matter of picking a format ID. YouTube
publishes a *muxed* (audio+video in one file) progressive stream only at 360p;
every higher format is video-only DASH, and because the player hands a single
URL to GStreamer's `playbin`, a video-only URL would play with no sound. The
muxed shapes above 360p are the HLS variants.

The catch is that the available formats depend on the player client yt-dlp
impersonates. With cookies, the default client offers **no HLS at all**, so a
plain height filter fails with `Requested format is not available` even when
720p exists. The player therefore probes with
`--extractor-args youtube:player_client=web_embedded`, which keeps the HLS
ladder available for signed-in requests.

| Source | Result |
|---|---|
| YouTube video with 720p | 1280x720 HLS, audio intact |
| YouTube video without 720p | best muxed progressive (usually 360p) |
| Direct MP3 / Icecast | unchanged (audio-only path) |
| Local file, HLS URL | unchanged (used as-is) |

## Dependencies

### Rust crates

Direct dependencies (7):

| Crate | Version | Why |
|---|---|---|
| `gpui` | 0.2.2 | Zed's GPU-accelerated UI framework — the whole UI layer |
| `gpui-video-player` | 0.1.3 | Video rendering widget (GPUI `Element` + GStreamer pipeline) |
| `gstreamer` | 0.25 | GStreamer bindings, for the audio backend |
| `serde_json` | 1 | Parsing `yt-dlp -J` output |
| `unicode-segmentation` | 1 | Grapheme-aware caret movement in the text field |
| `url` | 2 | URL parsing |
| `log` + `env_logger` | 0.4 / 0.11 | Logging |

Transitively this pulls in **~1,500 crates**, almost all of it `gpui` (the
Zed graphics stack, font stack, HTTP client, and platform bindings).

### System libraries

| Requirement | Used for | Arch / Manjaro | Debian / Ubuntu |
|---|---|---|---|
| GStreamer ≥ 1.14 + headers | decoding | `gstreamer gst-plugins-{base,good,bad,ugly} gst-libav` | `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-{base,good,bad,ugly} gstreamer1.0-libav` |
| `pkg-config` | locating GStreamer | `pkgconf` | `pkg-config` |
| Vulkan **or** GL drivers | rendering | `mesa` (or `vulkan-intel` / `vulkan-nouveau`) | `mesa-vulkan-drivers` / `libgl1-mesa-dri` |
| `libxkbcommon`, `wayland`, `libxcb`, `fontconfig`, `freetype2` | GPUI windowing + text | pulled in by `gstreamer`/GTK stacks | same |
| XDG desktop portal | the `Open` file dialog | `xdg-desktop-portal` | `xdg-desktop-portal-gtk` |

### External tools (runtime, optional)

| Tool | Used for | If missing |
|---|---|---|
| `yt-dlp` | YouTube and other extractor sites | those URLs cannot be resolved; everything else still works |
| `ffprobe` (from ffmpeg) | audio-only detection, stream facts | every stream is treated as video, so audio-only URLs fail; the `Engine::open` fallback catches this |
| `ffmpeg` | poster-frame extraction for local files | no cover art for local files |
| `curl` | thumbnail download | no cover art for extractor sites |

### Build-time workarounds

Two are committed to the repository because they are not optional:

**`vendor/xattr` (100 KB).** `xattr` 0.2.3 is pulled in transitively via
`gpui → gpui_http_client → zed-async-tar`, and does not compile against
libc ≥ 0.2.182, because libc removed `ENOATTR` from the Linux bindings.
Neither side of the conflict can move: `rustix` 1.1.6 (via `gpui →
wayland-backend`) requires libc ≥ 0.2.182, and `zed-async-tar` pins xattr to
`0.2`. The vendored copy changes one line — on Linux, `ENOATTR` is defined as
`ENODATA`, which is what it always aliased.

**`.cargo/config.toml`.** Pins `PKG_CONFIG=/usr/bin/pkg-config` and adds the
system `.pc` directories. A Homebrew `pkg-config` earlier in `PATH` will not
find a distro-packaged GStreamer, and `gstreamer-sys`'s build script then fails
with *"The system library gstreamer-1.0 was not found"*. Override these if you
build on a different layout.

---

## Build and run

```sh
cargo build --release

./target/release/stream-player                       # start empty
./target/release/stream-player "https://youtu.be/X"  # start playing immediately
./target/release/stream-player /path/to/file.mkv
```

A convenience launcher that also copies the binary to local disk (a release
build is ~29 MB, but a *debug* build is ~500 MB, and running that from a slow
network mount makes startup crawl because of demand paging):

```sh
./run.sh "https://youtu.be/X"
```

---

## Usage

### Header

| Control | Action |
|---|---|
| URL field | Type or paste a URL. `Enter` plays it. `Ctrl+V` pastes. |
| `Open` | Opens the platform file dialog and plays the chosen file. |
| `Play` | Resolves and plays whatever is in the field. |

### Transport

| Button | Action |
|---|---|
| `« 60` / `‹ 10` | Seek backwards 60 s / 10 s |
| `▶` / `❚❚` | Play / pause |
| `10 ›` / `60 »` | Seek forwards 10 s / 60 s |
| `−` / `+` | Playback speed down / up (0.25×–4×); click the value to reset to 1× |
| 🔊 | Mute / unmute |
| `vol −` / `vol +` | Volume ± 5 % |
| `⛶` | Fullscreen |

Click anywhere on the timeline to seek. The timeline is disabled for live
streams, which have no duration to seek within.

### Keyboard

| Key | Action |
|---|---|
| `Space` | Play / pause |
| `←` / `→` | Seek ∓ 10 s |
| `↑` / `↓` | Volume ± 5 % |
| `M` | Mute |
| `F` | Fullscreen |

These are suppressed while the URL field has focus.

### Info panel

The panel is resizable by dragging the splitter between the video and the
panel. Clicking any value copies it to the clipboard, and a badge confirms
what was copied.

---

## YouTube and the bot check

YouTube rate-limits datacenter IPs and answers with:

```
ERROR: [youtube] <id>: Sign in to confirm you're not a bot.
```

This is **not** a bug in the player — it is YouTube refusing the request. The
player detects it and shows an actionable message instead of dumping raw
yt-dlp stderr.

The fix is to give yt-dlp cookies from a browser session where you are signed
in to YouTube. The player looks for them in this order:

1. `STREAM_PLAYER_COOKIES` — path to a Netscape-format cookies file.
2. `STREAM_PLAYER_COOKIES_FROM_BROWSER` — browser name, e.g. `chromium`.
3. `~/.config/stream-player/cookies.txt` — the conventional location.

### Exporting cookies from a running Chrome

Chrome encrypts its cookie store with a key held in the OS keyring, which
yt-dlp often cannot read: the keyring may be locked, and the `secretstorage`
Python module may be missing. `yt-dlp --cookies-from-browser chrome` then
fails with *"cannot decrypt v11 cookies: no key found"*.

The bundled script sidesteps this entirely by reading the cookies over
Chrome's DevTools protocol:

```sh
# Chrome must be running with a debug port:
google-chrome --remote-debugging-port=9232

python3 scripts/export-youtube-cookies.py
# -> wrote 45 YouTube cookies to ~/.config/stream-player/cookies.txt
```

Options: `--port` (default `9232`), `--out` (default
`~/.config/stream-player/cookies.txt`).

---

## Known limitations

**YouTube quality is capped by the muxed-format strategy.** The player asks
yt-dlp for a single *progressive* (muxed) HTTP file so that one URL maps to
one pipeline. That is what YouTube serves at 360–720p. Getting 1080p means
requesting the DASH audio and video streams separately and remuxing them,
which requires yt-dlp to download and merge into a local file first.

**No subtitles.** `gpui-video-player` 0.1.3 parses subtitle text but keeps it
in a `pub(crate)` field and paints it internally only; there is no public
accessor for this app to use.

**Linux rendering is CPU-side.** Upstream's hardware path is macOS-only
(CVPixelBuffer). On Linux every frame goes through an NV12 → BGRA conversion
and a sprite-atlas upload. That is comfortable at 1080p30 and becomes the
bottleneck at 4K.

**Audio-only sources have no seek until the duration is known.** For
progressive HTTP media the duration often only becomes queryable once the
demuxer has indexed part of the stream; the player re-queries until it has one.

**Live streams disable seeking.** Deliberate: a flush-seek on a live source
stalls the stream.

**No hardware-accelerated decode pipeline.** The upstream crate requests
NV12 specifically, so a `glimagesink` / `vah264dec` path would require
rebuilding the pipeline via `Video::from_gst_pipeline` with different caps.

---

## Troubleshooting

**Startup takes minutes and the process sits in `D` state.** GPUI's text stack
calls `fontdb::Database::load_system_fonts()` at startup, which enumerates
every font fontconfig knows about. On a machine with a very large font
collection (this project was developed on one with **14 GB / 9,231 files** under
`~/.local/share/fonts`) it can spend minutes in page faults before the first
frame. The app now handles this itself: before opening a window it counts the
files in `~/.local/share/fonts` and `~/.fonts`, and past 500 it writes a
scoped config to `~/.cache/stream-player/fonts-scoped.conf` (system font
directories only) and points `FONTCONFIG_FILE` at it for its own process.
An explicit `FONTCONFIG_FILE` in the environment always wins, and if the
automatic scoping is not wanted, `FONTCONFIG_FILE=/etc/fonts/fonts.conf`
restores full enumeration.

Related: `fontdb` may print `Failed to load a font face ... doesn't have a
family name` warnings for valid fonts (it is a fontdb name-table quirk, not
broken files). The scoped scan avoids most of it; the warnings themselves are
harmless.

**`gstreamer-sys` fails with "The system library gstreamer-1.0 was not found".**
A `pkg-config` earlier in `PATH` is shadowing the system one. Check with
`which -a pkg-config`, then use the committed `.cargo/config.toml` or export
`PKG_CONFIG=/usr/bin/pkg-config` yourself.

**The `Open` button reports it could not open the file dialog.** The XDG
portal is not running. Install `xdg-desktop-portal` plus a backend
(`xdg-desktop-portal-gtk` or `xdg-desktop-portal-kde`). You can always paste
a path into the text box instead.

**Audio-only URLs fail to play.** `ffprobe` is missing, so the player cannot
tell that the stream has no video track. Install `ffmpeg`.

**A panel row's artwork is missing but everything else renders.** In a GPUI
flex column, a child that overflows gets `flex_shrink: 1` by default and
collapses to zero height *before* a sibling scroll container can scroll.
Fixed-height blocks in the panel (artwork, 1 px dividers) therefore need
`flex_none()` — see `artwork()` and `divider()` in `ui.rs`.

---

## Repository layout

```
stream-player/
├── Cargo.toml              dependencies + the xattr patch
├── Cargo.lock              pinned versions (1,535 crates)
├── .cargo/config.toml      pins the system pkg-config
├── run.sh                  build-if-stale + launch
├── scripts/
│   └── export-youtube-cookies.py   CDP cookie exporter
├── src/
│   ├── main.rs             entry point, dependency checks, CLI argument
│   ├── source.rs           URL classification, yt-dlp/ffprobe resolution
│   ├── quality.rs          default quality (720p) selection for YouTube
│   ├── player.rs           Engine: video backend + audio backend, transport
│   ├── metadata.rs         info-panel facts, artwork download/extraction
│   ├── text_input.rs       single-line editable text field for GPUI 0.2
│   └── ui.rs               layout, transport bar, seek bar, info panel
├── vendor/
│   └── xattr/              patched xattr 0.2.3 (one line)
└── README.md
```

Roughly **3,800 lines** of Rust and Python; `ui.rs` is the largest module at
~1,340 lines.

---

## Development

```sh
cargo build            # debug
cargo build --release  # release
cargo test             # 13 unit tests
cargo fmt
cargo clippy
```

The tests cover URL classification (extractor routing, direct media,
manifests, query-string host confusion, garbage rejection, `file://`
percent-encoding), yt-dlp error translation, the panel's bitrate/date
formatting, description truncation, and range clamping in the text field.

### Upstream dependencies

`gpui` is pre-1.0 and moves fast, and `gpui-video-player` is a
single-maintainer project (bus factor 1) with **zero reverse dependencies** on
crates.io. Treat upgrades to either as deliberate work rather than a routine
`cargo update`:

- `gpui` 0.2.x — expect API churn between minor versions.
- `gpui-video-player` 0.1.x — the `gpui` and `gpui-pre` features pin exact
  versions of Zed's pre-release snapshot crates.
- `gpui-pre` is published by a third party, not Zed.

---

## Contributing

**This project is young and the feature set is deliberately narrow. It wants
your help to grow.**

The player currently handles exactly one source at a time, resolves YouTube
through a single muxed progressive stream, and renders subtitles nowhere. Each
of those is a decision that was made to get a working player on screen, not a
principle. They are the most obvious places where a contribution would
change the experience the most.

### Where help would have the most impact

**Higher quality from YouTube (and other DASH sources).** Right now the
resolver asks `yt-dlp` for a single muxed HTTP file, which caps quality at
what YouTube serves progressive — usually 360–720p. The real fix is to
request the separate audio and video DASH streams, remux them, and hand the
player a local file. `gpui-video-player` exposes
`Video::from_gst_pipeline`, so a custom pipeline is already possible.

**Subtitle rendering.** The upstream crate parses subtitle *text* into a
`pub(crate)` field and paints it internally. Upstreaming an accessor, or
forking the element to render a cue track, would unlock subtitles.

**A playlist / queue.** The `Engine` abstraction already cleanly separates
"what is playing" from "how it is rendered"; a queue is mostly UI plus a
little state.

**Hardware-accelerated decoding on Linux.** The upstream rendering path
converts NV12 to BGRA on the CPU and uploads a sprite per frame, which is
comfortable at 1080p30 and a bottleneck at 4K. A `glupload` / `wgpu` path
would remove the CPU conversion entirely.

**Better metadata coverage.** The panel currently shows what `yt-dlp -J` and
`ffprobe` happen to provide. Chapters, playlists, subtitles, comments,
"now playing" integrations and richer artwork handling are all open.

**Playback persistence.** Nothing is remembered between runs: no library, no
recent files, no resume positions, no per-source settings.

**Platforms.** The code is Linux-only in practice. Windows and macOS would
need the platform-specific paths in the upstream crate revisited.

**Packaging.** There is no `.desktop` file, no AppImage / Flatpak, no
systemd user unit. Packaging is usually the last mile that stops a project
from being usable by anyone but its author.

### How to contribute

```sh
git clone https://github.com/ysofologis/APP__GPUStreamPlayer.git
cd APP__GPUStreamPlayer
cargo build --release
cargo test
```

Open an issue describing what you want to change before starting anything
large — the upstream `gpui` API moves between versions, and a short
discussion avoids a lot of rework.

Useful things to keep in mind while hacking:

- `player.rs` keeps both playback backends behind one `Engine` enum. Add new
  transport capabilities there, not in the UI.
- `source.rs` owns URL classification and resolution. If a new site needs
  special handling, that is where it goes.
- `ui.rs` is the largest module (~1,340 lines) and the one most likely to
  need splitting.
- `text_input.rs` is a self-contained GPUI text widget; it is usable on its
  own if you want to lift it.
- `vendor/xattr` is a **patched copy of a third-party crate**. Do not edit it
  casually — the one-line change is documented in `vendor/xattr/src/sys/mod.rs`
  and the reasoning lives in `Cargo.toml`.

### Upstream dependencies

Contributions that also help upstream are very welcome:

- [`gpui-video-player`](https://crates.io/crates/gpui-video-player) is a
  single-maintainer project with **zero reverse dependencies**. An issue
  reporting a GPUI 0.2 API break, or a pull request exposing the subtitle
  text, would be genuinely useful.
- [`gpui`](https://github.com/zed-industries/zed) is pre-1.0 and moves fast.
  Patches for real bugs are welcome upstream; the community benefits more
  than this repository does.

---

## License

Released under the [MIT License](LICENSE).

```text
MIT License

Copyright (c) 2026 stream-player contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

It links against `gpui` (Apache-2.0/MIT), `gpui-video-player` (MIT),
`gstreamer-rs` (MIT) and a patched copy of `xattr` (MIT, see
`vendor/xattr/LICENSE-MIT`).
