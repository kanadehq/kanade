//! #1140 PR2 — measurement driver for the screen-capture path.
//!
//! Answers the questions the remote-assistance design rests on, with
//! numbers from a real desktop instead of estimates:
//!
//! 1. Does Desktop Duplication work at all on this hardware?
//! 2. What frame rate can we actually sustain, and where does the time go
//!    (GPU→CPU copy vs. pixel conversion vs. JPEG encode)?
//! 3. How big is an encoded frame — i.e. what bandwidth would a relay need?
//! 4. **How much of the screen actually changes per frame?** This is the
//!    one that decides whether a dirty-rect encoder is worth building or
//!    whether full frames are fine.
//!
//! Driven by the hidden `--capture-probe` flag. It is a diagnostic, not a
//! product surface: it prints a human-readable summary plus one JSON line
//! so results can be pasted into an issue or diffed across machines.
//!
//! # Why the whole module is Windows-only
//!
//! The pixel-conversion and statistics helpers are plain platform-neutral
//! Rust, and the first cut deliberately left them outside the `cfg` gate so
//! their tests would run on every CI platform. That does not work: the only
//! caller is the `cfg(windows)` runner, so on Linux/macOS every helper is
//! genuinely unreachable and `dead_code` (correctly) fails the build.
//!
//! Rather than `allow(dead_code)` over the module — which would silence the
//! lint on real dead code too — this follows the precedent `klp` already
//! sets: gate at the module declaration and accept that the unit tests run
//! on Windows only. The production target is Windows-only, so coverage
//! stays meaningful; what is lost is catching a regression from a
//! Linux/macOS dev box, which is not how this code is worked on.

#![cfg(target_os = "windows")]

use std::time::Duration;

use crate::capture_encode::{bgra_to_rgb, crop_bgra_to_rgb, encode_jpeg};

/// Percentile over a sorted-on-demand sample set.
///
/// Nearest-rank: index `ceil(p/100 * n) - 1`, clamped. Averages hide
/// exactly the stalls that make remote control feel bad, so the summary
/// reports p50/p95 alongside the mean.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// One frame's timings and sizes.
#[derive(Debug, Clone, Copy)]
pub struct FrameSample {
    /// Acquire + GPU→CPU copy + row-pitch unpack.
    pub capture_ms: f64,
    /// BGRA→RGB conversion.
    pub convert_ms: f64,
    /// JPEG encode.
    pub encode_ms: f64,
    /// Encoded JPEG size in bytes, whole frame.
    pub jpeg_bytes: usize,
    /// Same frame encoded as dirty tiles only — the sum of every changed
    /// rectangle's JPEG. This is the number that decides whether a
    /// dirty-rect encoder is worth building.
    pub tile_bytes: usize,
    /// Time to encode all those tiles.
    pub tile_encode_ms: f64,
    /// How many tiles that took.
    pub tile_count: usize,
    /// Changed pixels (upper bound; overlapping rects double-count).
    pub dirty_px: u64,
    /// Full-frame pixel count.
    pub total_px: u64,
    /// Desktop updates the compositor coalesced into this frame.
    pub accumulated: u32,
}

impl FrameSample {
    /// End-to-end cost of turning one desktop update into bytes on the
    /// wire. This is what bounds the achievable frame rate.
    pub fn total_ms(&self) -> f64 {
        self.capture_ms + self.convert_ms + self.encode_ms
    }

    /// Fraction of the screen that changed, 0.0–1.0 (clamped, since
    /// overlapping dirty rects can sum past the full frame).
    pub fn dirty_ratio(&self) -> f64 {
        if self.total_px == 0 {
            return 0.0;
        }
        (self.dirty_px as f64 / self.total_px as f64).min(1.0)
    }
}

/// Accumulated run statistics.
#[derive(Debug, Default)]
pub struct ProbeStats {
    pub samples: Vec<FrameSample>,
    /// Polls that returned no new frame (idle desktop or pointer-only
    /// update). A high count against a low frame count just means the
    /// screen was static.
    pub idle_polls: u64,
    /// Times the desktop became unavailable (lock screen, secure desktop,
    /// mode change). **Non-zero is a finding, not a failure** — it is the
    /// capture boundary the design has to live with.
    pub unavailable: u64,
    /// Reasons behind `unavailable`, deduplicated for the summary.
    pub unavailable_reasons: Vec<String>,
}

