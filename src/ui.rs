//! The application window: layout, transport controls, and the event loop that
//! keeps the timeline in sync with the pipeline.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, prelude::*, px, rgb, rgba, size, App, Bounds, Context, CursorStyle, FocusHandle,
    KeyDownEvent, MouseButton, MouseDownEvent, ObjectFit, Pixels, Point, Render, Window,
    WindowBounds, WindowOptions,
};
use gpui_video_player::video;

use crate::metadata::{self, TrackInfo};
use crate::player::Engine;
use crate::source::{self, ResolveError, Resolved};

const WINDOW_W: f32 = 1280.0;
const WINDOW_H: f32 = 760.0;
/// Width of the right-hand info panel.
const PANEL_W: f32 = 320.0;

// Palette -- one place to retheme.
const BG: u32 = 0x0d1117;
const PANEL: u32 = 0x161b22;
const BORDER: u32 = 0x30363d;
const TEXT: u32 = 0xe6edf3;
const MUTED: u32 = 0x8b949e;
const ACCENT: u32 = 0x58a6ff;
const DANGER: u32 = 0xf85149;

/// Which control a keyboard shortcut or click activated.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    PlayPause,
    Back10,
    Forward10,
    Back60,
    Forward60,
    VolumeUp,
    VolumeDown,
    Mute,
    Fullscreen,
    SpeedReset,
    SpeedUp,
    SpeedDown,
}

pub struct PlayerView {
    /// `None` before the first successful resolve.
    engine: Option<Engine>,
    /// What the user typed / dropped.
    url_input: String,
    /// Set while a resolve is in flight so the UI can show a spinner state.
    loading: bool,
    /// Last error worth showing; cleared on the next attempt.
    error: Option<String>,
    /// Display label of the current source.
    current: Option<Resolved>,
    /// Cached transport snapshot, refreshed by a timer so `render` stays cheap.
    position: Duration,
    duration: Duration,
    is_playing: bool,
    is_live: bool,
    audio_only: bool,
    volume: f64,
    muted: bool,
    speed: f64,
    /// Set while the user drags the seek bar, to stop the timer fighting them.
    scrubbing: Option<f32>,
    focus: FocusHandle,
    /// Guards against a second `poll_transport` task being spawned.
    polling: Arc<AtomicBool>,
    /// Shown beneath the video while a stream has no video track.
    last_load_ms: Option<u128>,
    /// Metadata for the right-hand info panel.
    info: TrackInfo,
    /// True while `metadata::fetch` is running for the current source.
    info_loading: bool,
    /// Scroll position of the info panel; reset to the top on every new source.
    info_scroll: gpui::ScrollHandle,
}

