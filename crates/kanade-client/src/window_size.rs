//! Persisted main-window size (logical pixels, client area).
//!
//! Pure logic only — no Tauri types — so the clamp/validate rules and the
//! load/save fallbacks are unit-tested on every platform. The Windows wiring
//! lives in `app.rs`.
//!
//! Sizes are stored in *logical* pixels so a DPI-scale change between
//! launches does not inflate or shrink the window.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// File name under the per-user app-local data dir.
pub const FILE_NAME: &str = "window-size.json";
const VERSION: u32 = 1;
/// Anything above this (logical px) is treated as garbage, not a real size.
const ABSURD: f64 = 65536.0;
/// A valid file is a few dozen bytes; refuse to read anything big.
const MAX_FILE_BYTES: u64 = 4096;

/// Fallback minimum, matching `tauri.conf.json` (minWidth / minHeight).
pub const DEFAULT_MIN: (f64, f64) = (640.0, 420.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    width: f64,
    height: f64,
}

/// Validate and clamp a logical size.
///
/// Returns `None` for non-finite, non-positive or absurd inputs (e.g. the
/// `-32000` / `0x0` a minimized window reports). Otherwise raises to `min`
/// and then caps to `max` (work area, logical) — except that `min` always
/// wins, since the window can never be smaller than its configured minimum.
pub fn sanitize(
    width: f64,
    height: f64,
    min: (f64, f64),
    max: Option<(f64, f64)>,
) -> Option<WindowSize> {
    let ok = |v: f64| v.is_finite() && v > 0.0 && v <= ABSURD;
    if !ok(width) || !ok(height) {
        return None;
    }
    let mut w = width.max(min.0);
    let mut h = height.max(min.1);
    if let Some((mw, mh)) = max
        && mw.is_finite()
        && mh.is_finite()
        && mw > 0.0
        && mh > 0.0
    {
        w = w.min(mw).max(min.0);
        h = h.min(mh).max(min.1);
    }
    Some(WindowSize {
        width: w.round() as u32,
        height: h.round() as u32,
    })
}