impl ProbeStats {
    pub fn record(&mut self, s: FrameSample) {
        self.samples.push(s);
    }

    pub fn record_idle(&mut self) {
        self.idle_polls += 1;
    }

    pub fn record_unavailable(&mut self, reason: String) {
        self.unavailable += 1;
        if !self.unavailable_reasons.contains(&reason) {
            self.unavailable_reasons.push(reason);
        }
    }

    /// Build the summary. `elapsed` is wall-clock run duration, which is
    /// what the frame rate must be computed against — dividing by the sum
    /// of per-frame times would report the rate of a busy-loop that never
    /// waited for the desktop, flattering the result.
    pub fn summarize(&self, elapsed: Duration) -> ProbeSummary {
        let n = self.samples.len();
        let secs = elapsed.as_secs_f64().max(f64::EPSILON);

        let mut totals: Vec<f64> = self.samples.iter().map(FrameSample::total_ms).collect();
        let mut captures: Vec<f64> = self.samples.iter().map(|s| s.capture_ms).collect();
        let mut converts: Vec<f64> = self.samples.iter().map(|s| s.convert_ms).collect();
        let mut encodes: Vec<f64> = self.samples.iter().map(|s| s.encode_ms).collect();
        let mut sizes: Vec<f64> = self.samples.iter().map(|s| s.jpeg_bytes as f64).collect();
        for v in [
            &mut totals,
            &mut captures,
            &mut converts,
            &mut encodes,
            &mut sizes,
        ] {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }

        let mean = |v: &[f64]| {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };

        let mut tile_sizes: Vec<f64> = self.samples.iter().map(|s| s.tile_bytes as f64).collect();
        let mut tile_encodes: Vec<f64> = self.samples.iter().map(|s| s.tile_encode_ms).collect();
        for v in [&mut tile_sizes, &mut tile_encodes] {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }

        let total_bytes: u64 = self.samples.iter().map(|s| s.jpeg_bytes as u64).sum();
        let total_tile_bytes: u64 = self.samples.iter().map(|s| s.tile_bytes as u64).sum();
        let dirty_mean = if n == 0 {
            0.0
        } else {
            self.samples
                .iter()
                .map(FrameSample::dirty_ratio)
                .sum::<f64>()
                / n as f64
        };
        let coalesced = self.samples.iter().filter(|s| s.accumulated > 1).count();

        ProbeSummary {
            frames: n,
            elapsed_secs: secs,
            fps: n as f64 / secs,
            idle_polls: self.idle_polls,
            unavailable: self.unavailable,
            unavailable_reasons: self.unavailable_reasons.clone(),
            capture_ms_mean: mean(&captures),
            convert_ms_mean: mean(&converts),
            encode_ms_mean: mean(&encodes),
            total_ms_mean: mean(&totals),
            total_ms_p50: percentile(&totals, 50.0),
            total_ms_p95: percentile(&totals, 95.0),
            jpeg_bytes_mean: mean(&sizes),
            jpeg_bytes_p95: percentile(&sizes, 95.0),
            // Bits per second the relay would have pushed to sustain what
            // we just measured.
            mbps: (total_bytes as f64 * 8.0) / secs / 1_000_000.0,
            dirty_ratio_mean: dirty_mean,
            coalesced_frames: coalesced,
            tile_bytes_mean: mean(&tile_sizes),
            tile_encode_ms_mean: mean(&tile_encodes),
            tile_mbps: (total_tile_bytes as f64 * 8.0) / secs / 1_000_000.0,
            tile_count_mean: if n == 0 {
                0.0
            } else {
                self.samples
                    .iter()
                    .map(|s| s.tile_count as f64)
                    .sum::<f64>()
                    / n as f64
            },
        }
    }
}

/// Human- and machine-readable run summary.
#[derive(Debug, Clone)]
pub struct ProbeSummary {
    pub frames: usize,
    pub elapsed_secs: f64,
    pub fps: f64,
    pub idle_polls: u64,
    pub unavailable: u64,
    pub unavailable_reasons: Vec<String>,
    pub capture_ms_mean: f64,
    pub convert_ms_mean: f64,
    pub encode_ms_mean: f64,
    pub total_ms_mean: f64,
    pub total_ms_p50: f64,
    pub total_ms_p95: f64,
    pub jpeg_bytes_mean: f64,
    pub jpeg_bytes_p95: f64,
    pub mbps: f64,
    pub dirty_ratio_mean: f64,
    pub coalesced_frames: usize,
    pub tile_bytes_mean: f64,
    pub tile_encode_ms_mean: f64,
    pub tile_mbps: f64,
    pub tile_count_mean: f64,
}