impl PlayerView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let view = Self {
            engine: None,
            url_input: String::new(),
            loading: false,
            error: None,
            current: None,
            position: Duration::ZERO,
            duration: Duration::ZERO,
            is_playing: false,
            is_live: false,
            audio_only: false,
            volume: 0.8,
            muted: false,
            speed: 1.0,
            scrubbing: None,
            focus: cx.focus_handle(),
            polling: Arc::new(AtomicBool::new(false)),
            last_load_ms: None,
            info: TrackInfo::default(),
            info_loading: false,
            info_scroll: gpui::ScrollHandle::new(),
        };
        view.start_transport_poll(cx);
        view
    }

    /// Poll the pipeline 10x/second and re-render only when something changed.
    ///
    /// The video element drives its own repaints from frame arrival, but the
    /// timeline, the play/pause glyph, and position text have no such signal.
    fn start_transport_poll(&self, cx: &mut Context<Self>) {
        if self.polling.swap(true, Ordering::SeqCst) {
            return;
        }
        let flag = self.polling.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let keep_going = this
                    .update(cx, |view, cx| {
                        let changed = view.refresh_transport();
                        if changed {
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    flag.store(false, Ordering::SeqCst);
                    break;
                }
            }
        })
        .detach();
    }

    /// Pull a fresh snapshot; returns true if anything user-visible changed.
    fn refresh_transport(&mut self) -> bool {
        let Some(engine) = &self.engine else {
            return false;
        };
        let position = engine.position();
        let duration = engine.duration();
        let playing = engine.is_playing();
        let live = engine.transport().is_live();

        let changed = position.abs_diff(self.position) > Duration::from_millis(120)
            || duration != self.duration
            || playing != self.is_playing
            || live != self.is_live;

        self.position = position;
        self.duration = duration;
        self.is_playing = playing;
        self.is_live = live;
        changed
    }

    /// Submit `url_input` for resolution, then start playback.
    fn load_url(&mut self, cx: &mut Context<Self>) {
        let input = self.url_input.trim().to_string();
        if input.is_empty() || self.loading {
            return;
        }
        self.error = None;
        self.loading = true;
        cx.notify();

        let started = std::time::Instant::now();
        cx.spawn(async move |this, cx| {
            // Resolution shells out (yt-dlp / ffprobe); keep it off the UI thread.
            let resolved = cx
                .background_executor()
                .spawn(async move { source::resolve(&input) })
                .await;

            let elapsed = started.elapsed().as_millis();
            this.update(cx, |view, cx| {
                view.loading = false;
                view.last_load_ms = Some(elapsed);
                match resolved {
                    Ok(resolved) => {
                        // Start the metadata fetch before moving `resolved` into
                        // the engine, so the panel fills in while the first
                        // frames are still decoding.
                        view.load_metadata(&resolved.kind, resolved.probe.duration_secs, cx);
                        match Engine::open(
                            &resolved.kind.playable_url(),
                            resolved.probe.has_video,
                        ) {
                            Ok(engine) => {
                                view.apply_new_engine(engine, resolved);
                            }
                            Err(error) => {
                                view.error = Some(error);
                                view.engine = None;
                            }
                        }
                    }
                    Err(error) => {
                        view.error = Some(describe_error(error));
                        view.engine = None;
                        view.info = TrackInfo::default();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Fetch panel metadata on a background thread.
    ///
    /// Separate from `load_url` on purpose: `yt-dlp -J` and poster extraction
    /// take hundreds of milliseconds, and the video should start playing
    /// without waiting for the panel to fill in.
    fn load_metadata(&mut self, kind: &source::SourceKind, duration: Option<f64>, cx: &mut Context<Self>) {
        let kind = kind.clone();
        self.info = TrackInfo::default();
        self.info_loading = true;
        self.info_scroll.set_offset(gpui::Point {
            x: gpui::px(0.0),
            y: gpui::px(0.0),
        });

        cx.spawn(async move |this, cx| {
            let info = cx
                .background_executor()
                .spawn(async move { metadata::fetch(&kind, duration) })
                .await;

            this.update(cx, |view, cx| {
                view.info = info;
                view.info_loading = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_new_engine(&mut self, engine: Engine, resolved: Resolved) {
        let audio_only = engine.is_audio_only();
        let duration = engine.duration();
        let live = engine.transport().is_live();

        // Reapply the user's volume/mute preferences to the new pipeline.
        let _ = engine.set_volume(self.volume);
        let _ = engine.set_muted(self.muted);

        self.engine = Some(engine);
        self.current = Some(resolved);
        self.audio_only = audio_only;
        self.duration = duration;
        self.position = Duration::ZERO;
        self.is_live = live;
        self.is_playing = true;
        self.error = None;
        self.focus = self.focus.clone();
    }

    /// Execute a transport action against the live engine.
    fn dispatch(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let Some(engine) = &self.engine else {
            if matches!(action, Action::PlayPause) {
                self.load_url(cx);
            }
            return;
        };

        let result = match action {
            Action::PlayPause => engine.play_pause(),
            Action::Back10 => seek_relative(engine, -10.0),
            Action::Forward10 => seek_relative(engine, 10.0),
            Action::Back60 => seek_relative(engine, -60.0),
            Action::Forward60 => seek_relative(engine, 60.0),
            Action::VolumeUp => engine.set_volume((engine.volume() + 0.05).min(1.0)),
            Action::VolumeDown => engine.set_volume((engine.volume() - 0.05).max(0.0)),
            Action::Mute => engine.set_muted(!engine.is_muted()),
            Action::Fullscreen => {
                window.toggle_fullscreen();
                Ok(())
            }
            Action::SpeedReset => engine.set_speed(1.0),
            Action::SpeedUp => {
                let next = (engine.speed() + 0.25).min(4.0);
                engine.set_speed(next)
            }
            Action::SpeedDown => {
                let next = (engine.speed() - 0.25).max(0.25);
                engine.set_speed(next)
            }
        };

        if let Err(error) = result {
            self.error = Some(error);
        }

        // Re-read everything the action could have changed.
        if let Some(engine) = &self.engine {
            self.volume = engine.volume();
            self.muted = engine.is_muted();
            self.speed = engine.speed();
            self.is_playing = engine.is_playing();
            self.position = engine.position();
            self.duration = engine.duration();
        }
        cx.notify();
    }


    // -- right-hand info panel ----------------------------------------------

    /// A labelled "key / value" row. Returns `None` when the value is missing so
    /// absent facts leave no gap.
    fn info_row(&self, label: &'static str, value: Option<String>) -> Option<impl IntoElement> {
        let value = value.filter(|v| !v.is_empty())?;
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x6e7681))
                        .child(label),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(TEXT))
                        .child(value),
                ),
        )
    }

    /// Horizontal rule. `flex_none` because a 1px flex child in an overflowing
    /// column shrinks to nothing.
    fn divider(&self) -> impl IntoElement {
        div().flex_none().h(px(1.0)).w_full().bg(rgb(BORDER))
    }

    fn section(&self, title: &'static str) -> impl IntoElement {
        div()
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgb(MUTED))
            .child(title)
    }

    /// Cover art, or a generated placeholder block when there is none.
    fn artwork(&self) -> impl IntoElement {
        // No aspect-ratio helper in this GPUI version; a fixed square-ish box
        // keeps the panel layout stable regardless of artwork aspect.
        // `flex_none` is essential: the panel column overflows its height, and a
        // flex child with the default `flex_shrink: 1` collapses to zero height
        // before the scroll container ever gets a chance to scroll.
        let frame = div()
            .flex_none()
            .w_full()
            .h(px(190.0))
            .rounded_lg()
            .bg(rgb(0x1c2128))
            .border_1()
            .border_color(rgb(BORDER))
            .overflow_hidden();

        match &self.info.artwork {
            Some(path) => frame.child(
                img(std::sync::Arc::<std::path::Path>::from(path.as_path()))
                    .size_full()
                    .object_fit(ObjectFit::Cover),
            ),
            None => frame.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_3xl()
                    .text_color(rgb(0x30363d))
                    .child(if self.audio_only { "♪" } else { "▮" }),
            ),
        }
    }

    fn info_panel(&self) -> impl IntoElement {
        let info = &self.info;

        let shell = div()
            .id("info-panel")
            .w(px(PANEL_W))
            .flex_none()
            .h_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .overflow_hidden();

        let mut body = div()
            .id("info-scroll")
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .track_scroll(&self.info_scroll)
            .overflow_y_scroll();

        // Nothing loaded yet: say so instead of showing an empty shell.
        if self.current.is_none() {
            return shell
                .items_center()
                .justify_center()
                .gap_2()
                .child(div().text_xl().text_color(rgb(0x30363d)).child("ℹ"))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .text_center()
                        .child("Track details appear here once something is playing"),
                );
        }

        body = body.child(self.artwork());

        // Title block: title, then author.
        if let Some(title) = info.title.clone() {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .line_clamp(3)
                            .child(title),
                    )
                    .when_some(info.author.clone(), |this, author| {
                        this.child(div().text_sm().text_color(rgb(ACCENT)).child(author))
                    }),
            );
        } else if self.info_loading {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("Reading metadata…"),
            );
        }

        // Playback facts.
        let resolution = info.resolution.clone();
        let container = info.container.clone().map(|c| c.to_uppercase());
        let codecs = match (info.video_codec.clone(), info.audio_codec.clone()) {
            (Some(v), Some(a)) => Some(format!("{v} / {a}")),
            (Some(v), None) => Some(v),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let audio_detail = match (info.sample_rate.clone(), info.channels.clone()) {
            (Some(rate), Some(ch)) => Some(format!("{rate} · {ch}")),
            (Some(rate), None) => Some(rate),
            (None, Some(ch)) => Some(ch),
            (None, None) => None,
        };

        let mut facts = div().flex().flex_col().gap_2();
        let mut any = false;
        for row in [
            self.info_row("FORMAT", container),
            self.info_row("RESOLUTION", resolution),
            self.info_row("CODECS", codecs),
            self.info_row("BITRATE", info.bitrate.clone()),
            self.info_row("AUDIO", audio_detail),
        ]
        .into_iter()
        .flatten()
        {
            any = true;
            facts = facts.child(row);
        }
        if any {
            body = body.child(self.divider());
            body = body.child(self.section("STREAM"));
            body = body.child(facts);
        }

        // Popularity / provenance.
        let mut meta = div().flex().flex_col().gap_2();
        let mut any_meta = false;
        let views = info.view_count.map(format_count);
        let likes = info.like_count.map(format_count);
        for row in [
            self.info_row("CHANNEL", None::<String>),
            self.info_row("UPLOADED", info.upload_date.clone()),
            self.info_row("VIEWS", views),
            self.info_row("LIKES", likes),
            self.info_row("SOURCE", info.provider.clone()),
        ]
        .into_iter()
        .flatten()
        {
            any_meta = true;
            meta = meta.child(row);
        }
        if any_meta {
            body = body.child(self.divider());
            body = body.child(self.section("ABOUT"));
            body = body.child(meta);
        }

        // Categories / tags.
        if !info.categories.is_empty() || !info.tags.is_empty() {
            let mut tags = div().flex().flex_wrap().gap_1();
            for cat in info.categories.iter() {
                tags = tags.child(self.chip(cat.clone(), ACCENT));
            }
            for tag in info.tags.iter() {
                tags = tags.child(self.chip(tag.clone(), 0x6e7681));
            }
            body = body.child(self.divider());
            body = body.child(self.section("TAGS"));
            body = body.child(tags);
        }

        // Description last: it is the longest block and the least urgent.
        if let Some(description) = info.description.clone() {
            body = body.child(self.divider());
            body = body.child(self.section("DESCRIPTION"));
            body = body.child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .line_height(gpui::relative(1.45))
                    .child(description),
            );
        }

        shell.child(body)
    }

    fn chip(&self, text: String, color: u32) -> impl IntoElement {
        div()
            .px_2()
            .py(px(2.0))
            .rounded_full()
            .bg(rgba(0xffffff10))
            .border_1()
            .border_color(rgba(0xffffff18))
            .text_xs()
            .text_color(rgb(color))
            .child(text)
    }

    // -- rendering helpers ---------------------------------------------------

    fn control_button(
        &self,
        id: &'static str,
        label: impl IntoElement,
        action: Action,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .text_color(rgb(TEXT))
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(rgba(0xffffff14)).border_color(rgb(ACCENT)))
            .child(label)
            .on_click(cx.listener(move |view, _, window, cx| {
                view.dispatch(action, window, cx);
            }))
    }

    fn seek_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.duration.as_secs_f64();
        let fraction = if total > 0.0 {
            (self.position.as_secs_f64() / total).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let shown = self.scrubbing.unwrap_or(fraction);
        let enabled = !self.is_live && total > 0.0;
        let accent = if enabled { ACCENT } else { MUTED };
        let total_width = WINDOW_W - 96.0;

        let fill_el = div()
            .absolute()
            .left_0()
            .top_0()
            .h_full()
            .w(px(shown * total_width))
            .bg(rgb(accent))
            .rounded_full();

        div()
            .id("seek-bar")
            .h(px(18.0))
            .w_full()
            .flex()
            .items_center()
            .when(enabled, |this| this.cursor(CursorStyle::PointingHand))
            .child(
                div()
                    .relative()
                    .h(px(6.0))
                    .w_full()
                    .bg(rgb(BORDER))
                    .rounded_full()
                    .child(fill_el)
                    .child(
                        div()
                            .absolute()
                            .left(px(shown * total_width - 7.0))
                            .top(px(-4.0))
                            .size(px(14.0))
                            .rounded_full()
                            .bg(rgb(accent)),
                    ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                    if view.is_live || view.duration.is_zero() {
                        return;
                    }
                    view.scrubbing = Some(seek_fraction(event.position, total_width));
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |view, _event, _, cx| {
                    let Some(fraction) = view.scrubbing.take() else {
                        return;
                    };
                    if let Some(engine) = &view.engine {
                        let target =
                            view.duration.mul_f64(fraction as f64);
                        if let Err(error) = engine.seek(target) {
                            view.error = Some(error);
                        }
                        view.position = target;
                    }
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |view, event: &gpui::MouseMoveEvent, _, cx| {
                if view.scrubbing.is_some() {
                    view.scrubbing = Some(seek_fraction(event.position, total_width));
                    cx.notify();
                }
            }))
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let loading = self.loading;
        div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_color(if self.url_input.is_empty() {
                        rgb(MUTED)
                    } else {
                        rgb(TEXT)
                    })
                    .child(if self.url_input.is_empty() {
                        "Paste a YouTube link, an MP3/Icecast URL, an .m3u8, or a local file path…"
                            .to_string()
                    } else {
                        elide_middle(&self.url_input, 96)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _, window, cx| {
                            // Seed the field from the clipboard -- the common case
                            // is "copy a link, then click the bar".
                            if view.url_input.is_empty() {
                                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                                    let text = text.trim().to_string();
                                    if !text.is_empty() {
                                        view.url_input = text;
                                    }
                                }
                            }
                            view.focus = view.focus.clone();
                            window.focus(&view.focus);
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .id("play-url")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(if loading { rgb(PANEL) } else { rgb(ACCENT) })
                    .text_color(if loading { rgb(MUTED) } else { rgb(0x0d1117) })
                    .cursor(CursorStyle::PointingHand)
                    .child(if loading { "Opening…" } else { "Open" })
                    .when(!loading, |this| {
                        this.on_click(cx.listener(|view, _, _, cx| view.load_url(cx)))
                    }),
            )
    }

    fn stage(&self) -> impl IntoElement {
        let base = div()
            .flex_1()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000))
            .rounded_lg()
            .overflow_hidden();

        match (&self.engine, self.audio_only) {
            (Some(engine), false) => match engine.video_handle() {
                Some(handle) => base.child(
                    video(handle)
                        .id("main-video")
                        .buffer_capacity(6),
                ),
                None => base.child(self.audio_placeholder()),
            },
            (Some(_), true) => base.child(self.audio_placeholder()),
            (None, _) => base.child(self.idle_placeholder()),
        }
    }

    fn idle_placeholder(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .text_color(rgb(MUTED))
            .child(div().text_xl().text_color(rgb(TEXT)).child("Stream Player"))
            .child("Paste a URL above and press Open")
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x6e7681))
                    .child("YouTube · MP3/Icecast · HLS · DASH · local files"),
            )
    }

    /// Shown instead of a video surface for audio-only sources.
    fn audio_placeholder(&self) -> impl IntoElement {
        let title = self
            .current
            .as_ref()
            .map(|r| r.kind.label())
            .unwrap_or_else(|| "Audio stream".into());
        let format = self
            .current
            .as_ref()
            .map(|r| r.probe.format.clone())
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| "audio".into());

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .child(div().text_3xl().child("♪"))
            .child(
                div()
                    .text_lg()
                    .text_color(rgb(TEXT))
                    .child(elide_middle(&title, 60)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(format!("audio only · {format}")),
            )
    }

    fn transport_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let play_glyph = if self.is_playing { "❚❚" } else { "▶" };
        let mute_glyph = if self.muted { "🔇" } else { "🔊" };
        let position = format_time(self.position);
        let duration = if self.is_live {
            "LIVE".to_string()
        } else if self.duration.is_zero() {
            "--:--".to_string()
        } else {
            format_time(self.duration)
        };
        let volume_pct = format!("{:.0}%", self.volume * 100.0);
        let speed = format!("{:.2}×", self.speed);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .px_1()
            .child(self.seek_bar(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .w(px(58.0))
                            .child(position),
                    )
                    .child(
                        self.control_button("back60", "« 60", Action::Back60, cx),
                    )
                    .child(
                        self.control_button("back10", "‹ 10", Action::Back10, cx),
                    )
                    .child(
                        self.control_button("playpause", play_glyph, Action::PlayPause, cx),
                    )
                    .child(
                        self.control_button("fwd10", "10 ›", Action::Forward10, cx),
                    )
                    .child(
                        self.control_button("fwd60", "60 »", Action::Forward60, cx),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(format!("{} / {duration}", format_time(self.position))),
                    )
                    .child(div().flex_1())
                    .child(self.control_button("speeddown", "−", Action::SpeedDown, cx))
                    .child(
                        div()
                            .id("speed")
                            .px_2()
                            .py_2()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .cursor(CursorStyle::PointingHand)
                            .child(speed)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.dispatch(Action::SpeedReset, window, cx)
                            })),
                    )
                    .child(self.control_button("speedup", "+", Action::SpeedUp, cx))
                    .child(self.control_button("mute", mute_glyph, Action::Mute, cx))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .w(px(40.0))
                            .child(volume_pct),
                    )
                    .child(self.control_button("voldown", "vol −", Action::VolumeDown, cx))
                    .child(self.control_button("volup", "vol +", Action::VolumeUp, cx))
                    .child(
                        self.control_button("fullscreen", "⛶", Action::Fullscreen, cx),
                    ),
            )
    }

    fn status_line(&self) -> Option<impl IntoElement> {
        if let Some(error) = &self.error {
            return Some(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgba(0xf8514922))
                    .border_1()
                    .border_color(rgb(DANGER))
                    .text_color(rgb(DANGER))
                    .text_sm()
                    .child(error.clone()),
            );
        }
        let resolved = self.current.as_ref()?;
        let mut parts = vec![resolved.kind.label()];
        if !resolved.probe.format.is_empty() {
            parts.push(resolved.probe.format.clone());
        }
        if self.audio_only {
            parts.push("audio only".into());
        }
        if self.is_live {
            parts.push("live".into());
        }
        if let Some(ms) = self.last_load_ms {
            parts.push(format!("{ms} ms"));
        }
        Some(
            div()
                .w_full()
                .text_sm()
                .text_color(rgb(0x6e7681))
                .child(elide_middle(&parts.join(" · "), 130)),
        )
    }

    fn handle_shortcut(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.is_held {
            return;
        }
        let action = match event.keystroke.key.as_str() {
            "space" => {
                if self.url_input.is_empty() {
                    None
                } else {
                    Some(Action::PlayPause)
                }
            }
            "left" => Some(Action::Back10),
            "right" => Some(Action::Forward10),
            "up" => Some(Action::VolumeUp),
            "down" => Some(Action::VolumeDown),
            "f" => Some(Action::Fullscreen),
            "m" => Some(Action::Mute),
            _ => None,
        };
        if let Some(action) = action {
            cx.stop_propagation();
            self.dispatch(action, window, cx);
        }
    }
}

