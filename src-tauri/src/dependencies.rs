use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
#[cfg(not(store_build))]
use std::time::Duration;

#[cfg(target_os = "macos")]
use flate2::read::GzDecoder;

use crate::tools::{executable_name, managed_dir};

#[cfg(not(store_build))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Zip,
    TarGz,
}

#[cfg(not(store_build))]
fn client() -> Result<reqwest::Client, String> {
    // Per-phase timeouts instead of one total deadline, so big downloads finish on slow links.
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())
}

#[cfg(not(store_build))]
async fn download_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    client
        .get(url)
        .header(reqwest::header::USER_AGENT, "ADB-App")
        .send()
        .await
        .map_err(|error| format!("Could not download the dependency: {error}"))?
        .error_for_status()
        .map_err(|error| format!("The dependency download failed: {error}"))?
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| format!("Could not read the download: {error}"))
}

#[cfg(not(store_build))]
async fn tool_asset(tool: &str, client: &reqwest::Client) -> Result<(String, ArchiveKind), String> {
    if std::env::consts::OS == "linux" {
        return Err("Automatic installation is disabled on Linux. Use your distribution's package manager and Auto detect.".to_string());
    }
    match tool {
        "adb" => Ok((
            match std::env::consts::OS {
                "windows" => {
                    "https://dl.google.com/android/repository/platform-tools-latest-windows.zip"
                }
                "macos" => {
                    "https://dl.google.com/android/repository/platform-tools-latest-darwin.zip"
                }
                _ => {
                    return Err(
                        "ADB does not provide Platform Tools for this operating system".to_string(),
                    )
                }
            }
            .to_string(),
            ArchiveKind::Zip,
        )),
        "scrcpy" => {
            let (prefix, kind) = match (std::env::consts::OS, std::env::consts::ARCH) {
                ("windows", "x86_64") => ("scrcpy-win64-", ArchiveKind::Zip),
                ("windows", "x86") => ("scrcpy-win32-", ArchiveKind::Zip),
                ("macos", "x86_64") => ("scrcpy-macos-x86_64-", ArchiveKind::TarGz),
                ("macos", "aarch64") => ("scrcpy-macos-aarch64-", ArchiveKind::TarGz),
                _ => {
                    return Err(
                        "scrcpy does not publish a binary compatible with this computer"
                            .to_string(),
                    )
                }
            };
            let release = client
                .get("https://api.github.com/repos/Genymobile/scrcpy/releases/latest")
                .header(reqwest::header::USER_AGENT, "ADB-App")
                .send()
                .await
                .map_err(|error| error.to_string())?
                .error_for_status()
                .map_err(|error| error.to_string())?
                .json::<serde_json::Value>()
                .await
                .map_err(|error| error.to_string())?;
            let url = release
                .get("assets")
                .and_then(serde_json::Value::as_array)
                .and_then(|assets| {
                    assets.iter().find_map(|asset| {
                        let name = asset.get("name")?.as_str()?;
                        let valid_extension = matches!(kind, ArchiveKind::Zip)
                            && name.ends_with(".zip")
                            || matches!(kind, ArchiveKind::TarGz) && name.ends_with(".tar.gz");
                        (name.starts_with(prefix) && valid_extension)
                            .then(|| {
                                asset
                                    .get("browser_download_url")?
                                    .as_str()
                                    .map(str::to_string)
                            })
                            .flatten()
                    })
                })
                .ok_or_else(|| "Could not find a compatible scrcpy download".to_string())?;
            Ok((url, kind))
        }
        _ => Err(format!("Unknown dependency: {tool}")),
    }
}

#[cfg(not(store_build))]
fn extract(bytes: &[u8], kind: ArchiveKind, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    match kind {
        ArchiveKind::Zip => {
            let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
                .map_err(|error| format!("Invalid ZIP: {error}"))?;
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
                let relative = entry
                    .enclosed_name()
                    .ok_or_else(|| "The ZIP contains an unsafe path".to_string())?;
                let output = destination.join(relative);
                if entry.is_dir() {
                    fs::create_dir_all(output).map_err(|error| error.to_string())?;
                } else {
                    if let Some(parent) = output.parent() {
                        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                    }
                    io::copy(
                        &mut entry,
                        &mut fs::File::create(&output).map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())?;
                    // Keep the Unix mode bits (executables) recorded in the archive.
                    #[cfg(unix)]
                    if let Some(mode) = entry.unix_mode() {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&output, fs::Permissions::from_mode(mode & 0o777))
                            .map_err(|error| error.to_string())?;
                    }
                }
            }
        }
        ArchiveKind::TarGz => {
            #[cfg(target_os = "macos")]
            tar::Archive::new(GzDecoder::new(Cursor::new(bytes)))
                .unpack(destination)
                .map_err(|error| format!("Could not extract TAR.GZ: {error}"))?;
        }
    }
    Ok(())
}

