//! Console-user ScreenCaptureKit capture and the read-only remote responder.
//! The child owns framework state; the daemon owns its lifetime and transport.
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use kanade_shared::{
    subject,
    wire::{FrameMeta, RemoteCtrl, RemoteCtrlReply, TileEncoding},
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{Mutex, oneshot};
use tracing::warn;

use crate::capture_frame_io::{
    CapturedTile, FrameHeader, MAX_FRAME_BYTES, encode_frame, read_frame,
};
use crate::capture_types::Frame;

unsafe extern "C" {
    fn kanade_capture_supported() -> bool;
    fn kanade_capture(
        pixels: *mut *mut u8,
        width: *mut u32,
        height: *mut u32,
        reason: *mut std::ffi::c_char,
        reason_len: usize,
    ) -> i32;
}

pub fn supported() -> bool {
    // SAFETY: no arguments; only performs an OS availability check.
    unsafe { kanade_capture_supported() }
}

fn console_uid() -> Result<u32> {
    crate::screen_permission_macos::console_uid()
        .context("no user logged in; sign in on the Mac before connecting")
}

fn require_console_user() -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != console_uid()? {
        bail!("capture requires the currently logged-in console user; reconnect after signing in");
    }
    Ok(())
}

fn capture_error(code: i32, detail: &str) -> String {
    match code {
        1 => "remote viewing requires macOS 14 or later".into(),
        2 => format!(
            "Screen Recording permission is missing for console user {}. On the Mac, open System Settings > Privacy & Security > Screen Recording (or Screen & System Audio Recording), enable the capture process shown by macOS (agent executable: {}), then restart the installed agent and reconnect. Permission attribution depends on the launch context; a Terminal permission does not verify the installed LaunchDaemon path.",
            // SAFETY: getuid has no preconditions.
            unsafe { libc::getuid() },
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "kanade-agent".into())
        ),
        3 => "no primary display available for the logged-in user; attach/unlock a display and reconnect".into(),
        _ => format!("screen capture unavailable: {detail}; unlock the Mac and reconnect"),
    }
}

fn snapshot() -> Result<Frame> {
    require_console_user()?;
    let mut pixels = std::ptr::null_mut();
    let (mut width, mut height) = (0, 0);
    let mut reason = [0 as std::ffi::c_char; 1024];
    // SAFETY: all out pointers point to live locals; the bridge returns a
    // malloc-owned, tightly packed BGRA buffer on success only.
    let code = unsafe {
        kanade_capture(
            &mut pixels,
            &mut width,
            &mut height,
            reason.as_mut_ptr(),
            reason.len(),
        )
    };
    if code != 0 {
        // SAFETY: the bridge always leaves this zero-initialized buffer terminated.
        let detail = unsafe { std::ffi::CStr::from_ptr(reason.as_ptr()) }.to_string_lossy();
        bail!("{}", capture_error(code, &detail));
    }
    // SAFETY: successful bridge result owns width * height * 4 readable bytes.
    let bgra = unsafe {
        let copy = std::slice::from_raw_parts(
            pixels,
            width as usize * height as usize * crate::capture_types::BYTES_PER_PIXEL,
        )
        .to_vec();
        libc::free(pixels.cast());
        copy
    };
    // Logout/user switching during an asynchronous screenshot must not publish it.
    require_console_user()?;
    Ok(Frame {
        width,
        height,
        bgra,
        dirty_rects: Vec::new(),
    })
}

/// Hidden child mode. stdin is a lifetime pipe, including on an idle screen.
pub fn capture(cli: &crate::Cli) -> Result<()> {
    std::thread::spawn(|| {
        use std::io::Read;
        let mut byte = [0];
        let _ = std::io::stdin().read(&mut byte);
        std::process::exit(0);
    });
    let result = capture_loop(cli);
    if let Err(e) = &result {
        // A typed gap is also the startup refusal, before any tile is emitted.
        let bytes = encode_frame(&FrameHeader::Gap, e.to_string().as_bytes())?;
        let _ = std::io::stdout().write_all(&bytes);
    }
    result
}