impl ProbeSummary {
    /// Frame-rate ceiling if only the changed tiles were encoded — capture
    /// and convert costs stay, the whole-frame encode is replaced by the
    /// tile encode.
    pub fn tile_bound_fps(&self) -> f64 {
        let per_frame = self.capture_ms_mean + self.convert_ms_mean + self.tile_encode_ms_mean;
        if per_frame <= 0.0 {
            0.0
        } else {
            1000.0 / per_frame
        }
    }

    /// How much bandwidth dirty-rect encoding saves, 0.0–1.0. Negative
    /// savings are possible in principle (many tiny tiles, each paying a
    /// JPEG header), which is exactly why this is measured rather than
    /// assumed — so the value is deliberately not clamped.
    pub fn tile_saving(&self) -> f64 {
        if self.jpeg_bytes_mean <= 0.0 {
            return 0.0;
        }
        1.0 - (self.tile_bytes_mean / self.jpeg_bytes_mean)
    }

    /// Theoretical ceiling from per-frame cost alone, ignoring how often
    /// the desktop actually updates. Compared against the measured `fps`
    /// it separates "our pipeline is too slow" from "the screen was idle".
    pub fn cpu_bound_fps(&self) -> f64 {
        if self.total_ms_mean <= 0.0 {
            0.0
        } else {
            1000.0 / self.total_ms_mean
        }
    }

    /// One JSON line, so runs from different machines can be diffed or
    /// pasted into an issue without re-parsing the pretty output.
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"frames":{},"elapsed_secs":{:.3},"fps":{:.2},"cpu_bound_fps":{:.2},"idle_polls":{},"unavailable":{},"capture_ms_mean":{:.2},"convert_ms_mean":{:.2},"encode_ms_mean":{:.2},"total_ms_mean":{:.2},"total_ms_p50":{:.2},"total_ms_p95":{:.2},"jpeg_bytes_mean":{:.0},"jpeg_bytes_p95":{:.0},"mbps":{:.2},"dirty_ratio_mean":{:.4},"coalesced_frames":{},"tile_bytes_mean":{:.0},"tile_encode_ms_mean":{:.2},"tile_mbps":{:.2},"tile_count_mean":{:.1},"tile_bound_fps":{:.2},"tile_saving":{:.4}}}"#,
            self.frames,
            self.elapsed_secs,
            self.fps,
            self.cpu_bound_fps(),
            self.idle_polls,
            self.unavailable,
            self.capture_ms_mean,
            self.convert_ms_mean,
            self.encode_ms_mean,
            self.total_ms_mean,
            self.total_ms_p50,
            self.total_ms_p95,
            self.jpeg_bytes_mean,
            self.jpeg_bytes_p95,
            self.mbps,
            self.dirty_ratio_mean,
            self.coalesced_frames,
            self.tile_bytes_mean,
            self.tile_encode_ms_mean,
            self.tile_mbps,
            self.tile_count_mean,
            self.tile_bound_fps(),
            self.tile_saving(),
        )
    }
}

