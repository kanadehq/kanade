//! #1140 PR2 — DXGI Desktop Duplication screen capture.
//!
//! This is the endpoint half of the remote-assistance relay: it turns the
//! contents of the interactive desktop into BGRA frames that a later PR
//! encodes and pushes over `remote.frame.<sid>`. Nothing here touches NATS
//! or the network — the module is deliberately a pure "give me the next
//! frame" source so it can be driven by the probe (`--capture-probe`)
//! today and by the relay session tomorrow.
//!
//! # Why Desktop Duplication rather than BitBlt
//!
//! `BitBlt` off the screen DC copies the *whole* framebuffer every call, on
//! the CPU, with no notion of what changed. Desktop Duplication is the API
//! Windows itself exposes for this: the compositor hands us a texture that
//! is already in video memory, tells us how many frames it coalesced, and —
//! critically for a bandwidth-bound relay — hands back the **dirty
//! rectangles**. A mostly-static desktop then costs almost nothing to send,
//! which is the entire economic argument for the relay design.
//!
//! # Session affinity (the constraint that shapes everything)
//!
//! Duplication runs against the *input desktop of the calling process's
//! session*. A Session 0 service cannot capture a user's screen at all, so
//! this code only ever runs in the resident child process that
//! `session_supervisor` already keeps alive inside the logged-in user's
//! console session via the `RunAs::User` token dance in `process_as_user`.
//!
//! The same boundary is why `DXGI_ERROR_ACCESS_LOST` is a normal, expected
//! outcome rather than a failure: it fires whenever the desktop we are
//! attached to stops being the input desktop — the lock screen, a UAC
//! secure-desktop prompt, a resolution change, a GPU mode switch, fast user
//! switching. The caller cannot capture *through* any of those (that is an
//! OS security boundary, not something to work around), so the contract is
//! to report the gap upward and keep trying, letting the UI say "the screen
//! is unavailable" honestly. [`CaptureSession::next_frame`] surfaces this as
//! [`Capture::Unavailable`] and re-creates the duplication on a later call,
//! once the desktop is actually back — never as an error.
//!
//! # Every `unsafe` in the capture path lives here
//!
//! Same discipline as `process_as_user`: the COM/D3D dance is concentrated
//! in this module so callers stay plain safe Rust.

#![cfg(target_os = "windows")]

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter,
    IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

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
    pub fn area_px(&self) -> u64 {
        let w = (self.right - self.left).max(0) as u64;
        let h = (self.bottom - self.top).max(0) as u64;
        w * h
    }
}

impl From<RECT> for DirtyRect {
    fn from(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
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
    /// could not tell"**, not "nothing changed" — see
    /// [`Frame::dirty_area_px`] for why that distinction matters.
    pub dirty_rects: Vec<DirtyRect>,
    /// How many desktop updates the compositor coalesced into this one
    /// frame. A value above 1 means we are sampling slower than the
    /// desktop is changing.
    pub accumulated_frames: u32,
}

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

/// Outcome of asking for the next frame.
///
/// Three states rather than `Option<Frame>` because "nothing changed" and
/// "we are locked out of the desktop" are genuinely different events for
/// the caller: the first is the steady state of an idle screen, the second
/// must reach the operator's UI as "screen unavailable".
#[derive(Debug)]
pub enum Capture {
    /// A new frame is available.
    Frame(Box<Frame>),
    /// The timeout elapsed with no desktop update, or the only change was
    /// the mouse pointer moving. Either way there is nothing new to send.
    Idle,
    /// We lost access to the desktop — lock screen, UAC secure desktop,
    /// display mode change, fast user switch. The caller should surface a
    /// gap and keep polling: the duplication is re-created automatically on
    /// a later call, once the desktop is reachable again. Carries the reason
    /// for logging.
    Unavailable(String),
}

/// A live Desktop Duplication attachment to one display output.
pub struct CaptureSession {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    /// CPU-readable texture the GPU frame is copied into before mapping.
    /// Lazily created and then reused for the lifetime of the session —
    /// allocating one per frame would dominate the per-frame cost.
    /// Dropped on resolution change so the next frame reallocates.
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    /// Which output (display) this session is attached to, retained so the
    /// duplication can be rebuilt after `DXGI_ERROR_ACCESS_LOST`.
    output_index: u32,
    /// Set while we hold a frame from `AcquireNextFrame`. The API requires
    /// exactly one `ReleaseFrame` per successful acquire; tracking it means
    /// an early return on an error path cannot leak the frame and wedge
    /// every subsequent acquire.
    frame_held: bool,
    /// The duplication is dead and must be re-created before the next
    /// acquire.
    ///
    /// Recovery is **deferred rather than immediate** because the event that
    /// killed the duplication is usually still in force when we notice it:
    /// `DuplicateOutput` fails while the workstation is locked or the secure
    /// desktop is up, which is precisely what raised `ACCESS_LOST` in the
    /// first place. Rebuilding eagerly would therefore fail almost every
    /// time, and treating that failure as fatal would kill the session at
    /// the exact moment the three-state design exists to survive. So the
    /// flag is set, `Unavailable` is reported, and each later call retries
    /// the rebuild until the desktop comes back.
    needs_rebuild: bool,
}

impl CaptureSession {
    /// Attach to a display output. `output_index` is the position in the
    /// adapter's output list — 0 is the primary display on a
    /// single-adapter machine.
    pub fn new(output_index: u32) -> Result<Self> {
        let (device, context) = create_device()?;
        let dupl = duplicate_output(&device, output_index)?;
        Ok(Self {
            device,
            context,
            dupl,
            staging: None,
            width: 0,
            height: 0,
            output_index,
            frame_held: false,
            needs_rebuild: false,
        })
    }

