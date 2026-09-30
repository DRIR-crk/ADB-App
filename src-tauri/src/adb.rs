use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use tauri::Emitter;
use tokio::io::AsyncReadExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct AdbResult {
    pub exit_code: i32,
    /// stdout and stderr merged (stdout first), for messages shown to the user.
    pub output: String,
    /// stdout only, for parsing: adb prints warnings such as
    /// `adb server version (41) doesn't match this client` on stderr.
    pub stdout: String,
}

impl AdbResult {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// Run an ADB command and return the text output.
pub async fn run_adb(args: &[&str]) -> Result<AdbResult, String> {
    let path = crate::tools::resolve_tool_path("adb")
        .ok_or_else(|| "ADB is not installed. Configure or install it in Settings.".to_string())?;
    run_adb_with_path(path.to_string_lossy().as_ref(), args).await
}

/// Run an ADB command with a specific adb path.
///
/// The child is killed if the returned future is dropped (e.g. on a timeout) and never
/// inherits stdin, so commands that would prompt for input fail fast instead of hanging.
pub async fn run_adb_with_path(adb_path: &str, args: &[&str]) -> Result<AdbResult, String> {
    let mut cmd = crate::process::tokio_command(adb_path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn adb: {}", e))?;

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("Failed to wait for adb: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let combined = if stderr.is_empty() {
        stdout.clone()
    } else if stdout.is_empty() {
        stderr.clone()
    } else {
        format!("{}\n{}", stdout, stderr)
    };

    let exit_code = output.status.code().unwrap_or(-1);

    Ok(AdbResult {
        exit_code,
        output: combined,
        stdout,
    })
}

/// Run an ADB command targeting a specific device serial.
pub async fn run_adb_for_serial(serial: &str, args: &[&str]) -> Result<AdbResult, String> {
    let mut full_args = vec!["-s", serial];
    full_args.extend_from_slice(args);
    run_adb(&full_args).await
}

/// Run an ADB command and return binary (raw bytes) output.
pub async fn run_adb_binary(args: &[&str]) -> Result<(i32, Vec<u8>), String> {
    let path = crate::tools::resolve_tool_path("adb")
        .ok_or_else(|| "ADB is not installed. Configure or install it in Settings.".to_string())?;
    run_adb_binary_with_path(path.to_string_lossy().as_ref(), args).await
}

/// Raw output of an adb command plus the start of its stderr (the reason of a failure).
#[derive(Debug, Clone)]
pub struct BinaryResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// How much stderr is kept; the rest is drained so adb can never block on a full pipe.
const STDERR_KEEP_BYTES: usize = 8 * 1024;

/// Run an ADB command with a specific path and return binary output.
pub async fn run_adb_binary_with_path(
    adb_path: &str,
    args: &[&str],
) -> Result<(i32, Vec<u8>), String> {
    let result = run_adb_binary_detailed_with_path(adb_path, args).await?;
    Ok((result.exit_code, result.stdout))
}

/// Like `run_adb_binary_with_path`, but also returns stderr.
pub async fn run_adb_binary_detailed_with_path(
    adb_path: &str,
    args: &[&str],
) -> Result<BinaryResult, String> {
    let mut cmd = crate::process::tokio_command(adb_path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn adb: {}", e))?;
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err("Failed to capture adb output".to_string());
    };

    // Both pipes are read at the same time: reading one to the end first can deadlock.
    let read_stdout = async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    };
    let read_stderr = async {
        let mut kept = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = STDERR_KEEP_BYTES.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        kept
    };
    let (stdout_bytes, stderr_bytes) = tokio::join!(read_stdout, read_stderr);
    let stdout_bytes = stdout_bytes.map_err(|e| format!("Failed to read stdout: {}", e))?;

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait for adb: {}", e))?;

    Ok(BinaryResult {
        exit_code: status.code().unwrap_or(-1),
        stdout: stdout_bytes,
        stderr: String::from_utf8_lossy(&stderr_bytes).trim().to_string(),
    })
}

/// Binary adb command targeting a specific device serial, with stderr.
pub async fn run_adb_binary_detailed_for_serial(
    serial: &str,
    args: &[&str],
) -> Result<BinaryResult, String> {
    let path = crate::tools::resolve_tool_path("adb")
        .ok_or_else(|| "ADB is not installed. Configure or install it in Settings.".to_string())?;
    let mut full_args = vec!["-s", serial];
    full_args.extend_from_slice(args);
    run_adb_binary_detailed_with_path(path.to_string_lossy().as_ref(), &full_args).await
}

/// Run an ADB binary command targeting a specific device serial.
pub async fn run_adb_binary_for_serial(
    serial: &str,
    args: &[&str],
) -> Result<(i32, Vec<u8>), String> {
    let mut full_args = vec!["-s", serial];
    full_args.extend_from_slice(args);
    run_adb_binary(&full_args).await
}

// ---------------------------------------------------------------------------------------
// Device tracker (`adb track-devices`)
// ---------------------------------------------------------------------------------------

/// Number of callers that need the tracker (and therefore the adb server) to stay down,
/// e.g. while the adb binaries are being replaced.
static TRACKER_PAUSES: AtomicUsize = AtomicUsize::new(0);
static TRACKER_CHANGED: Notify = Notify::const_new();
/// PID of the running `adb track-devices` client (0 when none), so it can be killed on exit.
static TRACKER_PID: AtomicU32 = AtomicU32::new(0);
static TRACKER_STOPPED: AtomicBool = AtomicBool::new(false);

/// Keeps the device tracker stopped while alive. A running tracker holds `adb.exe` open and
/// would respawn the adb server right after `kill-server`, which breaks replacing the binaries.
pub struct TrackerPause;

impl TrackerPause {
    pub fn new() -> Self {
        TRACKER_PAUSES.fetch_add(1, Ordering::SeqCst);
        TRACKER_CHANGED.notify_waiters();
        TrackerPause
    }
}

impl Default for TrackerPause {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TrackerPause {
    fn drop(&mut self) {
        TRACKER_PAUSES.fetch_sub(1, Ordering::SeqCst);
        TRACKER_CHANGED.notify_waiters();
    }
}

fn tracker_paused() -> bool {
    TRACKER_STOPPED.load(Ordering::SeqCst) || TRACKER_PAUSES.load(Ordering::SeqCst) > 0
}

/// Stops the tracker for good (app exit) and kills its adb client, which would otherwise
/// outlive the app and keep `adb.exe` locked.
pub fn stop_tracker() {
    TRACKER_STOPPED.store(true, Ordering::SeqCst);
    TRACKER_CHANGED.notify_waiters();
    let pid = TRACKER_PID.swap(0, Ordering::SeqCst);
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    let _ = crate::process::command("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();
    #[cfg(not(windows))]
    let _ = crate::process::command("kill")
        .arg(pid.to_string())
        .output();
}

/// `adb kill-server`, using the currently resolved adb.
pub async fn kill_server() -> Result<(), String> {
    run_adb(&["kill-server"]).await.map(|_| ())
}

/// Collapses a burst of tracker lines (one per device on every change) into a single event.
const TRACKER_DEBOUNCE: Duration = Duration::from_millis(120);

/// Start a background tracker that emits a Tauri event when devices change
pub async fn start_device_tracker(app: tauri::AppHandle) {
    loop {
        if tracker_paused() {
            // Wake up as soon as the pause is lifted instead of polling.
            let _ = tokio::time::timeout(Duration::from_millis(500), TRACKER_CHANGED.notified())
                .await;
            continue;
        }

        let path = match crate::tools::resolve_tool_path("adb") {
            Some(p) => p,
            None => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let mut cmd = crate::process::tokio_command(path.to_string_lossy().as_ref());
        cmd.arg("track-devices")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        if let Ok(mut child) = cmd.spawn() {
            TRACKER_PID.store(child.id().unwrap_or(0), Ordering::SeqCst);
            if let Some(stdout) = child.stdout.take() {
                let mut reader = BufReader::new(stdout).lines();
                'tracking: loop {
                    let notified = TRACKER_CHANGED.notified();
                    tokio::pin!(notified);
                    if tracker_paused() {
                        break;
                    }
                    tokio::select! {
                        line = reader.next_line() => match line {
                            Ok(Some(_)) => {
                                // Swallow the rest of the burst, then notify once.
                                loop {
                                    match tokio::time::timeout(TRACKER_DEBOUNCE, reader.next_line()).await {
                                        Ok(Ok(Some(_))) => continue,
                                        Ok(_) => {
                                            let _ = app.emit("device-list-changed", ());
                                            break 'tracking;
                                        }
                                        Err(_) => break,
                                    }
                                }
                                let _ = app.emit("device-list-changed", ());
                            }
                            _ => break,
                        },
                        _ = &mut notified => {}
                    }
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            TRACKER_PID.store(0, Ordering::SeqCst);
        }

        if !tracker_paused() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
