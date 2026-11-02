//! Playback engine.
//!
//! # Why there are two backends
//!
//! `gpui-video-player::Video::new` builds this pipeline internally:
//!
//! ```text
//! playbin video-sink="videoscale ! videoconvert ! appsink caps=video/x-raw,format=NV12"..."
//! ```
//!
//! and then immediately queries the appsink pad's *current* caps for
//! `width`/`height`/`framerate`, returning `Error::Caps` if they are absent.
//! For an MP3, an Icecast radio stream, or any audio-only source there is no
//! video pad, so that construction always fails. There is no option to skip the
//! video sink.
//!
//! So this module has two paths:
//!
//! * [`Engine::Video`] -- the crate, for sources with a video track. We get its
//!   GPU/atlas rendering for free.
//! * [`Engine::Audio`] -- a plain `playbin` we own, with `autoaudiosink`. No
//!   frame delivery, just a transport with position/duration/EOS.
//!
//! The UI talks to [`Engine`] and never cares which one is live.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_video_player::{gst, Video, VideoOptions};
use gst::prelude::*;

/// Transport state shared by both backends so the UI can render it uniformly.
pub struct Transport {
    duration_ms: AtomicI64,
    is_live: AtomicBool,
}

impl Transport {
    fn new() -> Self {
        Self {
            duration_ms: AtomicI64::new(0),
            is_live: AtomicBool::new(false),
        }
    }

    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms.load(Ordering::Relaxed).max(0) as u64)
    }

    pub fn is_live(&self) -> bool {
        self.is_live.load(Ordering::Relaxed)
    }

    fn set_duration(&self, duration: Duration) {
        let ms = duration.as_millis().min(i64::MAX as u128) as i64;
        self.duration_ms.store(ms, Ordering::Relaxed);
        self.is_live.store(ms == 0, Ordering::Relaxed);
    }
}

/// A live playback session.
pub enum Engine {
    /// Video (or audio+video) rendered through the GPUI video element.
    Video {
        video: Video,
        transport: Arc<Transport>,
    },
    /// Audio-only playback through an owned `playbin`.
    Audio(AudioSession),
}

impl Engine {
    pub fn transport(&self) -> Arc<Transport> {
        match self {
            Engine::Video { transport, .. } => transport.clone(),
            Engine::Audio(session) => session.transport.clone(),
        }
    }

    /// Start a session. `has_video` comes from the ffprobe pass in `source.rs`;
    /// when the probe failed we assume video and fall back on a runtime check.
    pub fn open(url: &str, has_video: bool) -> Result<Self, String> {
        if has_video {
            match Self::open_video(url) {
                Ok(engine) => return Ok(engine),
                Err(video_error) => {
                    // A probe can be wrong (adaptive manifests, unusual
                    // containers). If the video pipeline cannot start, try the
                    // audio one before surfacing the failure.
                    log::warn!("video pipeline failed ({video_error}), trying audio-only");
                    return Self::open_audio(url).map_err(|audio_error| {
                        format!("{video_error}; audio fallback also failed: {audio_error}")
                    });
                }
            }
        }
        Self::open_audio(url).map_err(|error| {
            log::warn!("audio pipeline failed ({error}), trying video");
            format!("audio pipeline failed: {error}")
        })
    }

    fn open_audio(url: &str) -> Result<Self, String> {
        AudioSession::open(url).map(Engine::Audio)
    }

    fn open_video(url: &str) -> Result<Self, String> {
        let parsed = url::Url::parse(url).map_err(|e| format!("bad URL: {e}"))?;

        // Buffer a handful of frames: enough to ride out a scheduler hiccup,
        // small enough that seeking feels immediate.
        let options = VideoOptions {
            frame_buffer_capacity: Some(6),
            looping: Some(false),
            speed: Some(1.0),
        };

        let video = Video::new_with_options(&parsed, options)
            .map_err(|e| format!("cannot open video: {e}"))?;

        let transport = Arc::new(Transport::new());
        transport.set_duration(video.duration());

        Ok(Engine::Video { video, transport })
    }

    /// Handle for the GPUI element; `None` for audio sessions.
    pub fn video_handle(&self) -> Option<Video> {
        match self {
            Engine::Video { video, .. } => Some(video.clone()),
            Engine::Audio(_) => None,
        }
    }

    pub fn is_playing(&self) -> bool {
        match self {
            Engine::Video { video, .. } => !video.paused(),
            Engine::Audio(session) => session.is_playing(),
        }
    }

    pub fn is_audio_only(&self) -> bool {
        matches!(self, Engine::Audio(_))
    }

