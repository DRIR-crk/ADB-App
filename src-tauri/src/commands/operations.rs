use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

use crate::adb;
use crate::models::{AndroidUser, KeyboardInputMethod, SystemState};
use crate::tools::{self, ToolsStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSummary {
    pub package_name: String,
    pub display_name: String,
    pub apk_path: String,
    pub system_app: bool,
    pub disabled: bool,
    pub uninstalled: bool,
    pub icon_data_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppPermissionInfo {
    pub name: String,
    pub granted: bool,
    pub runtime: bool,
    pub changeable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppDetailsInfo {
    pub package_name: String,
    pub display_name: String,
    pub apk_path: String,
    pub is_split: bool,
    pub system_app: bool,
    pub disabled: bool,
    pub version_name: String,
    pub version_code: String,
    pub target_sdk: String,
    pub min_sdk: String,
    pub installer: String,
    pub data_dir: String,
    pub code_size_bytes: i64,
    pub data_size_bytes: i64,
    pub cache_size_bytes: i64,
    pub background_mode: String,
    pub permissions: Vec<AppPermissionInfo>,
    pub icon_data_url: String,
    pub install_date: String,
    pub update_date: String,
}

#[derive(Debug, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub permissions: String,
    pub size: i64,
    pub modified: String,
    pub is_directory: bool,
    pub is_link: bool,
    pub link_target: String,
}

#[derive(Debug, Serialize)]
pub struct MediaVolumeState {
    pub level: i32,
    pub maximum: i32,
}

#[derive(Debug, Serialize)]
pub struct ControlState {
    pub brightness: i32,
    pub volume_level: i32,
    pub volume_maximum: i32,
    pub rotation_auto: bool,
    pub rotation: i32,
    pub sound_mode: String,
}

#[derive(Debug, Deserialize)]
pub struct AppInstallOptions {
    pub replace_existing: bool,
    pub grant_runtime_permissions: bool,
    pub bypass_low_target_sdk_block: bool,
}

/// Android package and permission names only contain `[A-Za-z0-9._]`. Names are interpolated
/// into device shell strings, so anything else is refused instead of escaped.
fn validate_android_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 256
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(format!("Invalid package or permission name: {name}"))
    }
}

/// Single-quotes a value for the device shell (`it's` -> `'it'\''s'`).
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn refs(values: &[String]) -> Vec<&str> {
    values.iter().map(String::as_str).collect()
}

fn last_integer(value: &str) -> Option<i32> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .filter_map(|part| part.parse::<i32>().ok())
        .last()
}

fn last_output_line(value: &str) -> &str {
    value.lines().last().unwrap_or("").trim()
}

fn package_set(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .map(|value| value.rsplit_once('=').map_or(value, |(_, package)| package))
        .map(str::trim)
        .map(str::to_string)
        .collect()
}

fn settings_path() -> PathBuf {
    crate::app_paths::config_dir().join("settings.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub cache_enabled: bool,
    pub cache_path: String,
    pub kill_adb_on_exit: bool,
    /// Detect phones that open "Pair device with pairing code" (mDNS) while the app is open.
    pub pairing_detection: bool,
    pub auto_save_screenshots: bool,
    pub material_you_enabled: bool,
    pub material_you_background_tint: bool,
    pub window_effect: String,
    pub theme: String,
    pub language: String,
    pub adb_path: String,
    pub scrcpy_path: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            cache_enabled: true,
            cache_path: String::new(),
            kill_adb_on_exit: true,
            pairing_detection: true,
            auto_save_screenshots: false,
            material_you_enabled: true,
            material_you_background_tint: true,
            window_effect: "system".to_string(),
            theme: String::new(),
            language: String::new(),
            adb_path: String::new(),
            scrcpy_path: String::new(),
        }
    }
}

pub fn read_settings() -> AppSettings {
    let mut settings: AppSettings = fs::read_to_string(settings_path())
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default();

    if crate::app_paths::is_packaged() {
        settings.cache_path.clear();
    } else if !settings.cache_path.trim().is_empty() {
        let path = std::path::Path::new(&settings.cache_path);
        if !path.exists() {
            settings.cache_path = String::new();
            if let Ok(serialized) = serde_json::to_string_pretty(&settings) {
                let _ = fs::create_dir_all(crate::app_paths::config_dir());
                let _ = fs::write(settings_path(), serialized);
            }
        }
    }

    settings
}

#[derive(Serialize)]
pub struct AppSettingsView {
    #[serde(flatten)]
    settings: AppSettings,
    packaged: bool,
    store_build: bool,
}

#[tauri::command]
pub fn get_app_settings() -> AppSettingsView {
    AppSettingsView {
        settings: read_settings(),
        packaged: crate::app_paths::is_packaged(),
        store_build: cfg!(store_build),
    }
}

pub fn write_settings_sync(settings: &AppSettings) -> Result<(), String> {
    fs::create_dir_all(crate::app_paths::config_dir()).map_err(|error| error.to_string())?;
    let serialized = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    fs::write(settings_path(), serialized).map_err(|error| error.to_string())?;
    Ok(())
}

/// Old data directory waiting to be deleted by `close_app` once the app restarts. Kept on the
/// backend so the webview can never ask for an arbitrary directory to be wiped.
static PENDING_OLD_DATA_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Validates a new cache/data location before anything is changed and returns the data
/// directory it will use.
fn validate_new_data_dir(cache_path: &str, old_data_dir: &Path) -> Result<PathBuf, String> {
    let requested = cache_path.trim();
    if !requested.is_empty() && !Path::new(requested).is_dir() {
        return Err(format!("The folder does not exist: {requested}"));
    }
    let planned = crate::app_paths::planned_data_dir((!requested.is_empty()).then_some(requested))
        .ok_or_else(|| "The app paths are not initialized".to_string())?;
    if planned != old_data_dir
        && (planned.starts_with(old_data_dir) || old_data_dir.starts_with(&planned))
    {
        return Err(
            "The new location cannot be inside the current data folder, or contain it".to_string(),
        );
    }
    Ok(planned)
}

#[tauri::command]
pub async fn save_app_settings(
    window: tauri::WebviewWindow,
    mut settings: AppSettings,
) -> Result<Option<String>, String> {
    let old_settings = read_settings();
    let old_data_dir = crate::app_paths::data_dir();

    if crate::app_paths::is_packaged() {
        settings.cache_path.clear();
    }
    // Tool paths are owned by `set_tool_path` and the installer: a stale copy held by the UI
    // must never overwrite them.
    settings.adb_path = old_settings.adb_path.clone();
    settings.scrcpy_path = old_settings.scrcpy_path.clone();

    let path_changed = old_settings.cache_path != settings.cache_path;
    // Validate the new location before touching settings, adb or any file.
    let planned_data_dir = if path_changed {
        Some(validate_new_data_dir(&settings.cache_path, &old_data_dir)?)
    } else {
        None
    };

    write_settings_sync(&settings)?;
    if old_settings.window_effect != settings.window_effect {
        apply_window_effect(&window, &settings.window_effect);
    }
    if old_settings.pairing_detection != settings.pairing_detection {
        crate::commands::wireless::set_background(
            tauri::Manager::app_handle(&window),
            settings.pairing_detection,
        );
    }

    let Some(new_data_dir) = planned_data_dir else {
        return Ok(None);
    };
    let custom_path = |value: &str| (!value.trim().is_empty()).then(|| value.to_string());

    if new_data_dir == old_data_dir {
        crate::app_paths::update_base_path(custom_path(&settings.cache_path).as_deref());
        return Ok(None);
    }

    let _tracker_pause = crate::adb::TrackerPause::new();
    let _ = crate::adb::kill_server().await;
    #[cfg(windows)]
    let _ = crate::process::command("taskkill")
        .args(["/F", "/IM", "scrcpy.exe"])
        .output();
    #[cfg(not(windows))]
    let _ = crate::process::command("killall").arg("scrcpy").output();

    crate::app_paths::update_base_path(custom_path(&settings.cache_path).as_deref());
    // Tool locations depend on the data folder: forget the cached ones so adb is never respawned
    // from the folder that is about to be deleted.
    crate::tools::invalidate_tools_cache();

    if old_data_dir.exists() {
        let moved = fs::create_dir_all(&new_data_dir)
            .map_err(|error| error.to_string())
            .and_then(|_| move_directory_contents(&old_data_dir, &new_data_dir));
        if let Err(error) = moved {
            // Keep using the old location: nothing was deleted, so nothing is lost.
            settings.cache_path = old_settings.cache_path.clone();
            let _ = write_settings_sync(&settings);
            crate::app_paths::update_base_path(custom_path(&old_settings.cache_path).as_deref());
            crate::tools::invalidate_tools_cache();
            return Err(format!("Could not move the app data: {error}"));
        }
    }

    if let Ok(mut pending) = PENDING_OLD_DATA_DIR.lock() {
        *pending = Some(old_data_dir.clone());
    }
    // The app exits right after (`close_app`): keep the tracker from starting an adb server again.
    crate::adb::stop_tracker();
    Ok(Some(old_data_dir.to_string_lossy().into_owned()))
}

#[tauri::command]
pub async fn save_device_wallpaper_to_disk(
    app: tauri::AppHandle,
    serial: String,
    path: String,
) -> Result<(), String> {
    if crate::mock::enabled() {
        return Ok(());
    }

    push_daemons_if_needed(&app, &serial).await?;

    let version = env!("CARGO_PKG_VERSION");
    let jar_name = format!("wallpaper_extractor_{}.jar", version);
    let daemon_device_path = format!("/data/local/tmp/{}", jar_name);

    let result = adb::run_adb_for_serial(
        &serial,
        &[
            "shell",
            &format!(
                "CLASSPATH={} app_process / com.kyro.adbapp.extractwallpaper.WallpaperExtractor --max-res",
                daemon_device_path
            ),
        ],
    )
    .await?;

    if !result.ok() {
        return Err(format!("Failed to extract wallpaper: {}", result.output));
    }

    if let Some(base64) = wallpaper_payload(&result.output) {
        let decoded = STANDARD
            .decode(&base64)
            .map_err(|e| format!("Base64 decode error: {}", e))?;
        std::fs::write(&path, decoded)
            .map_err(|e| format!("Failed to write wallpaper to disk: {}", e))?;
        return Ok(());
    }

    Err("Extractor did not return encoded image.".to_string())
}

/// Base64 payload printed by the wallpaper extractor between its start/end markers, without
/// whitespace. Returns `None` when the markers are missing, out of order or the payload is empty.
fn wallpaper_payload(output: &str) -> Option<String> {
    const START: &str = "WALLPAPER_START";
    const END: &str = "WALLPAPER_END";
    let start = output.find(START)? + START.len();
    let end = output[start..].find(END)? + start;
    let payload: String = output[start..end]
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    (!payload.is_empty()).then_some(payload)
}

#[tauri::command]
pub async fn get_device_wallpaper(app: tauri::AppHandle, serial: String) -> Result<String, String> {
    if crate::mock::enabled() {
        return crate::mock::wallpaper_base64();
    }

    push_daemons_if_needed(&app, &serial).await?;

    let version = env!("CARGO_PKG_VERSION");
    let jar_name = format!("wallpaper_extractor_{}.jar", version);
    let daemon_device_path = format!("/data/local/tmp/{}", jar_name);

    // Ejecutar el jar
    let result = adb::run_adb_for_serial(
        &serial,
        &[
            "shell",
            &format!(
                "CLASSPATH={} app_process / com.kyro.adbapp.extractwallpaper.WallpaperExtractor",
                daemon_device_path
            ),
        ],
    )
    .await?;

    if !result.ok() {
        return Err(format!("Failed to extract wallpaper: {}", result.output));
    }

    wallpaper_payload(&result.output)
        .ok_or_else(|| "Extractor did not return encoded image.".to_string())
}

#[tauri::command]
pub fn set_window_theme(window: tauri::Window, theme: String) {
    let tauri_theme = match theme.as_str() {
        "dark" => Some(tauri::Theme::Dark),
        "light" => Some(tauri::Theme::Light),
        _ => None,
    };
    let _ = window.set_theme(tauri_theme);
}

#[derive(Clone)]
struct AppsCacheEntry {
    created_at: Instant,
    apps: Vec<AppSummary>,
}

static APPS_LIST_CACHE: OnceLock<Mutex<HashMap<String, AppsCacheEntry>>> = OnceLock::new();
static OPERATIONS_ACTIVE: AtomicBool = AtomicBool::new(false);

fn apps_list_cache() -> &'static Mutex<HashMap<String, AppsCacheEntry>> {
    APPS_LIST_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn invalidate_apps_cache(serial: &str) {
    if let Ok(mut cache) = apps_list_cache().lock() {
        cache.remove(serial);
    }
}

