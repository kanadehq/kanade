//! Owned, tightly packed capture pixels shared by platform backends.

/// Bytes per pixel in the `DXGI_FORMAT_B8G8R8A8_UNORM` frames the
/// duplication hands back. Named so the pitch arithmetic below reads as
/// intent rather than as a bare `4`.
pub const BYTES_PER_PIXEL: usize = 4;

/// A rectangle of the desktop that changed since the previous frame.
///
/// Mirrors Win32 `RECT` (right/bottom exclusive) but kept as our own type
/// so callers — and, later, the wire format — never need the `windows`
/// crate in their signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl DirtyRect {
    /// Pixel area, saturating at zero. DXGI is expected to hand back
    /// well-formed rectangles; clamping rather than trusting the
    /// subtraction keeps a degenerate one from wrapping the `u64` area
    /// accumulator into a nonsense number.
    #[cfg(target_os = "windows")]
    pub fn area_px(&self) -> u64 {
        let w = (self.right - self.left).max(0) as u64;
        let h = (self.bottom - self.top).max(0) as u64;
        w * h
    }
}

/// One captured desktop frame, already copied out of GPU memory.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed BGRA, `width * height * BYTES_PER_PIXEL` bytes. The
    /// GPU's row pitch (which is padded, and routinely wider than
    /// `width * 4`) has already been removed, so consumers can treat this
    /// as a plain image buffer.
    pub bgra: Vec<u8>,
    /// Regions that changed since the previous frame. **Empty means "we
    /// could not tell"**, not "nothing changed" — the encoder sends the full
    /// frame in that case.
    pub dirty_rects: Vec<DirtyRect>,
    /// How many desktop updates the compositor coalesced into this one
    /// frame. A value above 1 means we are sampling slower than the
    /// desktop is changing.
    #[cfg(target_os = "windows")]
    pub accumulated_frames: u32,
}

#[cfg(target_os = "windows")]
impl Frame {
    /// Total changed area. Note this **double-counts overlapping
    /// rectangles**: it is a cheap upper bound for bandwidth estimation,
    /// not an exact changed-pixel count. Computing the true union needs a
    /// sweep-line over the rects, which is not worth it for a metric whose
    /// only job is to answer "is a dirty-rect encoder worth building".
    pub fn dirty_area_px(&self) -> u64 {
        self.dirty_rects.iter().map(DirtyRect::area_px).sum()
    }

    /// Full-frame pixel count — the denominator for "what fraction of the
    /// screen changed".
    pub fn total_px(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}