fn capture_loop(cli: &crate::Cli) -> Result<()> {
    validate_start(cli.session_capture_output, false)?;
    let mut out = std::io::stdout().lock();
    let mut scratch = Vec::new();
    let started = std::time::Instant::now();
    let mut seq = 0;
    loop {
        let tick = std::time::Instant::now();
        let frame = snapshot()?;
        let tiles = crate::capture_encode::encode_tiles(
            &frame,
            cli.session_capture_quality.clamp(1, 100),
            &mut scratch,
        )?;
        let count = u16::try_from(tiles.len()).context("too many screen tiles")?;
        if count == 0 {
            bail!("capture produced no usable tiles");
        }
        for (index, tile) in tiles.into_iter().enumerate() {
            if tile.jpeg.len() > kanade_shared::wire::MAX_TILE_BYTES {
                bail!("screen tile exceeds transport limit");
            }
            let header = FrameHeader::Tile {
                meta: FrameMeta {
                    frame_seq: seq,
                    tile_index: index as u16,
                    tile_count: count,
                    x: tile.x,
                    y: tile.y,
                    w: tile.w,
                    h: tile.h,
                    screen_w: frame.width,
                    screen_h: frame.height,
                    captured_at_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
                        as u64,
                },
                encoding: TileEncoding::Jpeg,
            };
            out.write_all(&encode_frame(&header, &tile.jpeg)?)?;
        }
        out.flush()?;
        seq += 1;
        if cli.session_capture_secs > 0 && started.elapsed().as_secs() >= cli.session_capture_secs {
            return Ok(());
        }
        // Poll full screenshots conservatively; no dirty-region API in this slice.
        let interval =
            Duration::from_millis(1000 / u64::from(cli.session_capture_max_fps.clamp(1, 10)));
        std::thread::sleep(interval.saturating_sub(tick.elapsed()));
    }
}

fn validate_start(output: u32, input: bool) -> Result<()> {
    if input {
        bail!("macOS remote viewing is read-only; input control is unsupported");
    }
    if output != 0 {
        bail!("macOS remote viewing supports only the primary display (output 0)");
    }
    Ok(())
}

struct Live {
    id: String,
    generation: uuid::Uuid,
    cancel: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Live {
    fn drop(&mut self) {
        self.cancel.take();
    }
}

/// Kill the whole launch chain even if initialization or its future is cancelled.
struct Group(u32);
impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: this is the child's dedicated process group, never our own.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

type Sessions = Arc<Mutex<Option<Live>>>;

pub async fn serve(client: async_nats::Client, pc: String) {
    let live: Sessions = Arc::new(Mutex::new(None));
    loop {
        let mut sub = match client.subscribe(subject::remote_ctrl(&pc)).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "remote subscribe failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        while let Some(msg) = sub.next().await {
            let Some(reply) = msg.reply else { continue };
            let response = match serde_json::from_slice::<RemoteCtrl>(&msg.payload) {
                Ok(ctrl) => handle(&client, &live, ctrl).await,
                Err(e) => RemoteCtrlReply::refused(format!("undecodable request: {e}")),
            };
            if let Ok(bytes) = serde_json::to_vec(&response) {
                let _ = client.publish(reply, bytes.into()).await;
            }
        }
        // Lost control transport: release capture before reconnecting.
        stop(&live, None).await;
    }
}

async fn stop(live: &Sessions, id: Option<&str>) -> RemoteCtrlReply {
    let mut guard = live.lock().await;
    if guard
        .as_ref()
        .is_some_and(|held| id.is_some_and(|id| id != held.id))
    {
        return RemoteCtrlReply::refused("another session holds this display");
    }
    let held = guard.take();
    drop(guard);
    if let Some(mut held) = held {
        held.cancel.take();
        let _ = (&mut held.task).await;
    }
    RemoteCtrlReply {
        accepted: true,
        ..Default::default()
    }
}

async fn handle(client: &async_nats::Client, live: &Sessions, ctrl: RemoteCtrl) -> RemoteCtrlReply {
    match ctrl {
        RemoteCtrl::Stop { session_id } => stop(live, Some(&session_id)).await,
        RemoteCtrl::Tune { .. } => {
            RemoteCtrlReply::refused("tune is not implemented; stop and start instead")
        }
        RemoteCtrl::Start {
            session_id,
            output_index,
            quality,
            max_fps,
            allow_input,
        } => {
            if let Err(e) = validate_start(output_index, allow_input) {
                return RemoteCtrlReply::refused(e.to_string());
            }
            let mut guard = live.lock().await;
            if let Some(held) = guard.as_ref() {
                return if held.id == session_id {
                    RemoteCtrlReply {
                        accepted: true,
                        ..Default::default()
                    }
                } else {
                    RemoteCtrlReply::refused("another session holds this display")
                };
            }
            match start(client, live, &session_id, quality, max_fps).await {
                Ok(held) => {
                    *guard = Some(held);
                    RemoteCtrlReply {
                        accepted: true,
                        ..Default::default()
                    }
                }
                Err(e) => RemoteCtrlReply::refused(format!("could not start capture: {e}")),
            }
        }
    }
}

async fn read_async(reader: &mut (impl AsyncRead + Unpin)) -> Result<CapturedTile> {
    let len = reader.read_u32_le().await? as usize;
    if len > MAX_FRAME_BYTES {
        bail!("capture pipe frame too large");
    }
    let mut bytes = vec![0; len + 4];
    bytes[..4].copy_from_slice(&(len as u32).to_le_bytes());
    reader.read_exact(&mut bytes[4..]).await?;
    Ok(read_frame(&mut std::io::Cursor::new(bytes))?)
}