    pub fn play_pause(&self) -> Result<(), String> {
        match self {
            Engine::Video { video, .. } => {
                video.set_paused(!video.paused());
                Ok(())
            }
            Engine::Audio(session) => session.play_pause(),
        }
    }

    pub fn seek(&self, position: Duration) -> Result<(), String> {
        match self {
            Engine::Video { video, .. } => video
                .seek(position, false)
                .map_err(|e| format!("seek failed: {e}")),
            Engine::Audio(session) => session.seek(position),
        }
    }

    pub fn position(&self) -> Duration {
        match self {
            Engine::Video { video, .. } => video.position(),
            Engine::Audio(session) => session.position(),
        }
    }

    pub fn duration(&self) -> Duration {
        let transport = self.transport();
        let own = transport.duration();
        if own > Duration::ZERO {
            return own;
        }
        match self {
            Engine::Video { video, .. } => video.duration(),
            Engine::Audio(session) => session.duration(),
        }
    }

    pub fn set_volume(&self, volume: f64) -> Result<(), String> {
        let clamped = volume.clamp(0.0, 1.0);
        match self {
            Engine::Video { video, .. } => {
                video.set_volume(clamped);
                Ok(())
            }
            Engine::Audio(session) => session.set_volume(clamped),
        }
    }

    pub fn volume(&self) -> f64 {
        match self {
            Engine::Video { video, .. } => video.volume(),
            Engine::Audio(session) => session.volume(),
        }
    }

    pub fn set_muted(&self, muted: bool) -> Result<(), String> {
        match self {
            Engine::Video { video, .. } => {
                video.set_muted(muted);
                Ok(())
            }
            Engine::Audio(session) => session.set_muted(muted),
        }
    }

    pub fn is_muted(&self) -> bool {
        match self {
            Engine::Video { video, .. } => video.muted(),
            Engine::Audio(session) => session.is_muted(),
        }
    }

    pub fn set_speed(&self, speed: f64) -> Result<(), String> {
        match self {
            Engine::Video { video, .. } => video
                .set_speed(speed)
                .map_err(|e| format!("cannot set speed: {e}")),
            Engine::Audio(session) => session.set_speed(speed),
        }
    }

    pub fn speed(&self) -> f64 {
        match self {
            Engine::Video { video, .. } => video.speed(),
            Engine::Audio(session) => session.speed(),
        }
    }

    // NOTE: subtitle text is not exposed by gpui-video-player's public API in
    // 0.1.3 (`subtitle_text` is `pub(crate)` and only painted internally), so
    // there is no subtitle accessor here yet.
}

// ---------------------------------------------------------------------------
// Audio backend
// ---------------------------------------------------------------------------

/// An owned `playbin` for audio-only sources.
pub struct AudioSession {
    pipeline: gst::Pipeline,
    transport: Arc<Transport>,
    volume: Mutex<f64>,
    muted: Mutex<bool>,
    speed: Mutex<f64>,
    paused: AtomicBool,
    eos: Arc<AtomicBool>,
    /// Keeps the bus-watching thread from outliving the pipeline.
    alive: Arc<AtomicBool>,
    bus_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl AudioSession {
    fn open(url: &str) -> Result<Self, String> {
        gst::init().map_err(|e| format!("cannot init GStreamer: {e}"))?;

        // `autoaudiosink` picks PulseAudio/PipeWire/ALSA at runtime, which is
        // what we want on an unknown Linux desktop.
        let audio_sink = gst::ElementFactory::make("autoaudiosink")
            .build()
            .map_err(|e| format!("no audio sink available: {e}"))?;

        let pipeline = gst::ElementFactory::make("playbin")
            .build()
            .map_err(|e| format!("cannot create playbin: {e}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "playbin is not a pipeline".to_string())?;
        pipeline.set_property("uri", url);
        pipeline.set_property("audio-sink", &audio_sink);

        let eos = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let transport = Arc::new(Transport::new());

        // Some pipelines only report duration after preroll; poll briefly.
        pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| format!("cannot start pipeline: {e}"))?;

        let (state_result, _, _) = pipeline.state(gst::ClockTime::from_seconds(10));
        state_result.map_err(|e| format!("pipeline failed to preroll: {e}"))?;

        if let Some(duration) = pipeline.query_duration::<gst::ClockTime>() {
            transport.set_duration(Duration::from_nanos(duration.nseconds()));
        }

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| format!("cannot start playback: {e}"))?;

        let session = Self {
            pipeline,
            transport,
            volume: Mutex::new(1.0),
            muted: Mutex::new(false),
            speed: Mutex::new(1.0),
            paused: AtomicBool::new(false),
            eos: eos.clone(),
            alive: alive.clone(),
            bus_thread: Mutex::new(None),
        };

        session.spawn_bus_watcher(eos, alive);

        Ok(session)
    }

    /// Drain the bus so errors surface in the log and EOS flips our flag.
    fn spawn_bus_watcher(&self, eos: Arc<AtomicBool>, alive: Arc<AtomicBool>) {
        let Some(bus) = self.pipeline.bus() else {
            return;
        };

        let handle = std::thread::Builder::new()
            .name("gst-bus".into())
            .spawn(move || {
                while alive.load(Ordering::Relaxed) {
                    let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
                        continue;
                    };
                    use gst::MessageView;
                    match message.view() {
                        MessageView::Eos(..) => {
                            eos.store(true, Ordering::SeqCst);
                        }
                        MessageView::Error(err) => {
                            log::error!(
                                "playback error from {:?}: {} ({:?})",
                                err.src().map(|s| s.path_string()),
                                err.error(),
                                err.debug()
                            );
                        }
                        _ => {}
                    }
                }
            })
            .ok();

        if let Ok(mut slot) = self.bus_thread.lock() {
            *slot = handle;
        }
    }

