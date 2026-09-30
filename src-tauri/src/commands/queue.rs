use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{async_runtime, AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

use crate::commands::operations::{
    install_application_packages, pull_file, run_device_action, AppInstallOptions,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationJob {
    pub id: String,
    pub r#type: String, // "upload" | "download" | "install"
    pub name: String,
    pub source: String,
    pub destination: Option<String>,
    pub is_directory: bool,
    pub status: String, // "idle" | "transferring" | "installing" | "success" | "error"
    pub error: Option<String>,
    pub children: Option<Vec<OperationJob>>,
    #[serde(default)]
    pub serial: String,
}

#[derive(Default)]
pub struct JobQueueState(pub Arc<Mutex<Vec<OperationJob>>>);

/// The job the processor is currently running. Kept outside the queue lock so cancelling never
/// has to wait for a transfer.
struct ActiveJob {
    id: String,
    /// Run token: a result is only applied when it belongs to this exact run of the job.
    token: u64,
    /// Attached once the task is spawned. Aborting drops the adb future, which kills the child.
    abort: Option<tokio::task::AbortHandle>,
    /// Install jobs run blocking code that cannot be aborted: they are only flagged and the
    /// processor keeps waiting for them, discarding the result when they finally end.
    abortable: bool,
    cancelled: bool,
}

static ACTIVE_JOB: std::sync::Mutex<Option<ActiveJob>> = std::sync::Mutex::new(None);
static RUN_TOKEN: AtomicU64 = AtomicU64::new(1);

fn active_job() -> std::sync::MutexGuard<'static, Option<ActiveJob>> {
    ACTIVE_JOB
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Marks the running job as cancelled when it is `id`, aborting its task when possible.
fn cancel_active_job(id: &str) {
    if let Some(active) = active_job().as_mut() {
        if active.id == id {
            active.cancelled = true;
            if active.abortable {
                if let Some(abort) = &active.abort {
                    abort.abort();
                }
            }
        }
    }
}

fn is_running(status: &str) -> bool {
    status == "transferring" || status == "installing"
}

#[tauri::command]
pub async fn get_jobs(state: State<'_, JobQueueState>) -> Result<Vec<OperationJob>, String> {
    let queue = state.0.lock().await;
    Ok(queue.clone())
}

#[tauri::command]
pub async fn enqueue_job(
    app: AppHandle,
    state: State<'_, JobQueueState>,
    job: OperationJob,
) -> Result<(), String> {
    let mut queue = state.0.lock().await;
    queue.push(job);
    let _ = app.emit("operations-update", queue.clone());
    Ok(())
}

#[tauri::command]
pub async fn clear_completed_jobs(
    app: AppHandle,
    state: State<'_, JobQueueState>,
) -> Result<(), String> {
    let mut queue = state.0.lock().await;
    queue.retain(|j| j.status != "success");
    let _ = app.emit("operations-update", queue.clone());
    Ok(())
}

#[tauri::command]
pub async fn retry_job(
    app: AppHandle,
    state: State<'_, JobQueueState>,
    id: String,
    parent_id: Option<String>,
) -> Result<(), String> {
    let mut queue = state.0.lock().await;
    // Only failed jobs can be retried: re-queuing a running job would start it a second time.
    if let Some(pid) = parent_id {
        if let Some(parent) = queue.iter_mut().find(|j| j.id == pid) {
            if let Some(children) = &mut parent.children {
                if let Some(child_index) = children
                    .iter()
                    .position(|c| c.id == id && c.status == "error")
                {
                    let mut child = children.remove(child_index);
                    child.status = "idle".to_string();
                    child.error = None;
                    queue.push(child);
                }
            }
        }
    } else if let Some(job) = queue
        .iter_mut()
        .find(|j| j.id == id && j.status == "error")
    {
        job.status = "idle".to_string();
        job.error = None;
    }
    let _ = app.emit("operations-update", queue.clone());
    Ok(())
}

#[tauri::command]
pub async fn remove_job(
    app: AppHandle,
    state: State<'_, JobQueueState>,
    id: String,
    parent_id: Option<String>,
) -> Result<(), String> {
    let mut queue = state.0.lock().await;
    if let Some(pid) = parent_id {
        if let Some(parent) = queue.iter_mut().find(|j| j.id == pid) {
            if let Some(children) = &mut parent.children {
                children.retain(|c| c.id != id);
            }
        }
    } else {
        // Cancel first, under the queue lock, so the processor cannot finish it in between.
        cancel_active_job(&id);
        queue.retain(|j| j.id != id);
    }
    let _ = app.emit("operations-update", queue.clone());
    Ok(())
}

pub fn start_job_processor(app: AppHandle) {
    let state = app.state::<JobQueueState>();
    let queue_arc = state.0.clone();

    async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            let Some((job, token)) = take_next_job(&queue_arc, &app).await else {
                continue;
            };

            // Run the job in its own task so it can be aborted, and so a panic inside the
            // operation code cannot take the processor down with it.
            let handle = tokio::spawn(run_job(job.clone()));
            {
                let mut active = active_job();
                match active.as_mut() {
                    Some(current) if current.id == job.id && current.token == token => {
                        if current.cancelled && current.abortable {
                            handle.abort();
                        }
                        current.abort = Some(handle.abort_handle());
                    }
                    _ => handle.abort(),
                }
            }

            // Wait even for cancelled install jobs: their blocking work cannot be interrupted
            // and the next job must not start until it really ended.
            let joined = handle.await;

            let owns_result = active_job().take().is_some_and(|current| {
                current.id == job.id && current.token == token && !current.cancelled
            });
            if !owns_result {
                // Removed while running: it is already gone from the queue, drop the result.
                continue;
            }

            let result = match joined {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => {
                    Err("The operation was cancelled".to_string())
                }
                Err(error) => Err(format!("The operation stopped unexpectedly: {error}")),
            };
            finish_job(&queue_arc, &app, &job.id, result).await;
        }
    });
}