/// Read the saved size. Every failure mode (missing, unreadable, oversized,
/// corrupt, unknown version) yields `None` so startup is never blocked.
pub fn load(path: &Path, min: (f64, f64), max: Option<(f64, f64)>) -> Option<WindowSize> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let stored: Stored = serde_json::from_slice(&bytes).ok()?;
    if stored.version != VERSION {
        return None;
    }
    sanitize(stored.width, stored.height, min, max)
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Sanitize and atomically write the size (unique temp file + rename).
/// Returns `Ok(false)` when the size was rejected and nothing was written.
pub fn save(
    path: &Path,
    width: f64,
    height: f64,
    min: (f64, f64),
    max: Option<(f64, f64)>,
) -> std::io::Result<bool> {
    let Some(size) = sanitize(width, height, min, max) else {
        return Ok(false);
    };
    let body = serde_json::to_vec(&Stored {
        version: VERSION,
        width: f64::from(size.width),
        height: f64::from(size.height),
    })
    .map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp: PathBuf = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: (f64, f64) = DEFAULT_MIN;
    const WA: Option<(f64, f64)> = Some((1920.0, 1040.0));

    fn sz(w: u32, h: u32) -> Option<WindowSize> {
        Some(WindowSize {
            width: w,
            height: h,
        })
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "kanade-ws-{tag}-{}-{}",
                std::process::id(),
                TMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn passes_through_normal_size() {
        assert_eq!(sanitize(880.0, 560.0, MIN, WA), sz(880, 560));
    }

    #[test]
    fn rejects_garbage() {
        for (w, h) in [
            (0.0, 0.0),
            (-32000.0, -32000.0),
            (880.0, -1.0),
            (f64::NAN, 500.0),
            (500.0, f64::INFINITY),
            (1.0e9, 500.0),
            (500.0, 65537.0),
        ] {
            assert_eq!(sanitize(w, h, MIN, WA), None, "{w}x{h}");
        }
    }

    #[test]
    fn raises_to_min() {
        assert_eq!(sanitize(100.0, 50.0, MIN, WA), sz(640, 420));
    }

    #[test]
    fn caps_to_work_area() {
        assert_eq!(sanitize(5000.0, 3000.0, MIN, WA), sz(1920, 1040));
    }

    #[test]
    fn no_work_area_applies_min_only() {
        assert_eq!(sanitize(5000.0, 100.0, MIN, None), sz(5000, 420));
    }

    #[test]
    fn min_wins_over_tiny_work_area() {
        assert_eq!(
            sanitize(800.0, 600.0, MIN, Some((500.0, 300.0))),
            sz(640, 420)
        );
    }

    #[test]
    fn invalid_work_area_is_ignored() {
        assert_eq!(
            sanitize(800.0, 600.0, MIN, Some((0.0, f64::NAN))),
            sz(800, 600)
        );
    }

    #[test]
    fn logical_value_is_stable_across_dpi() {
        // 1760x1120 physical @200% and 1320x840 @150% are both 880x560 logical.
        let a = sanitize(1760.0 / 2.0, 1120.0 / 2.0, MIN, WA);
        let b = sanitize(1320.0 / 1.5, 840.0 / 1.5, MIN, WA);
        assert_eq!(a, sz(880, 560));
        assert_eq!(a, b);
    }

    #[test]
    fn load_missing_file_is_none() {
        let d = TempDir::new("missing");
        assert_eq!(load(&d.0.join(FILE_NAME), MIN, WA), None);
    }

    #[test]
    fn load_empty_corrupt_or_unknown_version_is_none() {
        let d = TempDir::new("bad");
        let p = d.0.join(FILE_NAME);
        for body in [
            "",
            "not json",
            "{\"version\":1}",
            "{\"version\":99,\"width\":800,\"height\":600}",
            "{\"version\":1,\"width\":-32000,\"height\":0}",
        ] {
            std::fs::write(&p, body).unwrap();
            assert_eq!(load(&p, MIN, WA), None, "{body:?}");
        }
    }

    #[test]
    fn load_oversized_file_is_none() {
        let d = TempDir::new("big");
        let p = d.0.join(FILE_NAME);
        std::fs::write(&p, vec![b' '; 10_000]).unwrap();
        assert_eq!(load(&p, MIN, WA), None);
    }

    #[test]
    fn load_directory_is_none() {
        let d = TempDir::new("dir");
        assert_eq!(load(&d.0, MIN, WA), None);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let d = TempDir::new("rt");
        let p = d.0.join("nested").join(FILE_NAME);
        assert!(save(&p, 1000.0, 700.0, MIN, WA).unwrap());
        assert_eq!(load(&p, MIN, WA), sz(1000, 700));
        // No temp files left behind.
        let n = std::fs::read_dir(p.parent().unwrap()).unwrap().count();
        assert_eq!(n, 1);
    }

    #[test]
    fn save_rejects_invalid_without_writing() {
        let d = TempDir::new("rej");
        let p = d.0.join(FILE_NAME);
        assert!(!save(&p, -32000.0, -32000.0, MIN, WA).unwrap());
        assert!(!p.exists());
    }

    #[test]
    fn save_to_unwritable_location_errors_without_panic() {
        let d = TempDir::new("unw");
        // Parent "directory" is actually a file → create_dir_all fails.
        let blocker = d.0.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(save(&blocker.join(FILE_NAME), 800.0, 600.0, MIN, WA).is_err());
    }

    #[test]
    fn restore_clamps_to_current_work_area() {
        let d = TempDir::new("clamp");
        let p = d.0.join(FILE_NAME);
        save(&p, 1800.0, 1000.0, MIN, WA).unwrap();
        assert_eq!(load(&p, MIN, Some((1280.0, 720.0))), sz(1280, 720));
    }
}
