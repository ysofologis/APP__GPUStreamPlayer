//! stream-player — a GPUI stream player for Linux.
//!
//! Supports YouTube (via `yt-dlp`), direct audio streams (MP3/Icecast/OGG),
//! HLS/DASH manifests, and local media files.

mod metadata;
mod player;
mod source;
mod text_input;
mod ui;

use gpui::{App, Application};

/// A user font directory holding more than this many files makes GPUI's
/// startup font scan unusable: `gpui` -> `cosmic-text` -> `fontdb` parses
/// every font file itself (a 14 GB / 9,231-file `~/.local/share/fonts` kept
/// this app from drawing a window for 40+ seconds, versus ~0.3 s scoped).
const FONT_FILE_LIMIT: usize = 500;

/// Scope the font scan when the user's font directories are huge.
///
/// `fontdb::Database::load_system_fonts()` (called unconditionally when GPUI
/// builds its text system) honours `FONTCONFIG_FILE`. No GPUI hook exists to
/// skip the scan, so the app sets the variable *for itself* before any window
/// opens: a generated config listing only the system font directories.
/// Never touches the user's environment permanently and never overrides an
/// explicit choice: if `FONTCONFIG_FILE` is already set, or the user font
/// directories are small (or absent), nothing changes.
fn limit_font_scan() {
    if std::env::var_os("FONTCONFIG_FILE").is_some() {
        return; // operator already chose a config
    }

    let home = match std::env::var_os("HOME") {
        Some(home) => std::path::PathBuf::from(home),
        None => return,
    };
    let user_dirs = [
        home.join(".local/share/fonts"),
        home.join(".fonts"),
    ];

    let mut total = 0usize;
    for dir in user_dirs {
        if dir.is_dir() {
            total += count_files_capped(&dir, FONT_FILE_LIMIT);
            if total > FONT_FILE_LIMIT {
                break;
            }
        }
    }
    if total <= FONT_FILE_LIMIT {
        return;
    }

    let config = match write_scoped_fontconfig() {
        Some(path) => path,
        None => {
            log::warn!("{total} user font files slow font startup, but could not write a scoped fontconfig; using the system one");
            return;
        }
    };
    // Safe: runs before Application::new() and any thread GPUI spawns exists.
    std::env::set_var("FONTCONFIG_FILE", &config);
    log::info!(
        "{total}+ user font files: scoped the startup font scan to system fonts ({})",
        config.display()
    );
}

/// Count files under `dir`, stopping as soon as `cap` is exceeded. The early
/// exit keeps the check cheap: a healthy font dir is walked fully (small),
/// a pathological one is abandoned after `cap` entries.
fn count_files_capped(dir: &std::path::Path, cap: usize) -> usize {
    let mut count = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                count += 1;
                if count > cap {
                    return count;
                }
            }
        }
    }
    count
}

/// Write a fontconfig file listing only the system font directories, in the
/// app's cache dir. Contents mirror what a minimal distro setup provides.
fn write_scoped_fontconfig() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })?;
    let dir = base.join("stream-player");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("fonts-scoped.conf");

    let mut dirs = vec!["/usr/share/fonts".to_string(), "/usr/local/share/fonts".to_string()];
    dirs.retain(|d| std::path::Path::new(d).is_dir());
    if dirs.is_empty() {
        return None;
    }
    let body = dirs
        .iter()
        .map(|d| format!("  <dir>{d}</dir>\n"))
        .collect::<String>();
    let config = format!(
        "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"fonts.dtd\">\n<fontconfig>\n{body}  <cachedir>{}</cachedir>\n</fontconfig>\n",
        dir.join("fontconfig").display()
    );
    std::fs::write(&path, config).ok()?;
    Some(path)
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("stream_player=info,warn"),
    )
    .init();

    // Must happen before `Application::new()`: the font scan runs while GPUI
    // builds its text system, which happens on the first window.
    limit_font_scan();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_counting_stops_early() {
        let dir = std::env::temp_dir().join("sp-fontcap-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..12 {
            std::fs::write(dir.join(format!("f{i}.ttf")), b"x").unwrap();
        }
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/f13.ttf"), b"x").unwrap();
        // cap exceeded: returns *more* than cap, proving early exit exists
        assert!(count_files_capped(&dir, 5) > 5);
        // under cap: exact count, subdirectory included
        assert_eq!(count_files_capped(&dir, 50), 13);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_counts_zero() {
        assert_eq!(
            count_files_capped(std::path::Path::new("/nonexistent-dir-for-test"), 100),
            0
        );
    }
}
