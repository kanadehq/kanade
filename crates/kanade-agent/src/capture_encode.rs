//! #1140 PR3a — turning a captured desktop frame into encoded tiles.
//!
//! Extracted from `capture_probe` so the probe and the in-session capture
//! child share one implementation. They ask different questions of it — the
//! probe compares strategies, the child ships bytes — but "crop this
//! rectangle out of a BGRA frame and JPEG it" must mean exactly one thing,
//! or the numbers #1142 measured stop describing what actually gets sent.
//!
//! # Why tiles at all
//!
//! #1142 measured mean changed area at 12.8% of the screen. Encoding only
//! the dirty rectangles cut bandwidth from 32 Mbps to 4.0 and lifted the
//! frame-rate ceiling from 5.8 to 24.2 fps. That is the measurement that
//! made this a tile encoder rather than a frame encoder.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use kanade_shared::wire::MAX_TILE_BYTES;

use crate::screen_capture::Frame;

/// Convert tightly-packed BGRA to tightly-packed RGB.
///
/// Duplication hands back BGRA; JPEG wants RGB, and dropping alpha shrinks
/// the buffer the encoder walks by a quarter. A trailing partial pixel
/// (only possible from a malformed buffer) is ignored rather than
/// panicking — a bad frame should be reported, not fatal.
pub fn bgra_to_rgb(bgra: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(bgra.len() / 4 * 3);
    for px in bgra.chunks_exact(4) {
        out.push(px[2]);
        out.push(px[1]);
        out.push(px[0]);
    }
}