    /// Dimensions of the attached output, valid once the first frame has
    /// been captured (both zero before that).
    ///
    /// Retained across access loss: a session that captured at 3840x1600 and
    /// then hit the lock screen still reports 3840x1600, not `0x0`. The
    /// staging texture is what must be rebuilt on a mode change, and
    /// [`Self::ensure_staging`] already forces that whenever `staging` is
    /// `None` — so zeroing the dimensions bought nothing and made every
    /// consumer's "what resolution is this session?" answer a lie the moment
    /// the user locked their PC.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Report an unavailable desktop, having first honoured the caller's
    /// timeout.
    ///
    /// Both routes to `Unavailable` return **immediately** from the OS —
    /// `duplicate_output` fails fast while the desktop is unreachable, and
    /// `AcquireNextFrame` reports `ACCESS_LOST` without waiting out its
    /// timeout. Neither is self-pacing, so the sleep has to be here.
    ///
    /// This exists because the first attempt at the fix only throttled the
    /// rebuild-retry route, on the assumption that `ACCESS_LOST` arrived
    /// after `AcquireNextFrame` had already blocked. A real lock-screen run
    /// disproved it: the session ping-ponged between a *successful*
    /// `duplicate_output` and an instant `ACCESS_LOST` on the next acquire,
    /// touching the throttled branch not once — 8421 `Unavailable`s in 90 s
    /// where ~900 was the intended ceiling.
    fn unavailable(timeout_ms: u32, reason: String) -> Capture {
        std::thread::sleep(Duration::from_millis(u64::from(timeout_ms)));
        Capture::Unavailable(reason)
    }

    /// Wait up to `timeout_ms` for the next desktop update.
    ///
    /// Errors are reserved for genuinely broken states. The routine
    /// desktop-lifecycle interruptions — lock screen, secure desktop, mode
    /// change — come back as [`Capture::Unavailable`], because a relay that
    /// gave up when the user locked their PC would be useless.
    ///
    /// `timeout_ms` bounds the wait for **every** outcome, not just a
    /// successful frame, so a plain `loop { next_frame(100) }` paces itself
    /// whatever the desktop is doing.
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Capture> {
        // Retry a deferred rebuild first. Still failing means the desktop
        // has not come back yet (the lock screen is still up), which is a
        // reportable gap, not an error — see `needs_rebuild`.
        if self.needs_rebuild {
            match duplicate_output(&self.device, self.output_index) {
                Ok(dupl) => {
                    self.dupl = dupl;
                    self.needs_rebuild = false;
                    // Access loss often accompanies a mode change, so force
                    // the staging texture to be re-created at the new size.
                    // The cached dimensions deliberately survive — see
                    // `dimensions`.
                    self.staging = None;
                }
                Err(e) => {
                    return Ok(Self::unavailable(
                        timeout_ms,
                        format!("desktop still gone: {e}"),
                    ));
                }
            }
        }

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        // SAFETY: both out-params are valid, correctly-typed locals living
        // past the call. On success the returned resource is owned by us
        // until the matching ReleaseFrame below.
        let acquired = unsafe {
            self.dupl
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };

        if let Err(e) = acquired {
            return match e.code() {
                // Nothing was presented within the timeout — an idle
                // desktop's normal state, not an error.
                DXGI_ERROR_WAIT_TIMEOUT => Ok(Capture::Idle),
                DXGI_ERROR_ACCESS_LOST => {
                    self.mark_lost();
                    Ok(Self::unavailable(
                        timeout_ms,
                        format!("desktop access lost: {e}"),
                    ))
                }
                _ => Err(anyhow!("AcquireNextFrame failed: {e}")),
            };
        }
        self.frame_held = true;

        // Run the rest under a closure so a failure still releases the
        // frame. Skipping ReleaseFrame leaves the duplication holding the
        // surface and every later acquire fails — a wedge that outlives the
        // error that caused it.
        let result = self.process_frame(&info, resource);
        self.release_frame();
        result
    }