    fn is_playing(&self) -> bool {
        !self.paused.load(Ordering::Relaxed)
    }

    fn play_pause(&self) -> Result<(), String> {
        self.set_paused(self.is_playing())
    }

    fn set_paused(&self, paused: bool) -> Result<(), String> {
        let target = if paused {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        self.pipeline
            .set_state(target)
            .map_err(|e| format!("cannot change state: {e}"))?;
        self.paused.store(paused, Ordering::Relaxed);
        Ok(())
    }

    fn seek(&self, position: Duration) -> Result<(), String> {
        // Live streams have no duration; seeking would break the pipeline.
        if self.transport.is_live() {
            return Err("live stream: seeking is disabled".into());
        }
        self.pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_nseconds(position.as_nanos().min(u64::MAX as u128) as u64),
            )
            .map_err(|e| format!("seek failed: {e}"))?;
        self.eos.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn position(&self) -> Duration {
        self.pipeline
            .query_position::<gst::ClockTime>()
            .map(|p| Duration::from_nanos(p.nseconds()))
            .unwrap_or_default()
    }

    fn duration(&self) -> Duration {
        // Progressive HTTP media frequently has no duration at preroll; it
        // becomes queryable once the demuxer has indexed enough of the stream.
        // Re-query on every read and cache the first non-zero answer, otherwise
        // the seek bar stays disabled forever on long MP3s.
        if let Some(queried) = self.pipeline.query_duration::<gst::ClockTime>() {
            let duration = Duration::from_nanos(queried.nseconds());
            if duration > Duration::ZERO {
                self.transport.set_duration(duration);
                return duration;
            }
        }
        self.transport.duration()
    }

    fn set_volume(&self, volume: f64) -> Result<(), String> {
        let mut current = self.volume.lock().map_err(|_| "lock poisoned")?;
        *current = volume;
        // playbin exposes `volume` as a linear 0..=10 property.
        self.pipeline.set_property("volume", volume);
        Ok(())
    }

    fn volume(&self) -> f64 {
        self.volume.lock().map(|v| *v).unwrap_or(1.0)
    }

    fn set_muted(&self, muted: bool) -> Result<(), String> {
        let mut current = self.muted.lock().map_err(|_| "lock poisoned")?;
        *current = muted;
        self.pipeline.set_property("mute", muted);
        Ok(())
    }

    fn is_muted(&self) -> bool {
        self.muted.lock().map(|m| *m).unwrap_or(false)
    }

    fn set_speed(&self, speed: f64) -> Result<(), String> {
        if !(0.1..=8.0).contains(&speed) {
            return Err("speed must be between 0.1 and 8.0".into());
        }
        // A rate change needs a flushing seek to take effect on playbin.
        let position = self.position();
        self.pipeline
            .seek(
                speed,
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                gst::ClockTime::from_nseconds(position.as_nanos().min(u64::MAX as u128) as u64),
                gst::SeekType::End,
                gst::ClockTime::from_seconds(0),
            )
            .map_err(|e| format!("cannot set speed: {e}"))?;
        if let Ok(mut current) = self.speed.lock() {
            *current = speed;
        }
        Ok(())
    }

    fn speed(&self) -> f64 {
        self.speed.lock().map(|s| *s).unwrap_or(1.0)
    }

}

impl Drop for AudioSession {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Ok(mut slot) = self.bus_thread.lock() {
            if let Some(handle) = slot.take() {
                let _ = handle.join();
            }
        }
    }
}