/// The probe runner — everything above feeds this.
pub fn run(secs: u64, quality: u8, save: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    use std::io::Write;
    use std::time::Instant;

    use anyhow::Context;

    use crate::screen_capture::{Capture, CaptureSession};

    println!("kanade capture probe — {secs}s, JPEG quality {quality}");
    println!("(#1140 PR2: measuring DXGI Desktop Duplication on this host)\n");

    let mut session = CaptureSession::new(0).context(
        "could not attach to display output 0 — note this must run in the interactive \
         desktop session, not as a Session 0 service",
    )?;

    let mut stats = ProbeStats::default();
    let mut rgb = Vec::new();
    let mut tile_rgb = Vec::new();
    let mut saved = save.is_none();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let started = Instant::now();

    while Instant::now() < deadline {
        let t0 = Instant::now();
        // 100 ms keeps the loop responsive to the deadline on a fully idle
        // desktop without spinning.
        match session.next_frame(100)? {
            Capture::Idle => {
                stats.record_idle();
                continue;
            }
            Capture::Unavailable(reason) => {
                stats.record_unavailable(reason);
                continue;
            }
            Capture::Frame(frame) => {
                let capture_ms = t0.elapsed().as_secs_f64() * 1000.0;

                let t1 = Instant::now();
                bgra_to_rgb(&frame.bgra, &mut rgb);
                let convert_ms = t1.elapsed().as_secs_f64() * 1000.0;

                let t2 = Instant::now();
                let jpeg = encode_jpeg(&rgb, frame.width, frame.height, quality)?;
                let encode_ms = t2.elapsed().as_secs_f64() * 1000.0;

                // Encode the same frame again as changed tiles only, so the
                // two strategies are measured on identical pixels rather
                // than on separate runs with different desktop activity.
                let t3 = Instant::now();
                let (tile_bytes, tile_count) = encode_dirty_tiles(&frame, quality, &mut tile_rgb)?;
                let tile_encode_ms = t3.elapsed().as_secs_f64() * 1000.0;

                if !saved {
                    if let Some(path) = save.as_ref() {
                        std::fs::write(path, &jpeg)
                            .with_context(|| format!("write probe frame to {}", path.display()))?;
                        println!("wrote first frame to {}\n", path.display());
                        saved = true;
                    }
                }

                stats.record(FrameSample {
                    capture_ms,
                    convert_ms,
                    encode_ms,
                    jpeg_bytes: jpeg.len(),
                    tile_bytes,
                    tile_encode_ms,
                    tile_count,
                    dirty_px: frame.dirty_area_px(),
                    total_px: frame.total_px(),
                    accumulated: frame.accumulated_frames,
                });
            }
        }
    }

    let (w, h) = session.dimensions();
    let s = stats.summarize(started.elapsed());

    println!("resolution        {w}x{h}");
    println!("frames captured   {} in {:.1}s", s.frames, s.elapsed_secs);
    println!(
        "measured fps      {:.1}   (pipeline ceiling {:.1} fps)",
        s.fps,
        s.cpu_bound_fps()
    );
    println!(
        "idle polls        {}   (desktop had nothing new)",
        s.idle_polls
    );
    println!(
        "per frame (mean)  capture {:.1}ms | convert {:.1}ms | encode {:.1}ms | total {:.1}ms",
        s.capture_ms_mean, s.convert_ms_mean, s.encode_ms_mean, s.total_ms_mean
    );
    println!(
        "per frame (p50/p95) {:.1}ms / {:.1}ms",
        s.total_ms_p50, s.total_ms_p95
    );
    println!(
        "jpeg size         mean {:.0} KB | p95 {:.0} KB",
        s.jpeg_bytes_mean / 1024.0,
        s.jpeg_bytes_p95 / 1024.0
    );
    println!("bandwidth         {:.2} Mbps at the measured rate", s.mbps);
    println!(
        "changed area      {:.1}% of the screen per frame (mean)",
        s.dirty_ratio_mean * 100.0
    );
    println!("\n-- dirty-rect encoding, same frames --");
    println!(
        "tiles per frame   {:.1}   encode {:.1}ms (vs {:.1}ms full frame)",
        s.tile_count_mean, s.tile_encode_ms_mean, s.encode_ms_mean
    );
    println!(
        "tile size         mean {:.0} KB   ({:.0}% smaller than a full frame)",
        s.tile_bytes_mean / 1024.0,
        s.tile_saving() * 100.0
    );
    println!(
        "bandwidth         {:.2} Mbps   (vs {:.2} Mbps full frame)",
        s.tile_mbps, s.mbps
    );
    println!(
        "pipeline ceiling  {:.1} fps   (vs {:.1} fps full frame)",
        s.tile_bound_fps(),
        s.cpu_bound_fps()
    );
    println!(
        "coalesced frames  {} (compositor merged >1 update)",
        s.coalesced_frames
    );

    if s.unavailable > 0 {
        println!("\ndesktop unavailable {} time(s):", s.unavailable);
        for r in &s.unavailable_reasons {
            println!("  - {r}");
        }
        println!(
            "  (expected at the lock screen / UAC secure desktop — capture cannot\n   \
             cross that boundary; see #1140 risk 4)"
        );
    }

    if s.frames == 0 {
        println!(
            "\nNo frames captured. If the desktop was genuinely idle this is normal —\n\
             move a window and re-run."
        );
    }

    println!("\nJSON {}", s.to_json());
    std::io::stdout().flush().ok();
    Ok(())
}