fn initial_frame(tile: &CapturedTile) -> Result<()> {
    if let Some(reason) = tile.as_gap() {
        bail!("{reason}");
    }
    let Some((meta, _)) = tile.as_tile() else {
        bail!("capture did not initialize with a frame");
    };
    meta.validate()
        .map_err(|e| anyhow::anyhow!("capture geometry: {e}"))?;
    if tile.payload.len() > kanade_shared::wire::MAX_TILE_BYTES {
        bail!("capture tile exceeds transport limit");
    }
    if meta.screen_w == 0
        || meta.screen_h == 0
        || meta.w == 0
        || meta.h == 0
        || tile.payload.is_empty()
    {
        bail!("capture initialized with an invalid frame");
    }
    let image = image::load_from_memory_with_format(&tile.payload, image::ImageFormat::Jpeg)
        .context("capture initialized with an invalid JPEG")?;
    if image.width() != meta.w || image.height() != meta.h {
        bail!("capture JPEG dimensions disagree with tile metadata");
    }
    Ok(())
}

async fn publish(client: &async_nats::Client, subject: &str, tile: CapturedTile) -> Result<()> {
    let headers = match tile.header {
        FrameHeader::Tile { meta, encoding } => meta.to_headers(encoding),
        FrameHeader::Gap => kanade_shared::wire::gap_headers(),
        FrameHeader::Resumed => kanade_shared::wire::resumed_headers(),
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        client.publish_with_headers(subject.to_string(), headers, tile.payload.into()),
    )
    .await??;
    Ok(())
}