    /// Copy the acquired frame to the CPU and collect its dirty rects.
    fn process_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<Capture> {
        // `LastPresentTime == 0` means the desktop image itself did not
        // change — the acquire woke us for a pointer-only update (move or
        // shape change). Sending a full frame for a moved cursor would be
        // pure waste; the pointer is composited separately anyway.
        if info.LastPresentTime == 0 {
            return Ok(Capture::Idle);
        }

        let resource = resource
            .ok_or_else(|| anyhow!("AcquireNextFrame succeeded but returned no resource"))?;
        let frame_tex: ID3D11Texture2D = resource
            .cast()
            .context("desktop resource is not an ID3D11Texture2D")?;

        // SAFETY: `frame_tex` is a live texture; GetDesc fills the out-param.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { frame_tex.GetDesc(&mut desc) };

        self.ensure_staging(desc.Width, desc.Height)?;
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| anyhow!("staging texture missing after ensure_staging"))?;

        // SAFETY: both textures are live, same format/size (ensure_staging
        // guarantees it), and CopyResource is the documented GPU→staging
        // path for a CPU read.
        unsafe { self.context.CopyResource(staging, &frame_tex) };

        let bgra = self.read_staging()?;
        let dirty_rects = self.dirty_rects()?;

        Ok(Capture::Frame(Box::new(Frame {
            width: self.width,
            height: self.height,
            bgra,
            dirty_rects,
            accumulated_frames: info.AccumulatedFrames,
        })))
    }