/// Encode only the changed rectangles, returning `(total_bytes, tiles)`.
///
/// When DXGI gave us no dirty rects we cannot tell what changed, so a real
/// encoder would have to send the whole frame — and that is what gets
/// measured here. Counting those frames as "0 bytes of tiles" would make
/// dirty-rect encoding look free precisely on the frames where it does not
/// help, which is the one way this comparison could lie.
///
/// A tile that fails to crop (a rect outside the frame) is skipped rather
/// than aborting the run; it shows up as a lower tile count.
fn encode_dirty_tiles(
    frame: &crate::screen_capture::Frame,
    quality: u8,
    scratch: &mut Vec<u8>,
) -> anyhow::Result<(usize, usize)> {
    if frame.dirty_rects.is_empty() {
        bgra_to_rgb(&frame.bgra, scratch);
        let jpeg = encode_jpeg(scratch, frame.width, frame.height, quality)?;
        return Ok((jpeg.len(), 1));
    }

    let mut total = 0usize;
    let mut tiles = 0usize;
    for r in &frame.dirty_rects {
        let x = r.left.max(0) as u32;
        let y = r.top.max(0) as u32;
        let w = (r.right - r.left).max(0) as u32;
        let h = (r.bottom - r.top).max(0) as u32;
        if !crop_bgra_to_rgb(&frame.bgra, frame.width, frame.height, x, y, w, h, scratch) {
            continue;
        }
        total += encode_jpeg(scratch, w, h, quality)?.len();
        tiles += 1;
    }
    Ok((total, tiles))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_saving_is_the_fraction_of_bytes_avoided() {
        let mut st = ProbeStats::default();
        // sample() sets tile_bytes to a quarter of jpeg_bytes → 75% saved.
        st.record(sample(1000, 100, 4000));
        let s = st.summarize(Duration::from_secs(1));
        assert!((s.tile_saving() - 0.75).abs() < 1e-9, "{}", s.tile_saving());
    }

    #[test]
    fn tile_saving_of_empty_run_is_zero() {
        let st = ProbeStats::default();
        let s = st.summarize(Duration::from_secs(1));
        assert_eq!(s.tile_saving(), 0.0);
    }

    #[test]
    fn tile_saving_goes_negative_when_tiles_cost_more() {
        // Many small tiles each paying a JPEG header can exceed the full
        // frame. The metric must show that rather than clamp it away.
        let mut st = ProbeStats::default();
        let mut s0 = sample(1000, 100, 1000);
        s0.tile_bytes = 1500;
        st.record(s0);
        let s = st.summarize(Duration::from_secs(1));
        assert!(s.tile_saving() < 0.0, "{}", s.tile_saving());
    }

    #[test]
    fn tile_bound_fps_uses_tile_encode_not_full_encode() {
        let mut st = ProbeStats::default();
        st.record(sample(1000, 100, 4000));
        let s = st.summarize(Duration::from_secs(1));
        // capture 1 + convert 2 + tile_encode 1 = 4ms → 250 fps
        assert!(
            (s.tile_bound_fps() - 250.0).abs() < 1e-9,
            "{}",
            s.tile_bound_fps()
        );
        // ...against 1+2+3 = 6ms for the full frame.
        assert!(s.tile_bound_fps() > s.cpu_bound_fps());
    }

    #[test]
    fn percentile_picks_nearest_rank() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&v, 50.0), 3.0);
        assert_eq!(percentile(&v, 100.0), 5.0);
        // ceil(0.2*5) - 1 = 0
        assert_eq!(percentile(&v, 20.0), 1.0);
    }

    #[test]
    fn percentile_of_empty_is_zero() {
        assert_eq!(percentile(&[], 95.0), 0.0);
    }

    #[test]
    fn percentile_never_indexes_past_the_end() {
        // p95 of a 2-element sample must not panic.
        let v = vec![1.0, 2.0];
        assert_eq!(percentile(&v, 95.0), 2.0);
    }

    fn sample(total_px: u64, dirty_px: u64, jpeg_bytes: usize) -> FrameSample {
        FrameSample {
            capture_ms: 1.0,
            convert_ms: 2.0,
            encode_ms: 3.0,
            jpeg_bytes,
            tile_bytes: jpeg_bytes / 4,
            tile_encode_ms: 1.0,
            tile_count: 2,
            dirty_px,
            total_px,
            accumulated: 1,
        }
    }

    #[test]
    fn frame_sample_total_is_the_sum_of_stages() {
        assert_eq!(sample(100, 10, 1000).total_ms(), 6.0);
    }

    #[test]
    fn dirty_ratio_is_clamped_to_one() {
        // Overlapping dirty rects can sum past the full frame; a ratio
        // above 1.0 would make the summary nonsense.
        assert_eq!(sample(100, 250, 0).dirty_ratio(), 1.0);
    }

    #[test]
    fn dirty_ratio_of_zero_area_frame_is_zero() {
        assert_eq!(sample(0, 0, 0).dirty_ratio(), 0.0);
    }

    #[test]
    fn summarize_computes_fps_against_wall_clock() {
        let mut st = ProbeStats::default();
        for _ in 0..10 {
            st.record(sample(1000, 100, 2048));
        }
        let s = st.summarize(Duration::from_secs(2));
        assert_eq!(s.frames, 10);
        assert!((s.fps - 5.0).abs() < 1e-9, "fps was {}", s.fps);
    }

    #[test]
    fn summarize_computes_bandwidth_from_total_bytes() {
        let mut st = ProbeStats::default();
        // 10 frames x 1000 bytes over 1s = 80_000 bits/s = 0.08 Mbps.
        for _ in 0..10 {
            st.record(sample(1000, 0, 1000));
        }
        let s = st.summarize(Duration::from_secs(1));
        assert!((s.mbps - 0.08).abs() < 1e-9, "mbps was {}", s.mbps);
    }

    #[test]
    fn summarize_of_empty_run_does_not_divide_by_zero() {
        let st = ProbeStats::default();
        let s = st.summarize(Duration::from_secs(5));
        assert_eq!(s.frames, 0);
        assert_eq!(s.fps, 0.0);
        assert_eq!(s.mbps, 0.0);
        assert_eq!(s.cpu_bound_fps(), 0.0);
    }

    #[test]
    fn summarize_survives_zero_elapsed() {
        // A run that ends instantly must not produce inf/NaN.
        let mut st = ProbeStats::default();
        st.record(sample(1000, 100, 100));
        let s = st.summarize(Duration::ZERO);
        assert!(s.fps.is_finite(), "fps was {}", s.fps);
        assert!(s.mbps.is_finite(), "mbps was {}", s.mbps);
    }

    #[test]
    fn cpu_bound_fps_inverts_mean_frame_cost() {
        let mut st = ProbeStats::default();
        // Each sample costs 6 ms total → ceiling of ~166.7 fps.
        st.record(sample(1000, 0, 100));
        let s = st.summarize(Duration::from_secs(1));
        assert!(
            (s.cpu_bound_fps() - 1000.0 / 6.0).abs() < 1e-9,
            "ceiling was {}",
            s.cpu_bound_fps()
        );
    }

    #[test]
    fn unavailable_reasons_are_deduplicated() {
        let mut st = ProbeStats::default();
        st.record_unavailable("lock screen".into());
        st.record_unavailable("lock screen".into());
        st.record_unavailable("mode change".into());
        let s = st.summarize(Duration::from_secs(1));
        assert_eq!(s.unavailable, 3, "every occurrence must still be counted");
        assert_eq!(s.unavailable_reasons.len(), 2, "but reasons are deduped");
    }

    #[test]
    fn coalesced_frames_counts_only_multi_update_frames() {
        let mut st = ProbeStats::default();
        st.record(sample(1000, 0, 100));
        let mut multi = sample(1000, 0, 100);
        multi.accumulated = 3;
        st.record(multi);
        let s = st.summarize(Duration::from_secs(1));
        assert_eq!(s.coalesced_frames, 1);
    }

    #[test]
    fn json_line_is_single_line_and_has_key_metrics() {
        let mut st = ProbeStats::default();
        st.record(sample(1000, 100, 2048));
        let json = st.summarize(Duration::from_secs(1)).to_json();
        assert!(!json.contains('\n'), "must stay a single line");
        for key in ["\"fps\"", "\"mbps\"", "\"dirty_ratio_mean\"", "\"frames\""] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
    }
}