/// Crop a rectangle out of a tightly-packed BGRA buffer into tightly-packed
/// RGB, replacing `out`.
///
/// Returns `false` — leaving `out` untouched — when the rectangle is empty
/// or falls outside the buffer, so a malformed rect degrades to "skip this
/// tile" instead of panicking mid-stream. Leaving `out` untouched matters
/// because callers reuse one scratch buffer across tiles: a half-filled
/// buffer would otherwise be encoded as if it were valid pixels.
#[allow(clippy::too_many_arguments)]
pub fn crop_bgra_to_rgb(
    bgra: &[u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    out: &mut Vec<u8>,
) -> bool {
    if w == 0 || h == 0 || x + w > width || y + h > height {
        return false;
    }
    let stride = width as usize * 4;
    if bgra.len() < stride * height as usize {
        return false;
    }

    out.clear();
    out.reserve(w as usize * h as usize * 3);
    for row in 0..h as usize {
        let start = (y as usize + row) * stride + x as usize * 4;
        let line = &bgra[start..start + w as usize * 4];
        for px in line.chunks_exact(4) {
            out.push(px[2]);
            out.push(px[1]);
            out.push(px[0]);
        }
    }
    true
}

/// JPEG-encode a tightly-packed RGB buffer.
pub fn encode_jpeg(rgb: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>> {
    use image::ExtendedColorType;
    use image::codecs::jpeg::JpegEncoder;

    let mut out = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut out, quality);
    enc.encode(rgb, width, height, ExtendedColorType::Rgb8)
        .context("jpeg encode")?;
    Ok(out)
}

/// One changed region, encoded and ready to ship.
#[derive(Debug, Clone)]
pub struct EncodedTile {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub jpeg: Vec<u8>,
}

/// Stop splitting once a band is this short.
///
/// A band this thin encoding above the budget would mean the budget is
/// wrong, not that the split failed — and halving forever would turn one
/// oversized tile into thousands of messages, which is worse than the
/// problem. The oversized tile is emitted instead and the caller decides.
const MIN_SPLIT_HEIGHT: u32 = 8;

/// Encode a frame's changed regions.
///
/// When DXGI reported no dirty rectangles we cannot tell what changed, so
/// the whole frame is encoded. That is the honest reading of an empty list:
/// it means "unknown", not "nothing changed" — treating it as nothing would
/// freeze the viewer's picture on exactly the frames where we have the
/// least information.
///
/// Any region encoding above [`MAX_TILE_BYTES`] is split into horizontal
/// bands until each fits. This is not theoretical: a real 3840x1600 capture
/// produced tiles averaging 345 KB and peaking at 578 KB against a 256 KB
/// budget, because a single dirty rectangle can cover most of the screen.
/// Without splitting those messages would simply be rejected by the broker
/// once PR3b starts publishing them.
///
/// A rectangle that fails to crop is skipped rather than aborting the
/// frame; the viewer keeps the stale pixels for that region, which is far
/// better than dropping the whole update.
pub fn encode_tiles(frame: &Frame, quality: u8, scratch: &mut Vec<u8>) -> Result<Vec<EncodedTile>> {
    let mut tiles = Vec::new();

    if frame.dirty_rects.is_empty() {
        push_split(
            frame,
            0,
            0,
            frame.width,
            frame.height,
            quality,
            scratch,
            &mut tiles,
        )?;
        return Ok(tiles);
    }

    for r in &frame.dirty_rects {
        let (x, y, w, h) = clamp_rect(r, frame.width, frame.height);
        push_split(frame, x, y, w, h, quality, scratch, &mut tiles)?;
    }
    Ok(tiles)
}

/// Clamp a dirty rectangle to the frame it came from.
///
/// **All four edges**, not just the origin. Clamping `left`/`top` to zero
/// while deriving the size from the *unclamped* values silently widens the
/// tile: a rect of `left = -10, right = 100` yields `x = 0, w = 110`, which
/// either grabs pixels the rect never claimed or — once `x + w` passes the
/// frame width — is rejected wholesale by [`crop_bgra_to_rgb`], dropping
/// that region's update for the frame.
///
/// DXGI is expected to hand back well-formed rectangles, so this most
/// likely never fires. It is worth being actually correct rather than
/// half-defensive now that these coordinates decide what gets shipped
/// rather than only feeding a benchmark statistic.
fn clamp_rect(
    r: &crate::screen_capture::DirtyRect,
    width: u32,
    height: u32,
) -> (u32, u32, u32, u32) {
    let max_x = width as i32;
    let max_y = height as i32;
    let left = r.left.clamp(0, max_x);
    let top = r.top.clamp(0, max_y);
    // Lower-bounded by the clamped origin, so an inverted rect collapses to
    // zero size instead of wrapping when the difference is cast to u32.
    let right = r.right.clamp(left, max_x);
    let bottom = r.bottom.clamp(top, max_y);
    (
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    )
}

/// Encode one region, halving it into horizontal bands while it exceeds the
/// wire budget.
///
/// Bands rather than quadrants: JPEG is encoded in rows, so a horizontal
/// split costs one crop per band and keeps each band's rows contiguous in
/// the source buffer. Splitting both axes would halve the byte count just
/// as well but doubles the crop work for no gain here.
#[allow(clippy::too_many_arguments)]
fn push_split(
    frame: &Frame,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    quality: u8,
    scratch: &mut Vec<u8>,
    out: &mut Vec<EncodedTile>,
) -> Result<()> {
    if !crop_bgra_to_rgb(&frame.bgra, frame.width, frame.height, x, y, w, h, scratch) {
        return Ok(());
    }
    let jpeg = encode_jpeg(scratch, w, h, quality)?;

    if jpeg.len() <= MAX_TILE_BYTES || h <= MIN_SPLIT_HEIGHT {
        out.push(EncodedTile { x, y, w, h, jpeg });
        return Ok(());
    }

    // Drop the oversized encode before recursing so the two halves don't
    // hold it alive alongside their own buffers.
    drop(jpeg);
    let top = h / 2;
    push_split(frame, x, y, w, top, quality, scratch, out)?;
    push_split(frame, x, y + top, w, h - top, quality, scratch, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen_capture::DirtyRect;

    /// 2x2 BGRA image, one distinct colour per pixel.
    fn tiny_bgra() -> Vec<u8> {
        vec![
            10, 20, 1, 255, // (0,0) → RGB 1,20,10
            11, 21, 2, 255, // (1,0) → RGB 2,21,11
            12, 22, 3, 255, // (0,1) → RGB 3,22,12
            13, 23, 4, 255, // (1,1) → RGB 4,23,13
        ]
    }

    fn frame(rects: Vec<DirtyRect>) -> Frame {
        Frame {
            width: 2,
            height: 2,
            bgra: tiny_bgra(),
            dirty_rects: rects,
            accumulated_frames: 1,
        }
    }

    #[test]
    fn bgra_to_rgb_reorders_channels_and_drops_alpha() {
        let mut out = Vec::new();
        bgra_to_rgb(&[1, 2, 3, 255], &mut out);
        assert_eq!(out, vec![3, 2, 1]);
    }

    #[test]
    fn bgra_to_rgb_reuses_the_buffer() {
        let mut out = Vec::new();
        bgra_to_rgb(&[1, 2, 3, 255, 10, 20, 30, 255], &mut out);
        bgra_to_rgb(&[9, 8, 7, 255], &mut out);
        assert_eq!(out, vec![7, 8, 9]);
    }

    #[test]
    fn bgra_to_rgb_ignores_trailing_partial_pixel() {
        let mut out = Vec::new();
        bgra_to_rgb(&[1, 2, 3, 255, 42, 42], &mut out);
        assert_eq!(out, vec![3, 2, 1]);
    }

    #[test]
    fn crop_walks_rows_with_the_right_stride() {
        let mut out = Vec::new();
        assert!(crop_bgra_to_rgb(&tiny_bgra(), 2, 2, 1, 0, 1, 2, &mut out));
        assert_eq!(out, vec![2, 21, 11, 4, 23, 13]);
    }

    #[test]
    fn crop_of_the_whole_frame_matches_the_full_conversion() {
        let bgra = tiny_bgra();
        let mut cropped = Vec::new();
        let mut full = Vec::new();
        assert!(crop_bgra_to_rgb(&bgra, 2, 2, 0, 0, 2, 2, &mut cropped));
        bgra_to_rgb(&bgra, &mut full);
        assert_eq!(cropped, full);
    }

    #[test]
    fn crop_rejects_out_of_bounds_and_empty_rects() {
        let mut out = Vec::new();
        assert!(!crop_bgra_to_rgb(&tiny_bgra(), 2, 2, 1, 1, 2, 2, &mut out));
        assert!(!crop_bgra_to_rgb(&tiny_bgra(), 2, 2, 0, 0, 0, 1, &mut out));
        assert!(!crop_bgra_to_rgb(&tiny_bgra(), 2, 2, 0, 0, 1, 0, &mut out));
    }

    #[test]
    fn crop_rejects_a_short_buffer() {
        let mut out = Vec::new();
        assert!(!crop_bgra_to_rgb(&[0u8; 4], 2, 2, 0, 0, 2, 2, &mut out));
    }

    #[test]
    fn crop_leaves_the_output_untouched_when_it_rejects() {
        let mut out = vec![9, 9, 9];
        assert!(!crop_bgra_to_rgb(&tiny_bgra(), 2, 2, 5, 5, 1, 1, &mut out));
        assert_eq!(out, vec![9, 9, 9]);
    }

    #[test]
    fn no_dirty_rects_encodes_one_full_frame_tile() {
        // "Unknown", not "nothing changed" — the viewer must get a picture.
        let mut scratch = Vec::new();
        let tiles = encode_tiles(&frame(Vec::new()), 75, &mut scratch).expect("encode");
        assert_eq!(tiles.len(), 1);
        assert_eq!(
            (tiles[0].x, tiles[0].y, tiles[0].w, tiles[0].h),
            (0, 0, 2, 2)
        );
        assert!(!tiles[0].jpeg.is_empty());
    }

    #[test]
    fn each_dirty_rect_becomes_a_tile() {
        let mut scratch = Vec::new();
        let tiles = encode_tiles(
            &frame(vec![
                DirtyRect {
                    left: 0,
                    top: 0,
                    right: 1,
                    bottom: 1,
                },
                DirtyRect {
                    left: 1,
                    top: 1,
                    right: 2,
                    bottom: 2,
                },
            ]),
            75,
            &mut scratch,
        )
        .expect("encode");
        assert_eq!(tiles.len(), 2);
        assert_eq!(
            (tiles[0].x, tiles[0].y, tiles[0].w, tiles[0].h),
            (0, 0, 1, 1)
        );
        assert_eq!(
            (tiles[1].x, tiles[1].y, tiles[1].w, tiles[1].h),
            (1, 1, 1, 1)
        );
    }

    #[test]
    fn a_rect_starting_left_of_the_frame_keeps_its_true_width() {
        // left = -1, right = 1 must give x=0, w=1 — not x=0, w=2, which
        // would claim a pixel the rect never covered.
        let (x, y, w, h) = clamp_rect(
            &DirtyRect {
                left: -1,
                top: 0,
                right: 1,
                bottom: 1,
            },
            2,
            2,
        );
        assert_eq!((x, y, w, h), (0, 0, 1, 1));
    }

    #[test]
    fn a_rect_extending_past_the_frame_is_clamped_not_dropped() {
        // Without clamping `right`, x + w exceeds the width and
        // crop_bgra_to_rgb rejects the whole tile — losing the update for a
        // region that was mostly on-screen.
        let (x, y, w, h) = clamp_rect(
            &DirtyRect {
                left: 1,
                top: 1,
                right: 99,
                bottom: 99,
            },
            2,
            2,
        );
        assert_eq!((x, y, w, h), (1, 1, 1, 1));
    }

    #[test]
    fn an_inverted_rect_collapses_to_zero_size() {
        // right < left must not wrap when the difference is cast to u32.
        let (_, _, w, h) = clamp_rect(
            &DirtyRect {
                left: 2,
                top: 2,
                right: 0,
                bottom: 0,
            },
            2,
            2,
        );
        assert_eq!((w, h), (0, 0));
    }

    #[test]
    fn a_rect_entirely_off_screen_collapses_to_zero_size() {
        let (_, _, w, h) = clamp_rect(
            &DirtyRect {
                left: -50,
                top: -50,
                right: -10,
                bottom: -10,
            },
            2,
            2,
        );
        assert_eq!((w, h), (0, 0));
    }

    #[test]
    fn an_out_of_bounds_rect_still_yields_its_on_screen_part() {
        // End to end: the clamped rect must survive encode_tiles rather
        // than being skipped by the crop bounds check.
        let mut scratch = Vec::new();
        let tiles = encode_tiles(
            &frame(vec![DirtyRect {
                left: -5,
                top: -5,
                right: 2,
                bottom: 2,
            }]),
            75,
            &mut scratch,
        )
        .expect("encode");
        assert_eq!(tiles.len(), 1);
        assert_eq!(
            (tiles[0].x, tiles[0].y, tiles[0].w, tiles[0].h),
            (0, 0, 2, 2)
        );
    }

    #[test]
    fn an_unusable_rect_is_skipped_not_fatal() {
        // One bad rectangle must not cost the viewer the whole update.
        let mut scratch = Vec::new();
        let tiles = encode_tiles(
            &frame(vec![
                DirtyRect {
                    left: 0,
                    top: 0,
                    right: 1,
                    bottom: 1,
                },
                DirtyRect {
                    left: 5,
                    top: 5,
                    right: 9,
                    bottom: 9,
                }, // outside
            ]),
            75,
            &mut scratch,
        )
        .expect("encode");
        assert_eq!(tiles.len(), 1);
    }

    /// A frame whose pixels are noisy enough that JPEG cannot compress them
    /// far — the only way to exceed the budget in a test without allocating
    /// something enormous.
    fn noisy_frame(width: u32, height: u32) -> Frame {
        let mut bgra = Vec::with_capacity((width * height * 4) as usize);
        // A cheap deterministic PRNG: random-looking pixels defeat the DCT,
        // so the encode stays large. A gradient would compress to nothing.
        let mut state: u32 = 0x1234_5678;
        for _ in 0..(width * height) {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let b = state.to_le_bytes();
            bgra.extend_from_slice(&[b[0], b[1], b[2], 255]);
        }
        Frame {
            width,
            height,
            bgra,
            dirty_rects: Vec::new(),
            accumulated_frames: 1,
        }
    }

    #[test]
    fn an_oversized_region_is_split_until_each_tile_fits() {
        // The case a real 3840x1600 capture hit: one dirty rect covering
        // most of the screen encoded to 578 KB against a 256 KB budget.
        let f = noisy_frame(1200, 1200);
        let mut scratch = Vec::new();
        let tiles = encode_tiles(&f, 95, &mut scratch).expect("encode");
        assert!(
            tiles.len() > 1,
            "expected a split, got {} tile(s)",
            tiles.len()
        );
        for t in &tiles {
            assert!(
                t.jpeg.len() <= MAX_TILE_BYTES,
                "tile {}x{} at ({},{}) is {} bytes",
                t.w,
                t.h,
                t.x,
                t.y,
                t.jpeg.len()
            );
        }
    }

    #[test]
    fn split_bands_tile_the_original_region_exactly() {
        // No gaps and no overlap: a viewer painting these must reconstruct
        // the region, not a striped approximation of it.
        let f = noisy_frame(1200, 1200);
        let mut scratch = Vec::new();
        let mut tiles = encode_tiles(&f, 95, &mut scratch).expect("encode");
        tiles.sort_by_key(|t| t.y);

        assert_eq!(tiles[0].y, 0);
        for pair in tiles.windows(2) {
            assert_eq!(
                pair[0].y + pair[0].h,
                pair[1].y,
                "band at y={} does not meet the next at y={}",
                pair[0].y,
                pair[1].y
            );
        }
        let last = tiles.last().unwrap();
        assert_eq!(last.y + last.h, 1200);
        // Width is untouched by a horizontal split.
        for t in &tiles {
            assert_eq!((t.x, t.w), (0, 1200));
        }
    }

    #[test]
    fn a_region_that_already_fits_is_not_split() {
        let mut scratch = Vec::new();
        let tiles = encode_tiles(&frame(Vec::new()), 75, &mut scratch).expect("encode");
        assert_eq!(tiles.len(), 1);
    }

    #[test]
    fn encoded_tiles_are_real_jpegs() {
        // SOI marker — cheap proof we produced an image rather than bytes.
        let mut scratch = Vec::new();
        let tiles = encode_tiles(&frame(Vec::new()), 75, &mut scratch).expect("encode");
        assert_eq!(&tiles[0].jpeg[..2], &[0xFF, 0xD8]);
    }
}