impl Render for PlayerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status_line();
        div()
            .id("root")
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .track_focus(&self.focus)
            .tab_stop(false)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                view.handle_shortcut(event, window, cx)
            }))
            .child(self.header(cx))
            .when_some(status, |this, status| this.child(status))
            .child(
                div()
                    .id("main-row")
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .h_full()
                            .child(self.stage()),
                    )
                    .child(self.info_panel()),
            )
            .child(self.transport_bar(cx))
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn seek_relative(engine: &Engine, delta_secs: f64) -> Result<(), String> {
    let current = engine.position().as_secs_f64();
    let total = engine.duration().as_secs_f64();
    let mut target = (current + delta_secs).max(0.0);
    if total > 0.0 {
        target = target.min(total - 0.05);
    }
    engine.seek(Duration::from_secs_f64(target))
}

fn seek_fraction(position: Point<Pixels>, track_width: f32) -> f32 {
    let x: f32 = position.x.into();
    (x / track_width).clamp(0.0, 1.0)
}

fn format_time(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// `23497972` -> `23.5M`.
fn format_count(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}K", value as f64 / 1_000.0),
        1_000_000..=999_999_999 => format!("{:.1}M", value as f64 / 1_000_000.0),
        _ => format!("{:.1}B", value as f64 / 1_000_000_000.0),
    }
}