/// Picks the next idle job, marks it as running and registers it as the active job, all under
/// the queue lock so `remove_job` can never race with the start of a job.
async fn take_next_job(
    queue_arc: &Arc<Mutex<Vec<OperationJob>>>,
    app: &AppHandle,
) -> Option<(OperationJob, u64)> {
    let mut queue = queue_arc.lock().await;

    // Nothing is running at this point, so a job still marked as running is stale (for example
    // enqueued in that state): fail it instead of blocking the queue forever.
    let mut changed = false;
    for job in queue.iter_mut().filter(|j| is_running(&j.status)) {
        job.status = "error".to_string();
        job.error = Some("The operation was interrupted".to_string());
        changed = true;
    }

    let picked = queue.iter().position(|j| j.status == "idle").map(|index| {
        let job = &mut queue[index];
        job.status = if job.r#type == "install" {
            "installing".to_string()
        } else {
            "transferring".to_string()
        };
        job.error = None;
        job.children = None;
        let token = RUN_TOKEN.fetch_add(1, Ordering::Relaxed);
        *active_job() = Some(ActiveJob {
            id: job.id.clone(),
            token,
            abort: None,
            abortable: job.r#type != "install",
            cancelled: false,
        });
        (job.clone(), token)
    });

    if picked.is_some() || changed {
        let _ = app.emit("operations-update", queue.clone());
    }
    picked
}

async fn run_job(job: OperationJob) -> Result<String, String> {
    let serial = job.serial.clone();
    if serial.is_empty() {
        return Err("No device connected".to_string());
    }

    match job.r#type.as_str() {
        "upload" => {
            let args = vec![
                "push".to_string(),
                "--sync".to_string(),
                job.source.clone(),
                job.destination.clone().unwrap_or_default(),
            ];
            run_device_action(serial, args).await
        }
        "download" => {
            pull_file(
                serial,
                job.source.clone(),
                job.destination.clone().unwrap_or_default(),
            )
            .await
        }
        "install" => {
            let mut install_options = AppInstallOptions {
                replace_existing: false,
                grant_runtime_permissions: false,
                bypass_low_target_sdk_block: false,
            };
            if let Some(dest) = &job.destination {
                if let Ok(options) = serde_json::from_str::<serde_json::Value>(dest) {
                    install_options.replace_existing = options
                        .get("replace")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    install_options.grant_runtime_permissions = options
                        .get("grant")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    install_options.bypass_low_target_sdk_block = options
                        .get("bypass")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                }
            }
            install_application_packages(serial, vec![job.source.clone()], install_options).await
        }
        _ => Err(format!("Unknown job type: {}", job.r#type)),
    }
}

async fn finish_job(
    queue_arc: &Arc<Mutex<Vec<OperationJob>>>,
    app: &AppHandle,
    job_id: &str,
    result: Result<String, String>,
) {
    let mut queue = queue_arc.lock().await;
    // Only the run that is still marked as running may be resolved: a job removed or re-queued
    // in the meantime keeps its own state.
    if let Some(job) = queue
        .iter_mut()
        .find(|j| j.id == job_id && is_running(&j.status))
    {
        match result {
            Ok(_) => {
                job.status = "success".to_string();
            }
            Err(e) => {
                job.status = "error".to_string();
                job.error = Some(e.clone());

                // Parse ADB push errors using string manipulation
                let mut children = Vec::new();
                for line in e.lines() {
                    if line.starts_with("adb: error: failed to copy '") {
                        let remainder = &line["adb: error: failed to copy '".len()..];
                        if let Some(quote1) = remainder.find("' to '") {
                            let src = &remainder[..quote1];
                            let remainder2 = &remainder[quote1 + "' to '".len()..];
                            if let Some(quote2) = remainder2.find("': ") {
                                let dest = &remainder2[..quote2];
                                let reason = &remainder2[quote2 + "': ".len()..];

                                let name = Path::new(src)
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string();

                                children.push(OperationJob {
                                    id: format!(
                                        "{}{}",
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis(),
                                        rand::random::<u16>()
                                    ),
                                    r#type: job.r#type.clone(),
                                    name,
                                    source: src.to_string(),
                                    destination: Some(dest.to_string()),
                                    is_directory: false,
                                    status: "error".to_string(),
                                    error: Some(reason.to_string()),
                                    children: None,
                                    serial: job.serial.clone(),
                                });
                            }
                        }
                    }
                }

                if !children.is_empty() {
                    job.children = Some(children);
                }
            }
        }
    }
    let _ = app.emit("operations-update", queue.clone());
}