fn cached_apps(serial: &str) -> Vec<AppSummary> {
    apps_list_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(serial).map(|entry| entry.apps.clone()))
        .unwrap_or_default()
}

pub fn operations_active() -> bool {
    OPERATIONS_ACTIVE.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn set_operations_active(active: bool) {
    OPERATIONS_ACTIVE.store(active, Ordering::Relaxed);
}

#[tauri::command]
pub fn confirm_close_with_operations(window: tauri::WebviewWindow) -> Result<(), String> {
    OPERATIONS_ACTIVE.store(false, Ordering::Relaxed);
    window.close().map_err(|error| error.to_string())
}

fn update_cached_app(serial: &str, updated: &AppSummary) {
    if let Ok(mut cache) = apps_list_cache().lock() {
        if let Some(entry) = cache.get_mut(serial) {
            if let Some(app) = entry
                .apps
                .iter_mut()
                .find(|app| app.package_name == updated.package_name)
            {
                *app = updated.clone();
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WindowEffectInfo {
    pub platform: String,
    pub mica: bool,
    pub acrylic: bool,
}

#[tauri::command]
pub fn get_window_effect_info() -> WindowEffectInfo {
    #[cfg(target_os = "windows")]
    let build = get_windows_build();
    #[cfg(not(target_os = "windows"))]
    let build = 0;

    WindowEffectInfo {
        platform: std::env::consts::OS.to_string(),
        mica: build >= 22000,
        acrylic: build >= 17763,
    }
}

pub fn apply_window_effect(window: &tauri::WebviewWindow, mode: &str) {
    let effects = window_effects(mode);
    let _ = window.set_effects(
        effects.map(|effects| tauri::utils::config::WindowEffectsConfig {
            effects,
            state: Some(tauri::window::EffectState::Active),
            ..Default::default()
        }),
    );
}

#[cfg(target_os = "windows")]
fn window_effects(mode: &str) -> Option<Vec<tauri::window::Effect>> {
    let build = get_windows_build();
    match mode {
        "disabled" => None,
        "acrylic" if build >= 17763 => Some(vec![tauri::window::Effect::Acrylic]),
        _ if build >= 22000 => Some(vec![
            tauri::window::Effect::Mica,
            tauri::window::Effect::Acrylic,
        ]),
        _ if build >= 17763 => Some(vec![tauri::window::Effect::Acrylic]),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn window_effects(mode: &str) -> Option<Vec<tauri::window::Effect>> {
    if mode == "disabled" {
        None
    } else {
        Some(vec![tauri::window::Effect::Sidebar])
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn window_effects(_mode: &str) -> Option<Vec<tauri::window::Effect>> {
    None
}

#[cfg(target_os = "windows")]
fn get_windows_build() -> u32 {
    #[repr(C)]
    struct RtlOsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform_id: u32,
        csd_version: [u16; 128],
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(version: *mut RtlOsVersionInfo) -> i32;
    }

    let mut version = RtlOsVersionInfo {
        size: std::mem::size_of::<RtlOsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform_id: 0,
        csd_version: [0; 128],
    };

    unsafe {
        if RtlGetVersion(&mut version) == 0 {
            version.build
        } else {
            0
        }
    }
}

/// Exits the app after a data-directory move, deleting the old directory recorded by
/// `save_app_settings` (the `old_data_dir` argument sent by older frontends is ignored).
#[tauri::command]
pub async fn close_app() -> Result<(), String> {
    let old_dir = PENDING_OLD_DATA_DIR
        .lock()
        .ok()
        .and_then(|mut pending| pending.take());
    let identifier = crate::app_paths::identifier();

    // Cleanup runs on its own thread so the webview can be torn down while we retry.
    std::thread::spawn(move || {
        crate::adb::stop_tracker();
        if let Some(old_dir) = old_dir {
            // Only ever delete a directory that really is an app data directory.
            let is_app_dir = !identifier.is_empty()
                && old_dir
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy() == identifier.as_str());
            if is_app_dir {
                for _ in 0..20 {
                    if !old_dir.exists() || std::fs::remove_dir_all(&old_dir).is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
        std::process::exit(0);
    });

    Ok(())
}

/// Copies the app data to its new location. The WebView2 profile (`EBWebView`) is skipped: it is
/// locked while the app runs and is recreated on the next start. Any copy error aborts the move.
/// The source is left in place (the running app still uses it); `close_app` deletes it on exit.
fn move_directory_contents(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() {
        return Ok(());
    }
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((source_dir, target_dir)) = stack.pop() {
        fs::create_dir_all(&target_dir)
            .map_err(|e| format!("Failed to create dir {}: {}", target_dir.display(), e))?;
        let entries = fs::read_dir(&source_dir)
            .map_err(|e| format!("Failed to read dir {}: {}", source_dir.display(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            if source_dir == src && name.to_string_lossy().eq_ignore_ascii_case("EBWebView") {
                continue;
            }
            let file_type = entry.file_type().map_err(|e| e.to_string())?;
            let target = target_dir.join(&name);
            if file_type.is_dir() {
                stack.push((entry.path(), target));
            } else {
                fs::copy(entry.path(), &target)
                    .map_err(|e| format!("Failed to copy {}: {}", entry.path().display(), e))?;
            }
        }
    }
    Ok(())
}

fn application_cache_dir() -> PathBuf {
    crate::app_paths::cache_dir().join("app-icons")
}

#[tauri::command]
pub fn get_default_cache_dir() -> String {
    crate::app_paths::default_data_dir()
        .to_string_lossy()
        .to_string()
}

fn apps_json_path() -> PathBuf {
    application_cache_dir().join("apps.json")
}

fn install_working_dir() -> Result<PathBuf, String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis();
    let path = crate::app_paths::cache_dir()
        .join("temp")
        .join(nonce.to_string());
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn run_local_command(program: &Path, args: &[String]) -> Result<String, String> {
    let output = crate::process::command(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Couldn't run {}: {error}", program.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}").trim().to_string();
    if output.status.success() {
        Ok(combined)
    } else {
        Err(if combined.is_empty() {
            format!("{} finished with error", program.display())
        } else {
            combined
        })
    }
}


fn collect_apks(directory: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(path: &Path, result: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            if path.is_dir() {
                visit(&path, result)?;
            } else if path
                .extension()
                .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case("apk"))
            {
                result.push(path);
            }
        }
        Ok(())
    }
    let mut result = Vec::new();
    visit(directory, &mut result)?;
    result.sort();
    Ok(result)
}

/// Screen densities of split APKs (`config.xxhdpi.apk` -> 480). Longest markers first: `hdpi` is a
/// substring of `xhdpi`, `xxhdpi` and `xxxhdpi`.
const DENSITY_MARKERS: [(&str, i32); 6] = [
    ("xxxhdpi", 640),
    ("xxhdpi", 480),
    ("xhdpi", 320),
    ("hdpi", 240),
    ("mdpi", 160),
    ("ldpi", 120),
];

/// ABIs of split APKs in canonical (hyphenated) form, longest markers first.
const ABI_MARKERS: [&str; 6] = [
    "arm64-v8a",
    "armeabi-v7a",
    "armeabi",
    "x86-64",
    "x86_64",
    "x86",
];

/// True when `needle` appears in `haystack` as a whole token (not glued to other letters/digits).
fn contains_token(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(start, _)| {
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + needle.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

fn apk_density(filename: &str) -> Option<i32> {
    DENSITY_MARKERS
        .iter()
        .find(|(marker, _)| contains_token(filename, marker))
        .map(|(_, density)| *density)
}

/// `arm64_v8a` / `arm64-v8a` -> `arm64-v8a`; `x86_64` -> `x86_64` (adb's spelling for that ABI).
fn canonical_abi(name: &str) -> String {
    let name = name.trim().to_ascii_lowercase().replace('_', "-");
    if name == "x86-64" {
        "x86_64".to_string()
    } else {
        name
    }
}

fn apk_abi(filename: &str) -> Option<String> {
    let normalized = filename.replace('_', "-");
    ABI_MARKERS
        .iter()
        .find(|marker| contains_token(&normalized, &marker.replace('_', "-")))
        .map(|marker| canonical_abi(marker))
}

/// Closest available density to the device's (ties go to the higher one, which scales down).
fn closest_density(device_density: i32, available: &[i32]) -> Option<i32> {
    if device_density <= 0 {
        return None;
    }
    available
        .iter()
        .copied()
        .min_by_key(|density| ((device_density - density).abs(), -density))
}

/// The device's most preferred ABI (`abilist` order) that is present among the split APKs.
fn preferred_abi(device_abis: &[String], available: &[String]) -> Option<String> {
    device_abis
        .iter()
        .map(|abi| canonical_abi(abi))
        .find(|abi| available.contains(abi))
}

fn resolve_install_files(
    serial: &str,
    package_file: &Path,
    working_directory: &Path,
) -> Result<Vec<PathBuf>, String> {
    let extension = package_file
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if extension == "apk" {
        return Ok(vec![package_file.to_path_buf()]);
    }

    let extraction_directory = working_directory.join(
        package_file
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .replace(|character: char| !character.is_ascii_alphanumeric(), "_"),
    );
    fs::create_dir_all(&extraction_directory).map_err(|error| error.to_string())?;

    if matches!(extension.as_str(), "apks" | "apkm" | "xapk" | "zip") {
        // Extracción mediante el crate zip para no depender de comandos nativos
        let file = std::fs::File::open(&package_file)
            .map_err(|e| format!("Failed to open package: {e}"))?;
        let mut archive =
            zip::ZipArchive::new(file).map_err(|e| format!("Invalid ZIP archive: {e}"))?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
            let relative = entry
                .enclosed_name()
                .ok_or_else(|| "Unsafe path in ZIP".to_string())?;
            let output = extraction_directory.join(relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&output).map_err(|e| e.to_string())?;
            } else {
                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let mut outfile = std::fs::File::create(&output).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut outfile).map_err(|e| e.to_string())?;
            }
        }

        let adb_path =
            tools::resolve_tool_path("adb").ok_or_else(|| "ADB is not available".to_string())?;

        // 1. Obtener la arquitectura del dispositivo conectado
        let abi_output = run_local_command(
            &adb_path,
            &[
                "-s".into(),
                serial.to_string(),
                "shell".into(),
                "getprop".into(),
                "ro.product.cpu.abilist".into(),
            ],
        )
        .unwrap_or_default();

        let mut supported_abis: Vec<String> = abi_output
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        // Dispositivos viejos (Android 4.4/5.0) pueden no tener abilist, usamos abi como respaldo
        if supported_abis.is_empty() {
            let fallback_abi = run_local_command(
                &adb_path,
                &[
                    "-s".into(),
                    serial.to_string(),
                    "shell".into(),
                    "getprop".into(),
                    "ro.product.cpu.abi".into(),
                ],
            )
            .unwrap_or_default();
            if !fallback_abi.trim().is_empty() {
                supported_abis.push(fallback_abi.trim().to_lowercase());
            }
        }

        // 2. Obtener el idioma del dispositivo conectado
        let locale_output = run_local_command(
            &adb_path,
            &[
                "-s".into(),
                serial.to_string(),
                "shell".into(),
                "getprop".into(),
                "persist.sys.locale".into(),
            ],
        )
        .unwrap_or_default();

        let mut device_lang = locale_output
            .trim()
            .split('-')
            .next()
            .unwrap_or("")
            .to_lowercase();
        if device_lang.is_empty() {
            let fallback_locale = run_local_command(
                &adb_path,
                &[
                    "-s".into(),
                    serial.to_string(),
                    "shell".into(),
                    "getprop".into(),
                    "ro.product.locale".into(),
                ],
            )
            .unwrap_or_default();
            device_lang = fallback_locale
                .trim()
                .split('-')
                .next()
                .unwrap_or("")
                .to_lowercase();
        }

        // 3. Obtener la densidad de pantalla
        let density_output = run_local_command(
            &adb_path,
            &[
                "-s".into(),
                serial.to_string(),
                "shell".into(),
                "wm".into(),
                "density".into(),
            ],
        )
        .unwrap_or_default();

        let mut device_density = 0;
        for part in density_output.split_whitespace() {
            if let Ok(num) = part.parse::<i32>() {
                device_density = num;
                break;
            }
        }

        let known_language_codes = [
            "af", "sq", "ar", "hy", "az", "eu", "be", "bn", "bs", "bg", "ca", "zh", "hr", "cs",
            "da", "nl", "en", "et", "fi", "fr", "gl", "ka", "de", "el", "gu", "ht", "he", "hi",
            "hu", "is", "id", "it", "ja", "kn", "kk", "km", "ko", "lo", "lv", "lt", "mk", "ms",
            "ml", "mr", "mn", "ne", "no", "fa", "pl", "pt", "pa", "ro", "ru", "sr", "sk", "sl",
            "es", "sw", "sv", "ta", "te", "th", "tr", "uk", "ur", "vi", "zu",
        ];

        let all_apks = collect_apks(&extraction_directory)?;

        // The density and ABI of the device decide which split APKs are installed.
        let apk_names: Vec<String> = all_apks
            .iter()
            .map(|apk| {
                apk.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase()
            })
            .collect();
        let available_densities: Vec<i32> = apk_names.iter().filter_map(|name| apk_density(name)).collect();
        let target_density = closest_density(device_density, &available_densities);
        let available_abis: Vec<String> = apk_names.iter().filter_map(|name| apk_abi(name)).collect();
        // Only the most preferred ABI is installed (the others would just waste storage).
        let target_abi = preferred_abi(&supported_abis, &available_abis);

        let mut filtered_apks = Vec::new();

        // 4. Filtrar los APKs extraídos
        for (apk, filename) in all_apks.into_iter().zip(apk_names) {
            // --- Filtro de Arquitectura (ABI) ---
            let apk_abi_name = apk_abi(&filename);
            let is_abi_split = apk_abi_name.is_some();
            let matches_device_abi = apk_abi_name.is_some() && apk_abi_name == target_abi;

            // --- Filtro de Densidad ---
            let apk_density_value = apk_density(&filename);
            let is_density_split = apk_density_value.is_some();
            let matches_device_density = target_density.is_none() || apk_density_value == target_density;

            // --- Filtro de Idioma ---
            let mut is_lang_split = false;
            let mut matches_device_lang = false;
            for lang in &known_language_codes {
                let dot_lang = format!(".{}.", lang);
                let dash_lang = format!("-{}.", lang);
                let dash_lang_dash = format!("-{}-", lang);
                let dot_lang_dash = format!(".{}-", lang);
                let config_lang = format!("config.{}.apk", lang);

                if filename.contains(&dot_lang)
                    || filename.contains(&dash_lang)
                    || filename.contains(&dash_lang_dash)
                    || filename.contains(&dot_lang_dash)
                    || filename.ends_with(&config_lang)
                {
                    is_lang_split = true;
                    if *lang == device_lang || *lang == "en" {
                        // Se permite inglés como fallback
                        matches_device_lang = true;
                    }
                    break;
                }
            }

            // Mantenemos el APK si no es un split de esa categoría, o si lo es y coincide con el dispositivo
            let abi_ok = !is_abi_split || matches_device_abi;
            let density_ok = !is_density_split || matches_device_density;
            let lang_ok = !is_lang_split || matches_device_lang;

            if abi_ok && density_ok && lang_ok {
                filtered_apks.push(apk);
            }
        }

        if filtered_apks.is_empty() {
            return Err(format!(
                "No compatible APKs found for your device in {}",
                package_file.display()
            ));
        }

        return Ok(filtered_apks);
    } else {
        return Err(format!("Unsupported format: {}", package_file.display()));
    }
}

fn dump_value(output: &str, key: &str) -> String {
    let key_eq = format!("{}=", key);
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(idx) = trimmed.find(&key_eq) {
            // Ensure it is either at the start or preceded by a space to avoid partial matches
            if idx == 0 || trimmed.as_bytes()[idx - 1] == b' ' {
                let rest = &trimmed[idx + key_eq.len()..];
                let value = if key == "versionName" {
                    rest
                } else {
                    rest.split_whitespace().next().unwrap_or("")
                };
                let value = value.trim().trim_matches(['\'', '"', ',']);
                if !value.is_empty() && value != "null" {
                    return value.to_string();
                }
            }
        }
    }
    "-".to_string()
}

fn dump_date_value(output: &str, key: &str) -> String {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix(&format!("{key}=")) {
            let mut parts = value.split_whitespace();
            if let Some(date) = parts.next() {
                if let Some(time) = parts.next() {
                    let time_without_seconds =
                        time.rsplit_once(':').map(|(h_m, _)| h_m).unwrap_or(time);
                    return format!("{} {}", date, time_without_seconds);
                }
                return date.to_string();
            }
        }
    }
    "-".to_string()
}

fn display_name_from_dump(output: &str, fallback: &str) -> String {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("application-label:") {
            let label = value.trim().trim_matches(['\'', '"']);
            if !label.is_empty() {
                return label.to_string();
            }
        }
        if let Some(index) = trimmed.find("nonLocalizedLabel=") {
            let label = trimmed[index + "nonLocalizedLabel=".len()..]
                .split(" icon=")
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(['\'', '"']);
            if !label.is_empty() && label != "null" {
                return label.to_string();
            }
        }
    }
    fallback.to_string()
}

fn parse_changeable_permissions(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("permission:"))
        .map(str::trim)
        .filter(|permission| permission.contains(".permission."))
        .map(str::to_string)
        .collect()
}

fn permission_is_device_fixed(line: &str) -> bool {
    line.contains("SYSTEM_FIXED")
        || line.contains("POLICY_FIXED")
        || line.contains("HARD_RESTRICTED")
}

fn parse_permissions(
    output: &str,
    changeable_permissions: &HashSet<String>,
) -> Vec<AppPermissionInfo> {
    let mut permissions: Vec<AppPermissionInfo> = Vec::new();
    let mut section = "";
    for line in output.lines() {
        let trimmed = line.trim();
        match trimmed {
            "requested permissions:" => {
                section = "requested";
                continue;
            }
            "install permissions:" => {
                section = "install";
                continue;
            }
            "runtime permissions:" => {
                section = "runtime";
                continue;
            }
            _ => {}
        }
        if trimmed.ends_with(':') && !trimmed.contains(".permission.") {
            section = "";
            continue;
        }
        if section.is_empty() || !trimmed.contains(".permission.") {
            continue;
        }
        let name = trimmed.split(':').next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let runtime = section == "runtime"
            || (section == "requested" && changeable_permissions.contains(name));
        let changeable = runtime && !permission_is_device_fixed(trimmed);
        if let Some(permission) = permissions
            .iter_mut()
            .find(|permission| permission.name == name)
        {
            permission.granted =
                permission.granted || trimmed.contains("granted=true") || section == "install";
            permission.runtime = permission.runtime || runtime;
            permission.changeable =
                (permission.changeable || changeable) && !permission_is_device_fixed(trimmed);
            continue;
        }
        permissions.push(AppPermissionInfo {
            name: name.to_string(),
            granted: trimmed.contains("granted=true") || section == "install",
            runtime,
            changeable,
        });
    }
    permissions
}

fn mock_run_device_action(args: &[String]) -> String {
    let command = args.join(" ");
    if command.contains("settings get system screen_brightness") {
        return ["180", "1", "0", "2"].join("\n---ADBAPPSEP---\n");
    }
    "OK".to_string()
}

#[tauri::command]
pub async fn run_device_action(serial: String, args: Vec<String>) -> Result<String, String> {
    if crate::mock::enabled() {
        return Ok(mock_run_device_action(&args));
    }

    if args.is_empty() {
        return Err("No ADB arguments supplied".to_string());
    }
    let arg_refs = refs(&args);
    let result = adb::run_adb_for_serial(&serial, &arg_refs).await?;
    if result.ok() {
        Ok(result.output.trim().to_string())
    } else {
        Err(result.output.trim().to_string())
    }
}

#[tauri::command]
pub async fn run_device_action_batch(
    serial: String,
    prefix_args: Vec<String>,
    paths: Vec<String>,
) -> Result<String, String> {
    if paths.is_empty() {
        return Ok("No paths provided".to_string());
    }

    let mut output = String::new();
    for path in paths {
        let mut args = prefix_args.clone();
        args.push(path);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let result = adb::run_adb_for_serial(&serial, &arg_refs).await?;
        if result.ok() {
            output.push_str(&result.output);
        } else {
            return Err(result.output.trim().to_string());
        }
    }
    Ok(output.trim().to_string())
}

/// Extracts the percentage from adb's sideload progress line: `serving: 'ota.zip'  (~47%)`.
/// The file name may contain digits, parentheses or percent signs, so only the last `(~` counts.
fn parse_sideload_progress(line: &str) -> Option<u32> {
    let rest = &line[line.rfind("(~")? + 2..];
    let percent = rest[..rest.find('%')?].trim();
    percent.parse::<u32>().ok().filter(|value| *value <= 100)
}

/// Reads an adb output stream to the end, emitting `sideload-progress` whenever the progress changes.
async fn read_sideload_stream<R>(mut reader: R, app: tauri::AppHandle) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tauri::Emitter;
    use tokio::io::AsyncReadExt;

    let mut buf = [0u8; 1024];
    let mut full_log = String::new();
    let mut current_line = String::new();
    let mut last_progress = None;
    let mut report = |line: &str| {
        if let Some(progress) = parse_sideload_progress(line) {
            if last_progress != Some(progress) {
                last_progress = Some(progress);
                let _ = app.emit("sideload-progress", progress);
            }
        }
    };

    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                full_log.push_str(&text);
                for c in text.chars() {
                    if c == '\r' || c == '\n' {
                        report(&current_line);
                        current_line.clear();
                    } else {
                        current_line.push(c);
                    }
                }
                // The progress line is refreshed in place, so also look at the unfinished line.
                report(&current_line);
            }
        }
    }
    full_log
}

#[tauri::command]
pub async fn sideload_device(
    app: tauri::AppHandle,
    serial: String,
    file_path: String,
) -> Result<String, String> {
    use tauri::Listener;

    let adb_path = crate::tools::resolve_tool_path("adb")
        .ok_or_else(|| "ADB is not installed. Configure or install it in Settings.".to_string())?;

    let mut cmd = crate::process::tokio_command(adb_path.to_string_lossy().as_ref());
    cmd.arg("-s")
        .arg(&serial)
        .arg("sideload")
        .arg(&file_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start adb sideload: {}", e))?;

    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill().await;
        return Err("Failed to capture adb sideload output".to_string());
    };

    // adb prints `serving: '<file>'  (~47%)` with carriage returns, on stdout or stderr
    // depending on the version: follow both.
    let stdout_handle = tokio::spawn(read_sideload_stream(stdout, app.clone()));
    let stderr_handle = tokio::spawn(read_sideload_stream(stderr, app.clone()));

    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let cancel_tx = std::sync::Mutex::new(Some(cancel_tx));
    let cancel_id = app.listen("cancel-sideload", move |_| {
        if let Some(tx) = cancel_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    });

    let status_result = tokio::select! {
        res = child.wait() => res,
        _ = &mut cancel_rx => {
            let _ = child.kill().await;
            app.unlisten(cancel_id);
            return Err("Cancelled by user".to_string());
        }
    };

    app.unlisten(cancel_id);

    let status = status_result.map_err(|e| format!("Failed to wait for adb sideload: {}", e))?;

    let out = stdout_handle.await.unwrap_or_default();
    let err = stderr_handle.await.unwrap_or_default();
    let combined = format!("{}\n{}", out, err).trim().to_string();

    if status.success() {
        Ok("Success".to_string())
    } else {
        Err(combined)
    }
}

async fn run_system_query(serial: &str, args: &[&str]) -> Result<String, String> {
    let result = adb::run_adb_for_serial(serial, args).await?;
    if result.ok() {
        Ok(result.output.trim().to_string())
    } else {
        Err(result.output.trim().to_string())
    }
}

fn parse_system_users(output: &str, current_user_id: i32) -> Vec<AndroidUser> {
    let mut users = output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let start = trimmed.find("UserInfo{")? + 9;
            let end = trimmed[start..].find('}')?;
            let content = &trimmed[start..start + end];
            let suffix = &trimmed[start + end + 1..];

            let mut parts = content.split(':');
            let id = parts.next()?.parse::<i32>().ok()?;
            let name = parts.next()?.trim().to_string();

            Some(AndroidUser {
                id,
                name,
                is_running: id == current_user_id
                    || suffix.to_ascii_lowercase().contains("running"),
            })
        })
        .collect::<Vec<_>>();
    users.sort_by_key(|user| user.id);
    users
}