/// Keep both ends of a long string, where the useful part usually is.
fn elide_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let keep = (max.saturating_sub(1)) / 2;
    let head: String = chars[..keep].iter().collect();
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("{head}…{tail}")
}

fn describe_error(error: ResolveError) -> String {
    match error {
        ResolveError::MissingBinary(bin) => {
            format!("`{bin}` is required but was not found in PATH.")
        }
        other => other.to_string(),
    }
}

/// Open the main window.
pub fn launch(cx: &mut App, initial_url: Option<String>) {
    let bounds = Bounds::centered(None, size(px(WINDOW_W), px(WINDOW_H)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(880.0), px(560.0))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Stream Player".into()),
            ..Default::default()
        }),
        app_id: Some("stream-player".into()),
        focus: true,
        show: true,
        ..Default::default()
    };

    if let Err(error) = cx.open_window(options, move |window, cx| {
        let view = cx.new(PlayerView::new);
        let handle = view.read(cx).focus.clone();
        window.focus(&handle);
        // A URL passed on the command line starts playing immediately, which
        // makes the app usable from a shell/desktop entry.
        if let Some(url) = initial_url {
            view.update(cx, |view, cx| {
                view.url_input = url;
                view.load_url(cx);
            });
        }
        view
    }) {
        log::error!("cannot open window: {error}");
        cx.quit();
    }
    cx.activate(true);
}