#[cfg(not(store_build))]
fn find_file(directory: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|value| value == name) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(not(store_build))]
fn remove_dir(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(store_build))]
const SWAP_ATTEMPTS: usize = 12;
#[cfg(not(store_build))]
const SWAP_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Errors Windows reports while a process that is shutting down still holds a file open
/// (ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION).
#[cfg(not(store_build))]
fn is_transient_lock(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

/// `fs::rename` across volumes: ERROR_NOT_SAME_DEVICE (17) on Windows, EXDEV (18) elsewhere.
#[cfg(not(store_build))]
fn is_cross_device(error: &io::Error) -> bool {
    let code = if cfg!(windows) { 17 } else { 18 };
    error.kind() == io::ErrorKind::CrossesDevices || error.raw_os_error() == Some(code)
}

/// `fs::rename` that retries briefly on Windows: the adb server that was just killed may still
/// be releasing `adb.exe` / `AdbWinApi.dll` when the swap starts.
#[cfg(not(store_build))]
fn rename_with_retries(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error)
                if cfg!(windows) && attempt < SWAP_ATTEMPTS && is_transient_lock(&error) =>
            {
                attempt += 1;
                std::thread::sleep(SWAP_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Moves one file, copying it when `from` and `to` live on different volumes.
#[cfg(not(store_build))]
fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device(&error) => {
            fs::copy(from, to).map_err(|error| {
                format!("Could not copy {} to {}: {error}", from.display(), to.display())
            })?;
            fs::remove_file(from).map_err(|error| {
                format!("Could not remove {} after copying it: {error}", from.display())
            })
        }
        Err(error) => Err(format!(
            "Could not move {} to {}: {error}",
            from.display(),
            to.display()
        )),
    }
}

/// Moves the tree at `src` to `dst` (which must not exist yet). Renames the whole directory when
/// possible and falls back to a file-by-file move across volumes; no failure is skipped.
#[cfg(not(store_build))]
fn move_tree(src: &Path, dst: &Path) -> Result<(), String> {
    match fs::rename(src, dst) {
        Ok(()) => return Ok(()),
        Err(error) if is_cross_device(&error) => {}
        Err(error) => {
            return Err(format!(
                "Could not move {} to {}: {error}",
                src.display(),
                dst.display()
            ))
        }
    }
    fs::create_dir_all(dst).map_err(|error| format!("{}: {error}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|error| format!("{}: {error}", src.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            move_tree(&from, &to)?;
        } else {
            move_file(&from, &to)?;
        }
    }
    fs::remove_dir(src).map_err(|error| format!("Could not remove {}: {error}", src.display()))
}

/// Sibling directory that keeps the previous installation while the new one is moved in place.
#[cfg(not(store_build))]
fn backup_dir(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tool".to_string());
    target.with_file_name(format!("{name}.previous"))
}

/// Replaces `target` with the verified tree at `extracted_root`, restoring the previous
/// installation if anything goes wrong so the user never ends up with a half-installed tool.
#[cfg(not(store_build))]
fn swap_into_place(extracted_root: &Path, target: &Path) -> Result<(), String> {
    let backup = backup_dir(target);
    remove_dir(&backup)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let had_previous = target.exists();
    if had_previous {
        rename_with_retries(target, &backup).map_err(|error| {
            format!(
                "Could not replace the current installation in {} (is it still in use?): {error}",
                target.display()
            )
        })?;
    }
    if let Err(error) = move_tree(extracted_root, target) {
        let mut message = error;
        if let Err(cleanup) = remove_dir(target) {
            message.push_str(&format!(
                " (cleanup of {} failed: {cleanup})",
                target.display()
            ));
        }
        if had_previous {
            if let Err(restore) = rename_with_retries(&backup, target) {
                message.push_str(&format!(
                    ". Restoring the previous version failed too ({restore}); it was kept in {}",
                    backup.display()
                ));
            }
        }
        return Err(message);
    }
    if had_previous {
        // Best effort: a leftover backup is removed at the start of the next install.
        let _ = remove_dir(&backup);
    }
    Ok(())
}

#[cfg(all(not(store_build), unix))]
fn ensure_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .map_err(|error| error.to_string())?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions).map_err(|error| error.to_string())
}

#[cfg(all(not(store_build), not(unix)))]
fn ensure_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(store_build)]
pub async fn install_tool(_tool: &str) -> Result<(), String> {
    Err("La versión de la tienda no soporta descargas.".to_string())
}

/// Downloads `tool` and installs it into its managed directory (never into the directory of a
/// system or third-party copy). The archive is unpacked and verified in a staging directory
/// first; only then is the installed copy swapped, with rollback on failure.
#[cfg(not(store_build))]
pub async fn install_tool(tool: &str) -> Result<(), String> {
    let client = client()?;
    let (url, kind) = tool_asset(tool, &client).await?;
    let target = managed_dir(tool);
    let staging = crate::app_paths::cache_dir().join("temp").join(tool);
    remove_dir(&staging)?;
    let archive = download_bytes(&client, &url).await?;

    let executable = {
        let staging = staging.clone();
        let name = executable_name(tool);
        tauri::async_runtime::spawn_blocking(move || -> Result<PathBuf, String> {
            extract(&archive, kind, &staging)?;
            let executable = find_file(&staging, &name)
                .ok_or_else(|| format!("The download does not contain {name}"))?;
            // Zip entries created on Windows carry no Unix mode: make sure the tool can run.
            ensure_executable(&executable)?;
            Ok(executable)
        })
        .await
        .map_err(|error| error.to_string())??
    };
    let extracted_root = executable.parent().unwrap_or(&staging).to_path_buf();

    // A running adb server and the device tracker's adb client keep adb.exe / AdbWinApi.dll
    // locked on Windows, and the tracker would restart the server right after kill-server.
    let stops_adb = tool == "adb"
        || crate::tools::resolve_tool_path("adb").is_some_and(|path| path.starts_with(&target));
    let _tracker_pause = stops_adb.then(crate::adb::TrackerPause::new);
    if stops_adb {
        let _ = crate::adb::kill_server().await;
        // Give the server (and the tracker client being killed) a moment to release the files.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    tauri::async_runtime::spawn_blocking(move || swap_into_place(&extracted_root, &target))
        .await
        .map_err(|error| error.to_string())??;
    remove_dir(&staging)
}