fn parse_keyboard_ids(output: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let candidate = if let Some(id) = line.strip_prefix("mId=") {
            Some(id.trim())
        } else if line.contains('/') && !line.contains(' ') {
            Some(line)
        } else {
            None
        };
        if let Some(id) = candidate {
            if !ids.iter().any(|current| current == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

fn keyboard_package_name(id: &str) -> &str {
    id.split_once('/').map_or(id, |(package, _)| package)
}

#[tauri::command]
pub async fn get_system_state(
    app: tauri::AppHandle,
    serial: String,
) -> Result<SystemState, String> {
    if crate::mock::enabled() {
        return Ok(crate::mock::system_state());
    }

    let all_keyboards_query = async {
        match run_system_query(&serial, &["shell", "ime", "list", "-a", "-s"]).await {
            Ok(output) => Ok(output),
            Err(_) => run_system_query(&serial, &["shell", "ime", "list", "-a"]).await,
        }
    };
    let (
        users_output,
        current_user_output,
        app_languages_output,
        overlays_output,
        all_keyboards_output,
        enabled_keyboards_output,
        current_keyboard_id,
        captive_portal_mode_output,
    ) = tokio::join!(
        run_system_query(&serial, &["shell", "pm", "list", "users"]),
        run_system_query(&serial, &["shell", "am", "get-current-user"]),
        run_system_query(
            &serial,
            &[
                "shell",
                "settings",
                "get",
                "global",
                "settings_app_locale_opt_in_enabled",
            ],
        ),
        run_system_query(&serial, &["shell", "cmd", "overlay", "list"]),
        all_keyboards_query,
        run_system_query(&serial, &["shell", "ime", "list", "-s"]),
        run_system_query(
            &serial,
            &["shell", "settings", "get", "secure", "default_input_method"],
        ),
        run_system_query(
            &serial,
            &["shell", "settings", "get", "global", "captive_portal_mode"],
        ),
    );
    // One unsupported sub-query (e.g. `cmd overlay` before Android 8) must not hide the rest:
    // the error is only reported when the device does not answer anything at all.
    let answered = [
        &users_output,
        &current_user_output,
        &app_languages_output,
        &overlays_output,
        &all_keyboards_output,
        &enabled_keyboards_output,
        &current_keyboard_id,
        &captive_portal_mode_output,
    ]
    .iter()
    .any(|result| result.is_ok());
    if !answered {
        return Err(users_output.err().unwrap_or_default());
    }
    let users_output = users_output.unwrap_or_default();
    let current_user_output = current_user_output.unwrap_or_default();
    let app_languages_output = app_languages_output.unwrap_or_default();
    let overlays_output = overlays_output.unwrap_or_default();
    let all_keyboards_output = all_keyboards_output.unwrap_or_default();
    let enabled_keyboards_output = enabled_keyboards_output.unwrap_or_default();
    let current_keyboard_id = current_keyboard_id.unwrap_or_default();
    let captive_portal_mode_output = captive_portal_mode_output.unwrap_or_default();

    let current_user_id = last_integer(&current_user_output).unwrap_or(-1);
    let mut all_keyboard_ids = parse_keyboard_ids(&all_keyboards_output);
    let enabled_keyboard_ids = parse_keyboard_ids(&enabled_keyboards_output);
    if !current_keyboard_id.is_empty()
        && !all_keyboard_ids
            .iter()
            .any(|keyboard| keyboard == &current_keyboard_id)
    {
        all_keyboard_ids.push(current_keyboard_id.clone());
    }

    let keyboard_packages = all_keyboard_ids
        .iter()
        .map(|id| keyboard_package_name(id).to_string())
        .collect::<HashSet<_>>();
    let mut apps = cached_apps(&serial);
    if keyboard_packages.iter().any(|package| {
        !apps
            .iter()
            .any(|app| &app.package_name == package && app.display_name != app.package_name)
    }) {
        if let Ok(loaded_apps) = list_apps(serial.clone(), None).await {
            apps = loaded_apps;
        }
    }

    let mut keyboard_labels = apps
        .iter()
        .filter(|app| {
            keyboard_packages.contains(&app.package_name) && app.display_name != app.package_name
        })
        .map(|app| (app.package_name.clone(), app.display_name.clone()))
        .collect::<HashMap<_, _>>();

    let mut keyboard_requests = Vec::new();
    for package in keyboard_packages.clone() {
        if keyboard_labels.contains_key(&package) {
            continue;
        }
        if let Some(app) = apps.iter().find(|app| app.package_name == package) {
            keyboard_requests.push(AppSummaryRequest {
                package_name: app.package_name.clone(),
                apk_path: app.apk_path.clone(),
                system_app: app.system_app,
                disabled: app.disabled,
                uninstalled: app.uninstalled,
            });
        }
    }

    if !keyboard_requests.is_empty() {
        if let Ok(summaries) =
            enrich_app_summaries(app.clone(), serial.clone(), keyboard_requests).await
        {
            for summary in summaries {
                keyboard_labels.insert(summary.package_name, summary.display_name);
            }
        }
    }

    Ok(SystemState {
        users: parse_system_users(&users_output, current_user_id),
        current_user_id,
        gestural_navigation: overlays_output.lines().any(|line| {
            line.contains("com.android.internal.systemui.navbar.gestural")
                && (line.trim_start().starts_with("[x]") || line.trim_start().starts_with("[X]"))
        }),
        app_languages_enabled: matches!(
            app_languages_output.trim().to_ascii_lowercase().as_str(),
            "false" | "0"
        ),
        captive_portal_mode: match captive_portal_mode_output.trim() {
            "null" | "" => None,
            mode => Some(mode.to_string()),
        },
        keyboards: all_keyboard_ids
            .into_iter()
            .map(|id| KeyboardInputMethod {
                label: keyboard_labels
                    .get(keyboard_package_name(&id))
                    .cloned()
                    .unwrap_or_else(|| keyboard_package_name(&id).to_string()),
                enabled: enabled_keyboard_ids.iter().any(|keyboard| keyboard == &id),
                is_default: id == current_keyboard_id,
                id,
            })
            .collect(),
        current_keyboard_id,
    })
}

#[tauri::command]
pub async fn set_device_dark_mode(
    serial: String,
    enabled: bool,
) -> Result<(), crate::models::AppError> {
    if crate::mock::enabled() {
        return Ok(());
    }

    let mode = if enabled { "yes" } else { "no" };
    let command =
        adb::run_adb_for_serial(&serial, &["shell", "cmd", "uimode", "night", mode]).await?;

    if !command.ok() {
        let value = if enabled { "2" } else { "1" };
        let fallback = adb::run_adb_for_serial(
            &serial,
            &["shell", "settings", "put", "secure", "ui_night_mode", value],
        )
        .await?;
        if !fallback.ok() {
            return Err(fallback.output.trim().into());
        }
    }

    let current = adb::run_adb_for_serial(&serial, &["shell", "cmd", "uimode", "night"]).await?;
    let expected = if enabled { "yes" } else { "no" };
    if current.ok() && current.output.to_ascii_lowercase().contains(expected) {
        Ok(())
    } else {
        Err(format!("Failed to verify {} mode", expected).into())
    }
}

/// `AudioManager.getStreamVolume(3) -> 7` (`cmd audio`, Android 16+) -> 7
fn parse_cmd_audio_value(output: &str) -> Option<i32> {
    output
        .split_once("->")
        .and_then(|(_, value)| value.trim().parse().ok())
}

/// `[v] volume is 7 in range [0..15]` (`media volume --get`, every Android version) -> (7, 15)
fn parse_media_volume(output: &str) -> Option<(i32, i32)> {
    let digits = |text: &str| {
        text.trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<i32>()
            .ok()
    };
    let rest = output.split("volume is ").nth(1)?;
    Some((digits(rest)?, digits(rest.split("..").nth(1)?)?))
}

/// Current and maximum media volume. `cmd audio get-stream-volume` only exists on Android 16+
/// and prints an error without digits on older versions, so `media volume` is the fallback.
fn resolve_media_volume(level_output: &str, max_output: &str, media_output: &str) -> Option<(i32, i32)> {
    match (parse_cmd_audio_value(level_output), parse_cmd_audio_value(max_output)) {
        (Some(level), Some(maximum)) if maximum > 0 => Some((level, maximum)),
        _ => parse_media_volume(media_output).filter(|(_, maximum)| *maximum > 0),
    }
}

const MEDIA_VOLUME_SCRIPT: &str = "cmd audio get-stream-volume 3 2>/dev/null; echo '---ADBAPPSEP---'; cmd audio get-max-volume 3 2>/dev/null; echo '---ADBAPPSEP---'; media volume --stream 3 --get 2>/dev/null";

async fn read_media_volume(serial: &str) -> Result<Option<(i32, i32)>, String> {
    let result = adb::run_adb_for_serial(serial, &["shell", MEDIA_VOLUME_SCRIPT]).await?;
    let mut parts = result.output.split("---ADBAPPSEP---");
    Ok(resolve_media_volume(
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    ))
}

#[tauri::command]
pub async fn get_media_volume(serial: String) -> Result<MediaVolumeState, String> {
    if crate::mock::enabled() {
        return Ok(crate::mock::media_volume());
    }

    let (level, maximum) = read_media_volume(&serial).await?.unwrap_or((7, 15));
    Ok(MediaVolumeState { level, maximum })
}

#[tauri::command]
pub async fn get_control_state(serial: String) -> Result<ControlState, String> {
    if crate::mock::enabled() {
        let volume = crate::mock::media_volume();
        return Ok(ControlState {
            brightness: 128,
            volume_level: volume.level,
            volume_maximum: volume.maximum,
            rotation_auto: true,
            rotation: 0,
            sound_mode: "NORMAL".to_string(),
        });
    }

    let script = "\
        settings get system screen_brightness 2>/dev/null; echo '---ADBAPPSEP---'; \
        cmd audio get-stream-volume 3 2>/dev/null; echo '---ADBAPPSEP---'; \
        cmd audio get-max-volume 3 2>/dev/null; echo '---ADBAPPSEP---'; \
        media volume --stream 3 --get 2>/dev/null; echo '---ADBAPPSEP---'; \
        settings get system accelerometer_rotation 2>/dev/null; echo '---ADBAPPSEP---'; \
        settings get system user_rotation 2>/dev/null; echo '---ADBAPPSEP---'; \
        settings get global mode_ringer 2>/dev/null\
    ";
    let result = adb::run_adb_for_serial(&serial, &["shell", script]).await?;
    if !result.ok() && !result.output.contains("---ADBAPPSEP---") {
        return Err(result.output);
    }

    let mut parts = result.output.split("---ADBAPPSEP---");
    let brightness = last_output_line(parts.next().unwrap_or(""))
        .parse::<i32>()
        .unwrap_or(128)
        .clamp(0, 255);
    let level_output = parts.next().unwrap_or("");
    let max_output = parts.next().unwrap_or("");
    let media_output = parts.next().unwrap_or("");
    let (volume_level, volume_maximum) =
        resolve_media_volume(level_output, max_output, media_output).unwrap_or((7, 15));
    let volume_level = volume_level.max(0);
    let volume_maximum = volume_maximum.max(1);
    let rotation_auto_value = last_output_line(parts.next().unwrap_or(""));
    let rotation = last_output_line(parts.next().unwrap_or(""))
        .parse::<i32>()
        .unwrap_or(0)
        .clamp(0, 3);
    let sound_mode = match last_output_line(parts.next().unwrap_or("")) {
        "0" => "SILENT",
        "1" => "VIBRATE",
        _ => "NORMAL",
    }
    .to_string();

    Ok(ControlState {
        brightness,
        volume_level: volume_level.min(volume_maximum),
        volume_maximum,
        rotation_auto: rotation_auto_value == "1"
            || rotation_auto_value == "null"
            || rotation_auto_value.is_empty(),
        rotation,
        sound_mode,
    })
}

#[tauri::command]
pub async fn set_media_volume(serial: String, volume: i32) -> Result<String, String> {
    if crate::mock::enabled() {
        return Ok(format!("Media volume: {}", volume.clamp(0, 25)));
    }

    let maximum = read_media_volume(&serial)
        .await?
        .map_or(30, |(_, maximum)| maximum.max(1));
    let safe_volume = volume.clamp(0, maximum);
    let value = safe_volume.to_string();
    let commands: [&[&str]; 4] = [
        &["shell", "cmd", "audio", "set-volume", "3", &value],
        &[
            "shell",
            "cmd",
            "media_session",
            "volume",
            "--stream",
            "3",
            "--set",
            &value,
        ],
        &["shell", "media", "volume", "--stream", "3", "--set", &value],
        &["shell", "settings", "put", "system", "volume_music", &value],
    ];
    let mut last_output = String::new();

    for (index, command) in commands.into_iter().enumerate() {
        let result = adb::run_adb_for_serial(&serial, command).await?;
        if result.ok() {
            if index == 0 {
                // `cmd audio set-volume` can exit 0 without applying: verify, else try the next way.
                let current = read_media_volume(&serial).await?;
                if current.map(|(level, _)| level) != Some(safe_volume) {
                    last_output = result.output;
                    continue;
                }
            }
            return Ok(format!("Media volume: {safe_volume}"));
        }
        last_output = result.output;
    }

    Err(if last_output.trim().is_empty() {
        "Could not apply the media volume".to_string()
    } else {
        last_output
    })
}

#[tauri::command]
pub async fn list_apps(
    serial: String,
    force_refresh: Option<bool>,
) -> Result<Vec<AppSummary>, String> {
    if crate::mock::enabled() {
        return Ok(Vec::new());
    }

    if !force_refresh.unwrap_or(false) {
        if let Ok(cache) = apps_list_cache().lock() {
            if let Some(entry) = cache.get(&serial) {
                if entry.created_at.elapsed() < Duration::from_secs(90) {
                    return Ok(entry.apps.clone());
                }
            }
        }
    }

    let cache_dir = application_cache_dir();
    let legacy_details = cache_dir.parent().unwrap_or(&cache_dir).join("app-details");
    if legacy_details.exists() {
        let _ = fs::remove_dir_all(legacy_details);
    }

    let script = "pm list packages -f -u; echo '---ADBAPPSEP---'; pm list packages -s; echo '---ADBAPPSEP---'; pm list packages -d; echo '---ADBAPPSEP---'; pm list packages -f || true";
    let script_result = adb::run_adb_for_serial(&serial, &["shell", script]).await?;

    if !script_result.ok() {
        return Err(script_result.output);
    }

    let mut parts = script_result.output.split("---ADBAPPSEP---");
    let result_out = parts.next().unwrap_or("").trim();
    let system_out = parts.next().unwrap_or("").trim();
    let disabled_out = parts.next().unwrap_or("").trim();
    let installed_out = parts.next().unwrap_or("").trim();

    let system_packages = package_set(system_out);
    let disabled_packages = package_set(disabled_out);

    // Parse installed packages to figure out which ones are uninstalled
    let mut installed_packages = HashSet::new();
    for line in installed_out.lines() {
        if let Some(value) = line.trim().strip_prefix("package:") {
            if let Some((_, package_name)) = value.rsplit_once('=') {
                installed_packages.insert(package_name.to_string());
            }
        }
    }

    let mut apps = result_out
        .lines()
        .filter_map(|line| {
            let value = line.trim().strip_prefix("package:")?;
            let (apk_path, package_name) = value.rsplit_once('=')?;
            Some(AppSummary {
                package_name: package_name.to_string(),
                display_name: package_name.to_string(),
                apk_path: apk_path.to_string(),
                system_app: system_packages.contains(package_name),
                disabled: disabled_packages.contains(package_name),
                uninstalled: !installed_packages.contains(package_name),
                icon_data_url: String::new(),
            })
        })
        .collect::<Vec<_>>();
    let cache_path = apps_json_path();
    let cached_apps: std::collections::HashMap<String, String> = fs::read_to_string(&cache_path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default();

    for app in &mut apps {
        if let Some(display_name) = cached_apps.get(&app.package_name) {
            app.display_name = display_name.clone();
            let icon_path = application_cache_dir().join(format!("{}.webp", app.package_name));
            if icon_path.exists() {
                app.icon_data_url = if cfg!(target_os = "windows") {
                    format!("http://adbapp.localhost/icon/{}", app.package_name)
                } else {
                    format!("adbapp://localhost/icon/{}", app.package_name)
                };
            } else {
                app.icon_data_url = "none".to_string();
            }
        }
    }
    apps.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });
    if let Ok(mut cache) = apps_list_cache().lock() {
        cache.insert(
            serial,
            AppsCacheEntry {
                created_at: Instant::now(),
                apps: apps.clone(),
            },
        );
    }
    Ok(apps)
}

#[derive(Deserialize, Serialize)]
pub struct AppSummaryRequest {
    pub package_name: String,
    pub apk_path: String,
    pub system_app: bool,
    pub disabled: bool,
    pub uninstalled: bool,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct DaemonResponse {
    package: Option<String>,
    label: Option<String>,
    icon: Option<String>,
    error: Option<String>,
    dataSize: Option<i64>,
    cacheSize: Option<i64>,
}

static PUSHED_DAEMONS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// One lock per device serial: concurrent callers (details, wallpaper, app list...) must not
/// push the same helper jars at the same time.
static DAEMON_PUSH_LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

pub async fn push_daemons_if_needed(app: &tauri::AppHandle, serial: &str) -> Result<(), String> {
    use tauri::Manager;
    let daemons_cache =
        PUSHED_DAEMONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let is_pushed = || {
        daemons_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(serial)
    };
    if is_pushed() {
        return Ok(());
    }

    let device_lock = DAEMON_PUSH_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(serial.to_string())
        .or_default()
        .clone();
    let _guard = device_lock.lock().await;
    // Another caller may have finished the push while we were waiting for the lock.
    if is_pushed() {
        return Ok(());
    }

    let version = env!("CARGO_PKG_VERSION");

    // --- info_apps.jar ---
    let info_apps_local = app
        .path()
        .resolve(
            "tools/java/info_apps.jar",
            tauri::path::BaseDirectory::Resource,
        )
        .map_err(|e| e.to_string())?;
    let info_apps_device = format!("/data/local/tmp/info_apps_{}.jar", version);
    let info_apps_size = std::fs::metadata(&info_apps_local)
        .map_err(|e| e.to_string())?
        .len() as usize;

    let check_info =
        adb::run_adb_for_serial(serial, &["shell", "ls", "-l", &info_apps_device]).await;
    let needs_push_info = match check_info {
        Ok(res) if res.ok() && !res.output.contains("No such file") => {
            let size_str = res.output.split_whitespace().nth(4).unwrap_or("");
            size_str.parse::<usize>().unwrap_or(0) != info_apps_size
        }
        _ => true,
    };

    if needs_push_info {
        let _ = adb::run_adb_for_serial(
            serial,
            &["shell", "rm", "-f", "/data/local/tmp/info_apps*.jar"],
        )
        .await;
        let pushed = adb::run_adb_for_serial(
            serial,
            &[
                "push",
                &info_apps_local.to_string_lossy(),
                &info_apps_device,
            ],
        )
        .await?;
        if !pushed.ok() {
            return Err(format!(
                "Could not copy the helper to the device: {}",
                pushed.output.trim()
            ));
        }
        let _ =
            adb::run_adb_for_serial(serial, &["shell", "chmod", "777", &info_apps_device]).await;
    }

    // --- wallpaper_extractor.jar ---
    let wallpaper_local = app
        .path()
        .resolve(
            "tools/java/wallpaper_extractor.jar",
            tauri::path::BaseDirectory::Resource,
        )
        .map_err(|e| e.to_string())?;
    let wallpaper_device = format!("/data/local/tmp/wallpaper_extractor_{}.jar", version);
    let wallpaper_size = std::fs::metadata(&wallpaper_local)
        .map_err(|e| e.to_string())?
        .len() as usize;

    let check_wallpaper =
        adb::run_adb_for_serial(serial, &["shell", "ls", "-l", &wallpaper_device]).await;
    let needs_push_wallpaper = match check_wallpaper {
        Ok(res) if res.ok() && !res.output.contains("No such file") => {
            let size_str = res.output.split_whitespace().nth(4).unwrap_or("");
            size_str.parse::<usize>().unwrap_or(0) != wallpaper_size
        }
        _ => true,
    };

    if needs_push_wallpaper {
        let _ = adb::run_adb_for_serial(
            serial,
            &[
                "shell",
                "rm",
                "-f",
                "/data/local/tmp/wallpaper_extractor*.jar",
            ],
        )
        .await;
        let pushed = adb::run_adb_for_serial(
            serial,
            &[
                "push",
                &wallpaper_local.to_string_lossy(),
                &wallpaper_device,
            ],
        )
        .await?;
        if !pushed.ok() {
            return Err(format!(
                "Could not copy the helper to the device: {}",
                pushed.output.trim()
            ));
        }
    }

    // Only remembered once both helpers are really on the device, so a failed push is retried.
    daemons_cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(serial.to_string());

    Ok(())
}

#[tauri::command]
pub async fn enrich_app_summaries(
    app: tauri::AppHandle,
    serial: String,
    requests: Vec<AppSummaryRequest>,
) -> Result<Vec<AppSummary>, String> {
    let mut results = Vec::new();

    let needs_daemon = requests;
    if needs_daemon.is_empty() {
        return Ok(results);
    }

    for request in &needs_daemon {
        validate_android_name(&request.package_name)?;
    }
    push_daemons_if_needed(&app, &serial).await?;

    let version = env!("CARGO_PKG_VERSION");
    let jar_name = format!("info_apps_{}.jar", version);
    let daemon_device_path = format!("/data/local/tmp/{}", jar_name);

    // 3. Run daemon with all packages
    let package_names: Vec<String> = needs_daemon
        .iter()
        .map(|r| r.package_name.clone())
        .collect();
    let joined_packages = package_names.join(" ");

    let result = adb::run_adb_for_serial(
        &serial,
        &[
            "shell",
            &format!(
                "CLASSPATH={} app_process / com.kyro.adbapp.extractapktool.Main {}",
                daemon_device_path, joined_packages
            ),
        ],
    )
    .await?;

    if !result.ok() {
        return Err(format!("Daemon execution failed: {}", result.output));
    }

    // JSON array could be on multiple lines or a single line.
    let daemon_output = result.output.trim();

    // Find the exact boundaries of the JSON array to ignore any Android linker warnings or extra logs
    let json_start = daemon_output.find('[').unwrap_or(0);
    let json_end = daemon_output
        .rfind(']')
        .unwrap_or_else(|| daemon_output.len().saturating_sub(1));

    let clean_json = if json_start <= json_end && json_end < daemon_output.len() {
        &daemon_output[json_start..=json_end]
    } else {
        daemon_output
    };

    // A label with a raw control character (tab, CR...) makes the helper's JSON invalid for the
    // whole batch; control characters are never meaningful in a label, so flatten them.
    let clean_json: String = clean_json
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    // On failure every package stays "unresolved" (empty icon, nothing cached) and is retried
    // later instead of being remembered as "no label, no icon".
    let parsed_responses: Vec<DaemonResponse> =
        serde_json::from_str(&clean_json).unwrap_or_default();

    let settings = read_settings();
    let cache_path = apps_json_path();
    let mut cached_apps: std::collections::HashMap<String, String> =
        fs::read_to_string(&cache_path)
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default();

    let app_dir = application_cache_dir();
    if !app_dir.exists() {
        let _ = fs::create_dir_all(&app_dir);
    }

    // 4. Map responses back to requests
    for req in needs_daemon {
        let mut display_name = req.package_name.clone();
        let mut icon_data_url = String::new();

        // Find matching response
        let mut resolved = false;
        if let Some(resp) = parsed_responses
            .iter()
            .find(|r| r.package.as_deref() == Some(&req.package_name))
        {
            if resp.error.is_none() {
                resolved = true;
                display_name = resp
                    .label
                    .clone()
                    .unwrap_or_else(|| req.package_name.clone());
                icon_data_url = resp.icon.clone().unwrap_or_default();
            }
        }
        // Empty = unresolved (retry later); "none" = resolved but the app has no icon.
        let mut protocol_url = if resolved { "none".to_string() } else { String::new() };

        if !icon_data_url.is_empty() {
            if settings.cache_enabled {
                let base64_str = icon_data_url
                    .strip_prefix("data:image/webp;base64,")
                    .unwrap_or(&icon_data_url);
                if let Ok(bytes) = STANDARD.decode(base64_str) {
                    let icon_path = app_dir.join(format!("{}.webp", req.package_name));
                    let _ = fs::write(&icon_path, bytes);
                }
                protocol_url = if cfg!(target_os = "windows") {
                    format!("http://adbapp.localhost/icon/{}", req.package_name)
                } else {
                    format!("adbapp://localhost/icon/{}", req.package_name)
                };
            } else {
                protocol_url = icon_data_url.clone();
            }
        }

        if settings.cache_enabled && resolved {
            cached_apps.insert(req.package_name.clone(), display_name.clone());
        }

        let summary = AppSummary {
            package_name: req.package_name,
            display_name,
            apk_path: req.apk_path,
            system_app: req.system_app,
            disabled: req.disabled,
            uninstalled: req.uninstalled,
            icon_data_url: protocol_url,
        };
        update_cached_app(&serial, &summary);
        results.push(summary);
    }

    if settings.cache_enabled {
        if let Ok(serialized) = serde_json::to_string(&cached_apps) {
            let _ = fs::write(cache_path, serialized);
        }
    }

    Ok(results)
}

#[tauri::command]
pub async fn install_application_packages(
    serial: String,
    files: Vec<String>,
    options: AppInstallOptions,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if files.is_empty() {
            return Err("Select at least one package to install".to_string());
        }
        let adb_path =
            tools::resolve_tool_path("adb").ok_or_else(|| "ADB is not available".to_string())?;
        let working_directory = install_working_dir()?;
        let mut log = Vec::new();
        let mut any_failed = false;

        for file in files {
            let package_file = PathBuf::from(&file);
            if !package_file.is_file() {
                any_failed = true;
                log.push(format!(
                    "ERROR · Does not exist: {}",
                    package_file.display()
                ));
                continue;
            }
            log.push(format!(
                "Preparing {}...",
                package_file
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ));
            match resolve_install_files(&serial, &package_file, &working_directory) {
                Ok(apks) => {
                    let mut args = vec!["-s".to_string(), serial.clone()];
                    args.push(if apks.len() > 1 {
                        "install-multiple".into()
                    } else {
                        "install".into()
                    });
                    if options.replace_existing {
                        args.push("-r".into());
                    }
                    if options.grant_runtime_permissions {
                        args.push("-g".into());
                    }
                    if options.bypass_low_target_sdk_block {
                        args.push("--bypass-low-target-sdk-block".into());
                    }
                    args.extend(apks.iter().map(|apk| apk.to_string_lossy().into_owned()));
                    match run_local_command(&adb_path, &args) {
                        Ok(output) => log.push(format!(
                            "OK · {}{}\n{}",
                            package_file
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy(),
                            if apks.len() > 1 {
                                format!(" ({} APKs)", apks.len())
                            } else {
                                String::new()
                            },
                            output
                        )),
                        Err(error) => {
                            any_failed = true;
                            log.push(format!(
                                "ERROR · {}\n{}",
                                package_file
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy(),
                                error
                            ))
                        }
                    }
                }
                Err(error) => {
                    any_failed = true;
                    log.push(format!(
                        "ERROR · {}\n{}",
                        package_file
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        error
                    ))
                }
            }
        }
        let _ = fs::remove_dir_all(&working_directory);
        invalidate_apps_cache(&serial);
        // The job queue turns `Ok` into a green "success": failures must come back as errors.
        if any_failed {
            Err(log.join("\n\n"))
        } else {
            Ok(log.join("\n\n"))
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn reinstall_app(serial: String, package_name: String) -> Result<String, String> {
    validate_android_name(&package_name)?;
    let result = adb::run_adb_for_serial(
        &serial,
        &["shell", "cmd", "package", "install-existing", &package_name],
    )
    .await?;
    if result.ok() && result.output.contains("installed for user") {
        invalidate_apps_cache(&serial);
        Ok(format!("{} reinstalled successfully", package_name))
    } else {
        Err(result.output)
    }
}

#[tauri::command]
pub async fn get_app_details(
    serial: String,
    package_name: String,
) -> Result<AppDetailsInfo, String> {
    validate_android_name(&package_name)?;
    let script = format!(
        "PKG='{pkg}'; \
         dumpsys package \"$PKG\"; \
         echo '---ADBAPPSEP---'; \
         pm path \"$PKG\"; \
         echo '---ADBAPPSEP---'; \
         pm list packages -s \"$PKG\"; \
         echo '---ADBAPPSEP---'; \
         pm list packages -d \"$PKG\"; \
         echo '---ADBAPPSEP---'; \
         cmd appops get \"$PKG\"; \
         echo '---ADBAPPSEP---'; \
         pm list permissions -g -d; \
         echo '---ADBAPPSEP---'; \
         APK=$(pm path \"$PKG\" | sed 's/package://' | head -n 1); \
         if [ -z \"$APK\" ]; then APK=\"/dev/null\"; fi; \
         du -sk \"$APK\" 2>/dev/null || true",
        pkg = package_name
    );

    let result = adb::run_adb_for_serial(&serial, &["shell", &script]).await?;
    if !result.ok() {
        return Err(result.output);
    }

    let mut parts = result.output.split("---ADBAPPSEP---");
    let dump_out = parts.next().unwrap_or("").trim();
    let paths_out = parts.next().unwrap_or("").trim();
    let system_out = parts.next().unwrap_or("").trim();
    let disabled_out = parts.next().unwrap_or("").trim();
    let appops_out = parts.next().unwrap_or("").trim();
    let changeable_permissions = parse_changeable_permissions(parts.next().unwrap_or(""));
    let du_out = parts.next().unwrap_or("").trim();

    let apk_paths: Vec<String> = paths_out
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .map(|s| s.to_string())
        .collect();
    let is_split = apk_paths.len() > 1;
    let apk_path = apk_paths
        .first()
        .cloned()
        .unwrap_or_else(|| "-".to_string());

    let data_dir = dump_value(&dump_out, "dataDir");

    let safe_apk = if apk_path.is_empty() || apk_path == "-" {
        "/dev/null".to_string()
    } else {
        apk_path.clone()
    };

    let mut code_size_bytes: i64 = -1;

    for line in du_out.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let kb = parts[0].parse::<i64>().ok().unwrap_or(-1);
            let bytes = if kb >= 0 { kb * 1024 } else { -1 };
            let path = parts[1..].join(" ");
            if path == safe_apk {
                code_size_bytes = bytes;
            }
        }
    }
    if safe_apk == "/dev/null" {
        code_size_bytes = -1;
    }

    let data_size_bytes = -1; // This is now loaded asynchronously via Java daemon
    let cache_size_bytes = -1; // This is now loaded asynchronously via Java daemon
    let background_mode = if appops_out.contains("RUN_ANY_IN_BACKGROUND: allow") {
        "unrestricted"
    } else if appops_out.contains("RUN_ANY_IN_BACKGROUND: ignore")
        || appops_out.contains("RUN_IN_BACKGROUND: ignore")
    {
        "restricted"
    } else {
        "optimized"
    };

    let mut permissions = parse_permissions(&dump_out, &changeable_permissions);

    // REQUEST_INSTALL_PACKAGES is an appop, but listed in manifest permissions.
    // If it exists in the manifest, we must mark it as changeable.
    if let Some(p) = permissions
        .iter_mut()
        .find(|p| p.name == "android.permission.REQUEST_INSTALL_PACKAGES")
    {
        p.changeable = true;
        if let Some(pos) = appops_out.find("REQUEST_INSTALL_PACKAGES: ") {
            let after = &appops_out[pos + "REQUEST_INSTALL_PACKAGES: ".len()..];
            p.granted = after.starts_with("allow");
        } else {
            p.granted = false;
        }
    }

    Ok(AppDetailsInfo {
        package_name: package_name.clone(),
        display_name: display_name_from_dump(&dump_out, &package_name),
        apk_path,
        is_split,
        system_app: system_out
            .lines()
            .any(|line| line.trim().ends_with(&package_name)),
        disabled: disabled_out
            .lines()
            .any(|line| line.trim().ends_with(&package_name)),
        version_name: dump_value(&dump_out, "versionName"),
        version_code: dump_value(&dump_out, "versionCode"),
        target_sdk: dump_value(&dump_out, "targetSdk"),
        min_sdk: dump_value(&dump_out, "minSdk"),
        installer: dump_value(&dump_out, "installerPackageName"),
        data_dir,
        code_size_bytes,
        data_size_bytes,
        cache_size_bytes,
        background_mode: background_mode.to_string(),
        permissions,
        icon_data_url: String::new(),
        install_date: dump_date_value(&dump_out, "firstInstallTime"),
        update_date: dump_date_value(&dump_out, "lastUpdateTime"),
    })
}

#[tauri::command]
pub async fn set_app_permission(
    serial: String,
    package_name: String,
    permission_name: String,
    grant: bool,
) -> Result<String, String> {
    validate_android_name(&package_name)?;
    validate_android_name(&permission_name)?;
    let action = if grant { "grant" } else { "revoke" };
    let with_user_args = [
        "shell",
        "pm",
        action,
        "--user",
        "current",
        &package_name,
        &permission_name,
    ];
    let result = adb::run_adb_for_serial(&serial, &with_user_args).await?;
    if result.ok() {
        return Ok(result.output);
    }

    let clear_flags_args = [
        "shell",
        "pm",
        "clear-permission-flags",
        "--user",
        "current",
        &package_name,
        &permission_name,
        "user-set",
        "user-fixed",
    ];
    let _ = adb::run_adb_for_serial(&serial, &clear_flags_args).await;

    let retry = adb::run_adb_for_serial(&serial, &with_user_args).await?;
    if retry.ok() {
        return Ok(retry.output);
    }

    let fallback_args = ["shell", "pm", action, &package_name, &permission_name];
    let fallback = adb::run_adb_for_serial(&serial, &fallback_args).await?;
    if fallback.ok() {
        Ok(fallback.output)
    } else {
        Err(fallback.output)
    }
}

#[tauri::command]
pub fn clear_application_cache() -> Result<String, String> {
    let cache_dir = application_cache_dir();
    let legacy_details = cache_dir.parent().unwrap_or(&cache_dir).join("app-details");
    let count = fs::read_dir(&cache_dir)
        .ok()
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    if cache_dir.exists() {
        fs::remove_dir_all(&cache_dir).map_err(|error| error.to_string())?;
    }
    if legacy_details.exists() {
        fs::remove_dir_all(legacy_details).map_err(|error| error.to_string())?;
    }
    Ok(format!("Application cache cleared: {count} entries"))
}

#[tauri::command]
pub async fn list_directory(serial: String, path: String) -> Result<Vec<FileEntry>, String> {
    let listing_path = if path == "/" {
        path.clone()
    } else {
        format!("{}/", path.trim_end_matches('/'))
    };

    // Escape single quotes and wrap in single quotes to prevent shell variable expansion
    let escaped_path = format!("'{}'", listing_path.replace("'", "'\\''"));
    let result = adb::run_adb_for_serial(&serial, &["shell", "ls", "-la", &escaped_path]).await?;
    if !result.ok() {
        return Err(result.output);
    }

    Ok(result.stdout.lines().filter_map(parse_ls_line).collect())
}

fn is_ls_date(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn is_ls_time(token: &str) -> bool {
    let mut parts = token.split(':');
    let valid = |part: Option<&str>| part.is_some_and(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()));
    let hours_minutes = valid(parts.next()) && valid(parts.next());
    hours_minutes && parts.next().map_or(true, |seconds| valid(Some(seconds))) && parts.next().is_none()
}

/// Parses one line of `ls -la`, either from toybox (`drwxrwx--x 3 root sdcard_rw 4096 2024-05-01 10:00 Name`)
/// or from the old toolbox of Android <= 5.1 (no link count, no size for directories).
/// The name is sliced verbatim from the line, so repeated/leading spaces in file names survive.
fn parse_ls_line(line: &str) -> Option<FileEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let kind = line.chars().next()?;
    if !matches!(kind, 'd' | '-' | 'l' | 'c' | 'b' | 'p' | 's') {
        return None;
    }

    // Whitespace separated tokens with their byte ranges.
    let mut tokens: Vec<(usize, usize)> = Vec::new();
    let mut token_start = None;
    for (index, character) in line.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = token_start.take() {
                tokens.push((start, index));
            }
        } else if token_start.is_none() {
            token_start = Some(index);
        }
    }
    if let Some(start) = token_start {
        tokens.push((start, line.len()));
    }
    let token = |index: usize| &line[tokens[index].0..tokens[index].1];

    let date_index = (1..tokens.len().saturating_sub(1))
        .find(|&index| is_ls_date(token(index)) && is_ls_time(token(index + 1)))?;
    let rest = &line[tokens[date_index + 1].1..];
    let raw_name = rest.strip_prefix(' ').unwrap_or(rest);
    if raw_name.is_empty() {
        return None;
    }

    let is_link = kind == 'l';
    let (name, link_target) = if is_link {
        raw_name
            .split_once(" -> ")
            .map(|(name, target)| (name.to_string(), target.to_string()))
            .unwrap_or_else(|| (raw_name.to_string(), String::new()))
    } else {
        (raw_name.to_string(), String::new())
    };
    if name == "." || name == ".." {
        return None;
    }

    // The size is the token before the date. Device nodes print `major, minor` instead, and
    // directories of the old toolbox have no size at all (the token is then the group name).
    let is_device_node = date_index >= 2 && token(date_index - 2).ends_with(',');
    let size = if is_device_node {
        0
    } else {
        token(date_index - 1).parse().unwrap_or(0)
    };

    Some(FileEntry {
        name,
        permissions: token(0).to_string(),
        size,
        modified: format!("{} {}", token(date_index), token(date_index + 1)),
        is_directory: kind == 'd',
        is_link,
        link_target,
    })
}