    /// Map the staging texture and copy it into a tightly packed buffer.
    ///
    /// The GPU's `RowPitch` is padded and generally exceeds `width * 4`, so
    /// the copy goes row by row; handing callers a padded buffer would push
    /// this detail into every consumer (and into the wire format).
    fn read_staging(&self) -> Result<Vec<u8>> {
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| anyhow!("read_staging called with no staging texture"))?;

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging was created USAGE_STAGING + CPU_ACCESS_READ, so a
        // D3D11_MAP_READ map is valid; the out-param is a live local.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("map staging texture")?;
        }

        let row_bytes = self.width as usize * BYTES_PER_PIXEL;
        let pitch = mapped.RowPitch as usize;
        let height = self.height as usize;
        let mut out = vec![0u8; row_bytes * height];

        if pitch < row_bytes {
            // Would over-read the mapped region on every row. Unmap before
            // bailing so the texture is not left mapped forever.
            // SAFETY: matching Unmap for the Map above.
            unsafe { self.context.Unmap(staging, 0) };
            bail!("staging RowPitch {pitch} is narrower than a {row_bytes}-byte row");
        }

        // SAFETY: the mapped region is `pitch * height` bytes (D3D11
        // guarantees at least that for a mapped 2D staging texture); every
        // read below stays inside row `y`'s `row_bytes` prefix, and
        // `pitch >= row_bytes` was just checked.
        unsafe {
            let src = mapped.pData as *const u8;
            for y in 0..height {
                std::ptr::copy_nonoverlapping(
                    src.add(y * pitch),
                    out.as_mut_ptr().add(y * row_bytes),
                    row_bytes,
                );
            }
            self.context.Unmap(staging, 0);
        }

        Ok(out)
    }

    /// Fetch the dirty rectangles for the frame currently held.
    ///
    /// A failure here is downgraded to an empty list rather than failing
    /// the whole frame: the pixels are already captured and perfectly
    /// usable, and the only cost of not knowing the dirty regions is
    /// falling back to sending a full frame.
    fn dirty_rects(&self) -> Result<Vec<DirtyRect>> {
        // Size the buffer generously up front. Asking with a zero-sized
        // buffer to learn the required size costs an extra call per frame,
        // and DXGI hands back the needed size only via the error path.
        let mut buf = vec![RECT::default(); 256];
        let mut required: u32 = 0;
        let buf_bytes = std::mem::size_of_val(buf.as_slice()) as u32;

        // SAFETY: buffer and out-param are live locals; `buf_bytes` is the
        // buffer's true size in bytes, which is what DXGI expects.
        let r = unsafe {
            self.dupl
                .GetFrameDirtyRects(buf_bytes, buf.as_mut_ptr(), &mut required)
        };

        match r {
            Ok(()) => {
                let n = required as usize / std::mem::size_of::<RECT>();
                Ok(buf.into_iter().take(n).map(DirtyRect::from).collect())
            }
            // More rects than the buffer held. Retry once at the size DXGI
            // asked for; a desktop churning through >256 regions in one
            // frame is rare enough that one retry is fine.
            Err(_) if required as usize > buf_bytes as usize => {
                let n = required as usize / std::mem::size_of::<RECT>();
                let mut big = vec![RECT::default(); n];
                let big_bytes = std::mem::size_of_val(big.as_slice()) as u32;
                let mut required2: u32 = 0;
                // SAFETY: as above, with a buffer sized to DXGI's own answer.
                let r2 = unsafe {
                    self.dupl
                        .GetFrameDirtyRects(big_bytes, big.as_mut_ptr(), &mut required2)
                };
                if r2.is_err() {
                    return Ok(Vec::new());
                }
                let n2 = required2 as usize / std::mem::size_of::<RECT>();
                Ok(big.into_iter().take(n2).map(DirtyRect::from).collect())
            }
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Create (or recreate) the CPU-readable staging texture.
    ///
    /// Reallocates only when the output resolution changed, so the steady
    /// state costs one comparison per frame.
    fn ensure_staging(&mut self, width: u32, height: u32) -> Result<()> {
        if self.staging.is_some() && self.width == width && self.height == height {
            return Ok(());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };

        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` is a fully-initialised staging description; the
        // out-param is a live local. No initial data for a staging texture.
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .context("create staging texture")?;
        }

        self.staging = tex;
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Release the frame held by the duplication, if any.
    ///
    /// Deliberately infallible: this runs on error paths, and a failure to
    /// release is already covered by the `ACCESS_LOST` handling on the next
    /// acquire. Returning an error here would mask the original one.
    fn release_frame(&mut self) {
        if !self.frame_held {
            return;
        }
        // SAFETY: exactly one ReleaseFrame per successful AcquireNextFrame,
        // which `frame_held` tracks.
        let _ = unsafe { self.dupl.ReleaseFrame() };
        self.frame_held = false;
    }

    /// Note the duplication as dead so the next call re-creates it.
    ///
    /// Infallible on purpose. The D3D device survives access loss — only the
    /// duplication object is invalidated — so recovery is just "duplicate
    /// the output again", and that has to happen when the desktop is back,
    /// not when we discover it is gone. `next_frame` does the retry.
    fn mark_lost(&mut self) {
        // The lost duplication owns no frame any more, so there is nothing
        // to release; clearing the flag keeps `release_frame` from calling
        // ReleaseFrame on a dead object.
        self.frame_held = false;
        self.needs_rebuild = true;
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // A held frame at drop time would otherwise keep the surface
        // checked out until the COM object itself is released.
        self.release_frame();
    }
}

/// Create the D3D11 device the duplication binds to.
///
/// Hardware only, deliberately. An earlier draft fell back to WARP (the
/// software rasteriser) on the theory that it would keep VMs and hosts with
/// a GPU driver mid-update working — but that does not hold: `DuplicateOutput`
/// requires a device on the adapter that actually owns the target display,
/// and the WARP adapter has no outputs attached at all. The fallback would
/// let `create_device` succeed and then push the failure one step later into
/// `EnumOutputs`, reporting "no display output at index N" for a machine
/// whose real problem was that hardware device creation failed. Failing here
/// keeps the error pointing at the actual cause.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    // Duplication hands back BGRA surfaces, so the device must be created
    // with BGRA support or DuplicateOutput fails.
    let levels = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    try_create_device(D3D_DRIVER_TYPE_HARDWARE, &levels)
}

fn try_create_device(
    driver: D3D_DRIVER_TYPE,
    levels: &[D3D_FEATURE_LEVEL],
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;

    // SAFETY: null adapter + non-software driver type is the documented
    // "pick the default adapter" form; both out-params are live locals.
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            // No software rasteriser DLL: the driver type above already
            // selects hardware or the built-in WARP renderer.
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .with_context(|| format!("D3D11CreateDevice({driver:?})"))?;
    }

    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
    let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;
    Ok((device, context))
}

/// Walk device → adapter → output and start duplicating it.
fn duplicate_output(device: &ID3D11Device, output_index: u32) -> Result<IDXGIOutputDuplication> {
    let dxgi_device: IDXGIDevice = device
        .cast()
        .context("D3D11 device does not implement IDXGIDevice")?;

    // SAFETY: `dxgi_device` is live; GetAdapter returns an owned interface.
    let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }.context("get DXGI adapter")?;

    // SAFETY: `adapter` is live. EnumOutputs errors on an out-of-range
    // index, which is the "no such display" case reported below.
    let output = unsafe { adapter.EnumOutputs(output_index) }
        .with_context(|| format!("no display output at index {output_index}"))?;

    let output1: IDXGIOutput1 = output
        .cast()
        .context("display output does not implement IDXGIOutput1 (needs Windows 8+)")?;

    // SAFETY: both interfaces are live. This is the call that fails when
    // another process already holds a duplication on this output, or when
    // the caller is not attached to the input desktop.
    unsafe { output1.DuplicateOutput(device) }.context(
        "DuplicateOutput failed — another process may already be duplicating this output, \
         or this process is not running in the interactive desktop session",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_rect_area_is_width_times_height() {
        let r = DirtyRect {
            left: 10,
            top: 20,
            right: 110,
            bottom: 70,
        };
        assert_eq!(r.area_px(), 100 * 50);
    }

    #[test]
    fn degenerate_dirty_rect_has_zero_area() {
        // An inverted rect must not wrap the unsigned area arithmetic into
        // a huge bogus number — the probe sums these into a u64.
        let r = DirtyRect {
            left: 100,
            top: 100,
            right: 10,
            bottom: 10,
        };
        assert_eq!(r.area_px(), 0);
    }

    #[test]
    fn empty_dirty_rect_has_zero_area() {
        let r = DirtyRect {
            left: 5,
            top: 5,
            right: 5,
            bottom: 5,
        };
        assert_eq!(r.area_px(), 0);
    }

    #[test]
    fn frame_dirty_area_sums_every_rect() {
        let f = Frame {
            width: 1920,
            height: 1080,
            bgra: Vec::new(),
            dirty_rects: vec![
                DirtyRect {
                    left: 0,
                    top: 0,
                    right: 10,
                    bottom: 10,
                },
                DirtyRect {
                    left: 100,
                    top: 100,
                    right: 120,
                    bottom: 110,
                },
            ],
            accumulated_frames: 1,
        };
        assert_eq!(f.dirty_area_px(), 100 + 200);
        assert_eq!(f.total_px(), 1920 * 1080);
    }

    #[test]
    fn frame_with_no_dirty_rects_reports_zero_area() {
        // "Empty" means the dirty-rect query failed, and the caller is
        // expected to treat that as "send everything" — but the metric
        // itself must still be 0, not a panic or a full-frame guess.
        let f = Frame {
            width: 640,
            height: 480,
            bgra: Vec::new(),
            dirty_rects: Vec::new(),
            accumulated_frames: 1,
        };
        assert_eq!(f.dirty_area_px(), 0);
    }

    #[test]
    fn rect_converts_from_win32_rect() {
        let r: DirtyRect = RECT {
            left: 1,
            top: 2,
            right: 3,
            bottom: 4,
        }
        .into();
        assert_eq!(
            r,
            DirtyRect {
                left: 1,
                top: 2,
                right: 3,
                bottom: 4
            }
        );
    }
}