async fn start(
    client: &async_nats::Client,
    live: &Sessions,
    id: &str,
    quality: u8,
    fps: u8,
) -> Result<Live> {
    if !supported() {
        bail!("{}", capture_error(1, ""));
    }
    let exe = std::env::current_exe()?;
    let args = [
        "--session-capture".into(),
        "--session-capture-quality".into(),
        quality.clamp(1, 100).to_string(),
        "--session-capture-max-fps".into(),
        fps.clamp(1, 10).to_string(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // SAFETY: geteuid has no preconditions.
    let mut command = if unsafe { libc::geteuid() } == 0 {
        crate::process_as_user_macos::session_command(
            kanade_shared::wire::RunAs::User,
            exe.to_str().context("agent executable path is not UTF-8")?,
            &args,
            None,
        )?
    } else {
        require_console_user()?;
        let mut c = tokio::process::Command::new(&exe);
        c.args(&args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/");
        c
    };
    use std::process::Stdio;
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command
        .spawn()
        .context("launch console-user capture child")?;
    let group = Group(child.id().context("capture child has no pid")?);
    let stdin = child
        .stdin
        .take()
        .context("capture child has no lifetime pipe")?;
    let mut stdout = child.stdout.take().context("capture child has no stdout")?;
    // Shorter than both the NATS request default (10s) and backend START_TIMEOUT
    // (15s), including both native 3s waits.
    let first = match tokio::time::timeout(Duration::from_secs(8), read_async(&mut stdout)).await {
        Ok(Ok(first)) => first,
        result => {
            drop(group);
            let _ = child.wait().await;
            bail!(
                "capture initialization failed or timed out: {result:?}; check logged-in user, Screen Recording permission and installed executable access"
            );
        }
    };
    if let Err(e) = initial_frame(&first) {
        drop(group);
        let _ = child.wait().await;
        return Err(e);
    }
    let generation = uuid::Uuid::new_v4();
    let weak = Arc::downgrade(live);
    let (cancel, mut cancelled) = oneshot::channel();
    let client = client.clone();
    let subject = subject::remote_frame(id);
    let task = tokio::spawn(async move {
        let run = async {
            publish(&client, &subject, first).await?;
            loop {
                // No static desktop stalls: the child emits full snapshots.
                let tile = tokio::time::timeout(Duration::from_secs(10), read_async(&mut stdout))
                    .await??;
                if let Some(reason) = tile.as_gap() {
                    bail!("{reason}");
                }
                publish(&client, &subject, tile).await?;
            }
        };
        let failure = tokio::select! { _ = &mut cancelled => None, result = run => { let result: Result<()> = result; result.err() } };
        drop(stdin);
        drop(group);
        let _ = child.wait().await;
        if let Some(e) = failure {
            warn!(error = %e, "remote capture ended");
            let _ = publish(
                &client,
                &subject,
                CapturedTile {
                    header: FrameHeader::Gap,
                    payload: e.to_string().into_bytes(),
                },
            )
            .await;
        }
        if let Some(live) = weak.upgrade() {
            release_generation(&live, generation).await;
        }
    });
    Ok(Live {
        id: id.into(),
        generation,
        cancel: Some(cancel),
        task,
    })
}

async fn release_generation(live: &Sessions, generation: uuid::Uuid) {
    let mut guard = live.lock().await;
    if guard
        .as_ref()
        .is_some_and(|held| held.generation == generation)
    {
        guard.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_and_secondary_displays_are_explicitly_refused() {
        assert!(validate_start(0, false).is_ok());
        assert!(
            validate_start(0, true)
                .unwrap_err()
                .to_string()
                .contains("read-only")
        );
        assert!(validate_start(1, false).is_err());
    }

    #[tokio::test]
    async fn startup_error_survives_pipe_and_is_not_an_accept() {
        let bytes =
            encode_frame(&FrameHeader::Gap, b"Screen Recording permission is missing").unwrap();
        let tile = read_async(&mut bytes.as_slice()).await.unwrap();
        assert_eq!(
            initial_frame(&tile).unwrap_err().to_string(),
            "Screen Recording permission is missing"
        );
        let resume = CapturedTile {
            header: FrameHeader::Resumed,
            payload: vec![],
        };
        assert!(initial_frame(&resume).is_err());
    }

    #[tokio::test]
    async fn malformed_and_truncated_pipe_frames_fail() {
        assert!(
            read_async(&mut u32::MAX.to_le_bytes().as_slice())
                .await
                .is_err()
        );
        assert!(read_async(&mut [20, 0, 0, 0, 1].as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn stop_checks_ownership_and_is_idempotent() {
        let (cancel, cancelled) = oneshot::channel();
        let live = Arc::new(Mutex::new(Some(Live {
            id: "one".into(),
            generation: uuid::Uuid::new_v4(),
            cancel: Some(cancel),
            task: tokio::spawn(async {
                let _ = cancelled.await;
            }),
        })));
        assert!(!stop(&live, Some("two")).await.accepted);
        assert!(live.lock().await.is_some());
        assert!(stop(&live, Some("one")).await.accepted);
        assert!(stop(&live, Some("one")).await.accepted);
        assert!(live.lock().await.is_none());
    }

    #[tokio::test]
    async fn late_completion_cannot_remove_a_reconnected_session() {
        let old = uuid::Uuid::new_v4();
        let current = uuid::Uuid::new_v4();
        let (cancel, cancelled) = oneshot::channel();
        let live = Arc::new(Mutex::new(Some(Live {
            // Reusing an ID must still be safe against an old task finishing.
            id: "same-session".into(),
            generation: current,
            cancel: Some(cancel),
            task: tokio::spawn(async {
                let _ = cancelled.await;
            }),
        })));
        release_generation(&live, old).await;
        assert_eq!(live.lock().await.as_ref().unwrap().generation, current);
        release_generation(&live, current).await;
        assert!(live.lock().await.is_none());
    }

    #[tokio::test]
    async fn process_group_guard_terminates_and_reaps_capture_process() {
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("60").process_group(0).kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let group = Group(child.id().unwrap());
        drop(group);
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }

    #[test]
    fn first_tile_must_have_valid_geometry_and_transport_size() {
        let mut tile = CapturedTile {
            header: FrameHeader::Tile {
                meta: FrameMeta {
                    frame_seq: 0,
                    tile_index: 0,
                    tile_count: 1,
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2,
                    screen_w: 2,
                    screen_h: 2,
                    captured_at_ms: 0,
                },
                encoding: TileEncoding::Jpeg,
            },
            payload: crate::capture_encode::encode_jpeg(&[0; 12], 2, 2, 75).unwrap(),
        };
        assert!(initial_frame(&tile).is_ok());
        let valid = tile.payload.clone();
        tile.payload = vec![1, 2, 3];
        assert!(initial_frame(&tile).is_err());
        tile.payload.clear();
        assert!(initial_frame(&tile).is_err());
        tile.payload
            .resize(kanade_shared::wire::MAX_TILE_BYTES + 1, 0);
        assert!(initial_frame(&tile).is_err());
        tile.payload = valid;
        if let FrameHeader::Tile { ref mut meta, .. } = tile.header {
            meta.w = 3;
        }
        assert!(initial_frame(&tile).is_err());
    }

    #[test]
    fn permission_error_has_operator_recovery_steps() {
        let reason = capture_error(2, "");
        for text in [
            "console user",
            "System Settings",
            "restart",
            "reconnect",
            "LaunchDaemon",
        ] {
            assert!(reason.contains(text));
        }
        assert!(capture_error(1, "").contains("macOS 14"));
        assert!(capture_error(3, "").contains("no primary display"));
    }
}