#[tauri::command]
pub async fn read_file_bytes(serial: String, path: String) -> Result<tauri::ipc::Response, String> {
    // `exec-out` goes through the device shell: the path must be quoted.
    let result =
        adb::run_adb_binary_detailed_for_serial(&serial, &["exec-out", "cat", &shell_quote(&path)])
            .await?;
    if result.exit_code == 0 {
        Ok(tauri::ipc::Response::new(result.stdout))
    } else if result.stderr.is_empty() {
        Err("Failed to read file".to_string())
    } else {
        Err(result.stderr)
    }
}

#[tauri::command]
pub async fn pull_file(
    serial: String,
    remote_path: String,
    local_path: String,
) -> Result<String, String> {
    let result = adb::run_adb_for_serial(&serial, &["pull", &remote_path, &local_path]).await?;
    if result.ok() {
        Ok(result.output.trim().to_string())
    } else {
        Err(result.output.trim().to_string())
    }
}

#[tauri::command]
pub async fn get_file_thumbnail(serial: String, path: String) -> Result<String, String> {
    let (exit_code, bytes) =
        adb::run_adb_binary_for_serial(&serial, &["exec-out", "cat", &shell_quote(&path)]).await?;
    if exit_code != 0 {
        return Err("Could not get the thumbnail".to_string());
    }
    let extension = Path::new(&path)
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mime = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    };
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::{
        apk_abi, apk_density, closest_density, is_blocked_open_extension, keyboard_package_name,
        parse_changeable_permissions, parse_cmd_audio_value, parse_ls_line, parse_media_volume,
        parse_permissions, parse_sideload_progress, preferred_abi, resolve_media_volume,
        sanitize_local_file_name, shell_quote, validate_android_name, wallpaper_payload,
    };
    use std::collections::HashSet;

    #[test]
    fn parses_toybox_ls_lines() {
        let dir = parse_ls_line("drwxrwx--x  3 root sdcard_rw 4096 2024-05-01 10:00 Android").unwrap();
        assert!(dir.is_directory && !dir.is_link);
        assert_eq!((dir.name.as_str(), dir.size, dir.modified.as_str()), ("Android", 4096, "2024-05-01 10:00"));

        let file = parse_ls_line("-rw-rw----  1 u0_a1 media_rw 1234567 2024-05-01 10:00:05 My  Photo (1).jpg").unwrap();
        assert!(!file.is_directory);
        // Repeated spaces in the name must survive.
        assert_eq!(file.name, "My  Photo (1).jpg");
        assert_eq!(file.size, 1_234_567);

        let link = parse_ls_line("lrwxrwxrwx  1 root root 21 2024-05-01 10:00 sdcard -> /storage/self/primary").unwrap();
        assert!(link.is_link);
        assert_eq!((link.name.as_str(), link.link_target.as_str()), ("sdcard", "/storage/self/primary"));
    }

    #[test]
    fn parses_old_toolbox_and_device_node_ls_lines() {
        // Android <= 5.1: no link count, and no size for directories.
        let dir = parse_ls_line("drwxrwx--x root     sdcard_rw          2014-06-23 12:00 Android").unwrap();
        assert!(dir.is_directory);
        assert_eq!((dir.name.as_str(), dir.size), ("Android", 0));
        let file = parse_ls_line("-rw-rw---- root     sdcard_rw     1234 2014-06-23 12:00 notes.txt").unwrap();
        assert_eq!((file.name.as_str(), file.size), ("notes.txt", 1234));
        // Device nodes print `major, minor` instead of a size.
        let node = parse_ls_line("crw-rw-rw- 1 root root 1, 3 2024-05-01 10:00 null").unwrap();
        assert_eq!((node.name.as_str(), node.size), ("null", 0));
    }

    #[test]
    fn ignores_non_entry_ls_lines() {
        assert!(parse_ls_line("total 24").is_none());
        assert!(parse_ls_line("").is_none());
        assert!(parse_ls_line("ls: /sdcard/x: No such file or directory").is_none());
        assert!(parse_ls_line("drwxr-xr-x 2 root root 4096 2024-05-01 10:00 .").is_none());
        assert!(parse_ls_line("drwxr-xr-x 2 root root 4096 2024-05-01 10:00 ..").is_none());
    }

    #[test]
    fn extracts_the_wallpaper_payload_safely() {
        assert_eq!(wallpaper_payload("noise\nWALLPAPER_START\nAAAA\nBBBB\nWALLPAPER_END\n").as_deref(), Some("AAAABBBB"));
        // Markers in the wrong order or missing must not panic.
        assert_eq!(wallpaper_payload("WALLPAPER_END xx WALLPAPER_START"), None);
        assert_eq!(wallpaper_payload("WALLPAPER_STARTWALLPAPER_END"), None);
        assert_eq!(wallpaper_payload("nothing here"), None);
    }

    #[test]
    fn sanitizes_device_supplied_file_names() {
        assert_eq!(sanitize_local_file_name("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_local_file_name("..\\..\\Startup\\evil.bat"), "evil.bat");
        assert_eq!(sanitize_local_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_local_file_name("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_local_file_name("..."), "file");
        assert_eq!(sanitize_local_file_name(""), "file");
        assert!(sanitize_local_file_name(&"x".repeat(500)).len() <= 120);
    }

    #[test]
    fn refuses_to_open_executable_types() {
        for name in ["setup.exe", "run.BAT", "x.ps1", "a.lnk", "tool.jar", "s.sh", "m.msi"] {
            assert!(is_blocked_open_extension(name), "{name}");
        }
        for name in ["photo.jpg", "notes.txt", "movie.mp4", "doc.pdf", "archive", "x.apk"] {
            assert!(!is_blocked_open_extension(name), "{name}");
        }
    }

    #[test]
    fn validates_android_names_and_quotes_shell_values() {
        assert!(validate_android_name("com.example.app_1").is_ok());
        for bad in ["", "a b", "a;b", "a'b", "$(id)", "a/b", "a\"b"] {
            assert!(validate_android_name(bad).is_err(), "{bad}");
        }
        assert_eq!(shell_quote("/sdcard/My Photos/a (1).jpg"), "'/sdcard/My Photos/a (1).jpg'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn reads_media_volume_on_every_android_version() {
        assert_eq!(parse_cmd_audio_value("AudioManager.getStreamVolume(3) -> 9"), Some(9));
        assert_eq!(parse_cmd_audio_value("Unknown command: get-stream-volume"), None);
        let media = "[v] Connecting to AudioService\n[v] volume is 7 in range [0..15]\n";
        assert_eq!(parse_media_volume(media), Some((7, 15)));
        // Android 16+: `cmd audio` answers.
        assert_eq!(
            resolve_media_volume("AudioManager.getStreamVolume(3) -> 4", "AudioManager.getStreamMaxVolume(3) -> 25", ""),
            Some((4, 25))
        );
        // Android <= 15: `cmd audio` prints an error, `media volume` answers.
        assert_eq!(
            resolve_media_volume("Unknown command: get-stream-volume", "Unknown command: get-max-volume", media),
            Some((7, 15))
        );
        assert_eq!(resolve_media_volume("", "", ""), None);
    }

    #[test]
    fn parses_adb_sideload_progress_lines() {
        assert_eq!(parse_sideload_progress("serving: 'ota.zip'  (~47%)    "), Some(47));
        assert_eq!(parse_sideload_progress("serving: '/home/me/update (1).zip'  (~5%)"), Some(5));
        assert_eq!(parse_sideload_progress("serving: 'a~b (~9%).zip'  (~100%)"), Some(100));
        assert_eq!(parse_sideload_progress("serving: 'ota.zip'  (~250%)"), None);
        assert_eq!(parse_sideload_progress("Total xfer: 1.00x"), None);
        assert_eq!(parse_sideload_progress(""), None);
    }

    #[test]
    fn density_splits_match_whole_tokens_only() {
        // `hdpi` is a substring of the other qualifiers: it must not shadow them.
        assert_eq!(apk_density("config.xxxhdpi.apk"), Some(640));
        assert_eq!(apk_density("split_config.xxhdpi.apk"), Some(480));
        assert_eq!(apk_density("config.xhdpi.apk"), Some(320));
        assert_eq!(apk_density("config.hdpi.apk"), Some(240));
        assert_eq!(apk_density("base-mdpi.apk"), Some(160));
        assert_eq!(apk_density("config.ldpi.apk"), Some(120));
        assert_eq!(apk_density("base.apk"), None);
        assert_eq!(apk_density("config.fr.apk"), None);
    }

    #[test]
    fn picks_the_closest_available_density() {
        assert_eq!(closest_density(420, &[240, 320, 480]), Some(480));
        assert_eq!(closest_density(300, &[240, 320, 480]), Some(320));
        assert_eq!(closest_density(400, &[320, 480]), Some(480));
        assert_eq!(closest_density(0, &[320]), None);
        assert_eq!(closest_density(420, &[]), None);
    }

    #[test]
    fn abi_splits_are_recognised_in_both_spellings() {
        assert_eq!(apk_abi("split_config.arm64_v8a.apk").as_deref(), Some("arm64-v8a"));
        assert_eq!(apk_abi("config.arm64-v8a.apk").as_deref(), Some("arm64-v8a"));
        assert_eq!(apk_abi("config.armeabi_v7a.apk").as_deref(), Some("armeabi-v7a"));
        assert_eq!(apk_abi("config.armeabi.apk").as_deref(), Some("armeabi"));
        assert_eq!(apk_abi("config.x86_64.apk").as_deref(), Some("x86_64"));
        assert_eq!(apk_abi("config.x86.apk").as_deref(), Some("x86"));
        assert_eq!(apk_abi("com.example.x86tool.apk"), None);
        assert_eq!(apk_abi("base.apk"), None);
    }

    #[test]
    fn installs_only_the_preferred_abi_present_in_the_bundle() {
        let device = vec!["arm64-v8a".to_string(), "armeabi-v7a".to_string(), "armeabi".to_string()];
        let both = vec!["armeabi-v7a".to_string(), "arm64-v8a".to_string()];
        assert_eq!(preferred_abi(&device, &both).as_deref(), Some("arm64-v8a"));
        let only_v7 = vec!["armeabi-v7a".to_string()];
        assert_eq!(preferred_abi(&device, &only_v7).as_deref(), Some("armeabi-v7a"));
        let only_x86 = vec!["x86".to_string()];
        assert_eq!(preferred_abi(&device, &only_x86), None);
        let x86_64_device = vec!["x86_64".to_string(), "x86".to_string()];
        assert_eq!(preferred_abi(&x86_64_device, &["x86_64".to_string()]).as_deref(), Some("x86_64"));
    }

    #[test]
    fn extracts_keyboard_package_from_component_id() {
        assert_eq!(
            keyboard_package_name("com.samsung.android.honeyboard/.service.HoneyBoardService"),
            "com.samsung.android.honeyboard"
        );
        assert_eq!(keyboard_package_name("package.only"), "package.only");
    }

    #[test]
    fn marks_dumpsys_runtime_permissions_as_toggleable() {
        let output = r#"
          requested permissions:
            android.permission.CAMERA
          runtime permissions:
            android.permission.CAMERA: granted=false, flags=[ USER_SET]
        "#;

        let permissions = parse_permissions(output, &HashSet::new());
        let camera = permissions
            .iter()
            .find(|permission| permission.name == "android.permission.CAMERA")
            .expect("camera permission should be parsed");

        assert!(camera.runtime);
        assert!(camera.changeable);
        assert!(!camera.granted);
    }

    #[test]
    fn marks_dangerous_requested_permissions_as_runtime_when_dumpsys_omits_runtime_section() {
        let dangerous = parse_changeable_permissions(
            "group:android.permission-group.CAMERA\n  permission:android.permission.CAMERA\n",
        );
        let output = r#"
          requested permissions:
            android.permission.CAMERA
        "#;

        let permissions = parse_permissions(output, &dangerous);
        let camera = permissions
            .iter()
            .find(|permission| permission.name == "android.permission.CAMERA")
            .expect("camera permission should be parsed");

        assert!(camera.runtime);
        assert!(camera.changeable);
    }

    #[test]
    fn does_not_mark_install_permissions_as_changeable() {
        let dangerous = parse_changeable_permissions(
            "group:android.permission-group.CAMERA\n  permission:android.permission.CAMERA\n",
        );
        let output = r#"
          install permissions:
            android.permission.CAMERA: granted=true
        "#;

        let permissions = parse_permissions(output, &dangerous);
        let camera = permissions
            .iter()
            .find(|permission| permission.name == "android.permission.CAMERA")
            .expect("camera permission should be parsed");

        assert!(!camera.runtime);
        assert!(!camera.changeable);
        assert!(camera.granted);
    }

    #[test]
    fn marks_fixed_runtime_permissions_as_not_changeable() {
        let output = r#"
          runtime permissions:
            android.permission.CAMERA: granted=true, flags=[ SYSTEM_FIXED]
        "#;

        let permissions = parse_permissions(output, &HashSet::new());
        let camera = permissions
            .iter()
            .find(|permission| permission.name == "android.permission.CAMERA")
            .expect("camera permission should be parsed");

        assert!(camera.runtime);
        assert!(!camera.changeable);
    }
}

#[tauri::command]
pub fn launch_scrcpy(serial: String, extra_args: Vec<String>) -> Result<String, String> {
    if crate::mock::enabled() {
        return Ok(format!("scrcpy launched for {serial}"));
    }

    let executable = tools::resolve_tool_path("scrcpy").ok_or_else(|| {
        "scrcpy is not installed. Configure or install it in Settings.".to_string()
    })?;
    if let Some(record_path) = extra_args
        .iter()
        .find_map(|argument| argument.strip_prefix("--record="))
    {
        if let Some(parent) = Path::new(record_path).parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not create recording directory: {error}"))?;
        }
    }
    let mut command = crate::process::command(executable);
    command
        .arg("--serial")
        .arg(&serial)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not launch scrcpy: {error}"))?;
    // Wait for it on a helper thread: an un-waited child stays defunct on Unix once it exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(format!("scrcpy launched for {serial}"))
}

#[tauri::command]
pub async fn list_scrcpy_cameras(serial: String) -> Result<Vec<String>, String> {
    if crate::mock::enabled() {
        return Ok(vec!["0 (Back)".to_string(), "1 (Front)".to_string()]);
    }

    tauri::async_runtime::spawn_blocking(move || {
        let executable = tools::resolve_tool_path("scrcpy").ok_or_else(|| {
            "scrcpy is not installed. Configure or install it in Settings.".to_string()
        })?;
        let output = crate::process::command(executable)
            .args(["--serial", &serial, "--list-cameras"])
            .output()
            .map_err(|error| format!("Could not query scrcpy cameras: {error}"))?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let cameras = combined
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("--camera-id="))
            .map(str::to_string)
            .collect::<Vec<_>>();
        if output.status.success() || !cameras.is_empty() {
            Ok(cameras)
        } else {
            Err(combined.trim().to_string())
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn get_tools_snapshot() -> tools::ToolsSnapshot {
    if crate::mock::enabled() {
        return crate::mock::tools_snapshot();
    }

    tools::tools_snapshot().await
}

#[tauri::command]
pub async fn force_check_updates(app: tauri::AppHandle) -> tools::ToolsSnapshot {
    use tauri::Emitter;
    tools::force_check_updates_flag();
    tauri::async_runtime::spawn(async move {
        let status = tools::tools_status_with_updates().await;
        let _ = app.emit("tools-updates-checked", status);
    });

    tools::ToolsSnapshot {
        tools: tools::tools_status_cached().await,
        checking_updates: true,
    }
}

#[tauri::command]
pub async fn set_tool_path(tool: String, path: String) -> Result<ToolsStatus, String> {
    tauri::async_runtime::spawn_blocking(move || tools::save_tool_path(&tool, &path))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn install_or_update_tool(tool: String) -> Result<ToolsStatus, String> {
    tools::install_or_update(&tool).await
}

#[tauri::command]
pub async fn export_apk(
    serial: String,
    package_name: String,
    destination: String,
) -> Result<(), String> {
    validate_android_name(&package_name)?;
    let paths_result =
        adb::run_adb_for_serial(&serial, &["shell", "pm", "path", &package_name]).await?;
    if !paths_result.ok() {
        return Err(paths_result.output);
    }
    let apk_paths: Vec<String> = paths_result
        .output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .map(|s| s.to_string())
        .collect();

    if apk_paths.is_empty() {
        return Err(format!("No APK paths found for {}", package_name));
    }

    if apk_paths.len() == 1 {
        // Single APK
        let result =
            adb::run_adb_for_serial(&serial, &["pull", &apk_paths[0], &destination]).await?;
        if result.ok() {
            Ok(())
        } else {
            Err(result.output)
        }
    } else {
        // Split APKs
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis();
        let temp_dir = crate::app_paths::cache_dir()
            .join("temp")
            .join(nonce.to_string());
        fs::create_dir_all(&temp_dir).map_err(|error| error.to_string())?;

        // Pull all APKs
        for path in &apk_paths {
            let result =
                adb::run_adb_for_serial(&serial, &["pull", path, &temp_dir.to_string_lossy()])
                    .await?;
            if !result.ok() {
                let _ = fs::remove_dir_all(&temp_dir);
                return Err(format!("Failed to pull {}: {}", path, result.output));
            }
        }

        // Zip them into destination
        let file = std::fs::File::create(&destination)
            .map_err(|e| format!("Failed to create destination file: {}", e))?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Deflated);

        for entry in fs::read_dir(&temp_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.is_file() {
                let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
                zip.start_file(file_name, options)
                    .map_err(|e| format!("Failed to start zip file: {}", e))?;
                let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                std::io::copy(&mut f, &mut zip)
                    .map_err(|e| format!("Failed to write to zip: {}", e))?;
            }
        }

        zip.finish()
            .map_err(|e| format!("Failed to finish zip: {}", e))?;
        let _ = fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}

#[derive(serde::Serialize)]
pub struct HomeDetails {
    pub device_name: String,
    pub airplane_mode: bool,
    pub carrier: String,
}

#[tauri::command]
pub async fn get_home_details(serial: String) -> Result<HomeDetails, String> {
    if crate::mock::enabled() {
        return Ok(crate::mock::home_details());
    }

    let (name_res, airplane_res, carrier_res) = tokio::join!(
        adb::run_adb_for_serial(
            &serial,
            &["shell", "settings", "get", "global", "device_name"]
        ),
        adb::run_adb_for_serial(
            &serial,
            &["shell", "settings", "get", "global", "airplane_mode_on"]
        ),
        adb::run_adb_for_serial(&serial, &["shell", "getprop", "gsm.operator.alpha"])
    );

    let mut name = String::new();
    if let Ok(res) = name_res {
        if res.ok() && !res.output.trim().is_empty() && res.output.trim() != "null" {
            name = res.output.trim().to_string();
        }
    }

    let mut airplane = false;
    if let Ok(res) = airplane_res {
        if res.ok() && res.output.trim() == "1" {
            airplane = true;
        }
    }

    let mut carrier = String::new();
    if let Ok(res) = carrier_res {
        if res.ok() && !res.output.trim().is_empty() {
            let mut parts: Vec<String> = res
                .output
                .trim()
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| {
                    !p.is_empty() && p.to_lowercase() != "unknown" && p.to_lowercase() != "null"
                })
                .collect();
            parts.dedup();
            carrier = parts.join(" / ");
        }
    }

    Ok(HomeDetails {
        device_name: name,
        airplane_mode: airplane,
        carrier,
    })
}

/// File types the OS would run (or treat as active content) when "opened".
const BLOCKED_OPEN_EXTENSIONS: &[&str] = &[
    "exe", "com", "bat", "cmd", "msi", "msp", "scr", "pif", "cpl", "dll", "vbs", "vbe", "js",
    "jse", "wsf", "wsh", "ps1", "psm1", "hta", "lnk", "url", "reg", "inf", "jar", "appx", "msix",
    "app", "command", "sh", "desktop", "scf",
];

fn is_blocked_open_extension(file_name: &str) -> bool {
    Path::new(file_name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|extension| BLOCKED_OPEN_EXTENSIONS.contains(&extension.as_str()))
}

/// Reduces a device-supplied file name to a safe local one: last path component only,
/// characters invalid on Windows replaced, no leading/trailing dots or spaces, bounded length.
fn sanitize_local_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    let cleaned: String = cleaned.chars().take(120).collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

#[tauri::command]
pub async fn download_and_open_file(
    app: tauri::AppHandle,
    serial: String,
    remote_path: String,
    file_name: String,
) -> Result<String, String> {
    let temp_dir = crate::app_paths::cache_dir().join("temp");
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;

    // The name comes from the device: keep only a plain file name (no `..\`, drive letters...) and
    // never hand executable types to the OS "open" action.
    let file_name = sanitize_local_file_name(&file_name);
    if is_blocked_open_extension(&file_name) {
        return Err(
            "For your safety, executable files are not opened directly. Download the file and open it manually if you trust it."
                .to_string(),
        );
    }

    // Generar un sufijo aleatorio para evitar conflictos de nombres
    let nonce: u32 = rand::random();
    let local_file_name = format!("{}_{}", nonce, file_name);
    let local_path = temp_dir.join(&local_file_name);
    let local_path_str = local_path.to_string_lossy().to_string();

    let result = adb::run_adb_for_serial(&serial, &["pull", &remote_path, &local_path_str]).await?;
    if !result.ok() {
        return Err(result.output);
    }

    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(local_path_str, None::<&str>)
        .map_err(|e| e.to_string())?;

    Ok("Opened successfully".to_string())
}

#[derive(Deserialize)]
struct ApkmirrorApp {
    link: String,
}

#[derive(Deserialize)]
struct ApkmirrorData {
    exists: bool,
    pname: String,
    app: Option<ApkmirrorApp>,
}

#[derive(Deserialize)]
struct ApkmirrorResponse {
    data: Vec<ApkmirrorData>,
}

#[tauri::command]
pub async fn search_apkmirror(package_name: String) -> Result<Option<String>, String> {
    let client = reqwest::Client::new();
    let url = "https://www.apkmirror.com/wp-json/apkm/v1/app_exists";

    let mut map = HashMap::new();
    map.insert("pnames", package_name.clone());

    let res = client
        .post(url)
        .header("User-Agent", "ADB-App")
        .header(
            "Authorization",
            "Basic YXBpLWFwa3VwZGF0ZXI6cm01cmNmcnVVakt5MDRzTXB5TVBKWFc4",
        )
        .header("Content-Type", "application/json")
        .json(&map)
        .send()
        .await;

    if let Ok(response) = res {
        if let Ok(json) = response.json::<ApkmirrorResponse>().await {
            if let Some(data) = json.data.first() {
                if data.exists && data.pname == package_name {
                    if let Some(app) = &data.app {
                        let link = format!("https://www.apkmirror.com{}", app.link);
                        return Ok(Some(link));
                    }
                }
            }
        }
    }

    Ok(None)
}

#[cfg(target_os = "windows")]
#[tauri::command]
pub async fn open_store_review(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = "ms-windows-store://review/?ProductId=9N4VV2153B05";
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct StorageSizes {
    pub data_size_bytes: i64,
    pub cache_size_bytes: i64,
}

#[tauri::command]
pub async fn get_app_storage_sizes(
    app: tauri::AppHandle,
    serial: String,
    package_name: String,
) -> Result<StorageSizes, String> {
    if crate::mock::enabled() {
        return Ok(StorageSizes {
            data_size_bytes: -1,
            cache_size_bytes: -1,
        });
    }

    validate_android_name(&package_name)?;
    push_daemons_if_needed(&app, &serial).await?;

    let version = env!("CARGO_PKG_VERSION");
    let jar_name = format!("info_apps_{}.jar", version);
    let daemon_device_path = format!("/data/local/tmp/{}", jar_name);

    let result = adb::run_adb_for_serial(
        &serial,
        &[
            "shell",
            &format!(
                "CLASSPATH={} app_process / com.kyro.adbapp.extractapktool.GetSizes {}",
                daemon_device_path, package_name
            ),
        ],
    )
    .await?;

    if !result.ok() {
        return Err(format!("Daemon execution failed: {}", result.output));
    }

    let daemon_output = result.output.trim();
    let json_start = daemon_output.find('[').unwrap_or(0);
    let json_end = daemon_output
        .rfind(']')
        .unwrap_or_else(|| daemon_output.len().saturating_sub(1));

    let clean_json = if json_start <= json_end && json_end < daemon_output.len() {
        &daemon_output[json_start..=json_end]
    } else {
        daemon_output
    };

    let parsed_responses: Vec<DaemonResponse> =
        serde_json::from_str(clean_json).unwrap_or_else(|_| Vec::new());

    if let Some(resp) = parsed_responses.first() {
        let cache = resp.cacheSize.unwrap_or(-1);
        // `StorageStats.getDataBytes()` includes the cache, which is reported separately.
        let data = match resp.dataSize {
            Some(data) if data >= 0 && cache >= 0 => (data - cache).max(0),
            Some(data) => data,
            None => -1,
        };
        Ok(StorageSizes {
            data_size_bytes: data,
            cache_size_bytes: cache,
        })
    } else {
        Ok(StorageSizes {
            data_size_bytes: -1,
            cache_size_bytes: -1,
        })
    }
}
