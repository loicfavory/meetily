use log::{info, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_store::StoreExt;

use anyhow::Result;
#[cfg(target_os = "macos")]
use log::error;

#[cfg(target_os = "macos")]
use crate::audio::capture::AudioCaptureBackend;

const STORE_FILE: &str = "recording_preferences.json";
const STORE_KEY: &str = "preferences";
const SCHEMA_VERSION: u32 = 2;
const V1_BACKUP_FILE: &str = "recording_preferences.v1.backup.json";
const INVALID_BACKUP_FILE: &str = "recording_preferences.invalid.backup.json";
const GENERAL_KEYS: [&str; 4] = [
    "save_folder",
    "auto_save",
    "file_format",
    "system_audio_backend",
];
const SYSTEM_AUDIO_SUFFIX: &str = " (System Audio)";

/// Serializes load -> migrate -> save and every write command.
static PREFERENCES_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Persisted recording preferences (schema version 2).
///
/// Device fields, per role (`mic` / `system`):
///
/// | `preferred_*` | `unresolved_*` | Meaning                          | At start                      |
/// |---------------|----------------|----------------------------------|-------------------------------|
/// | `None`        | `None`         | Real system default              | system default, silent        |
/// | `Some(id)`    | `None`         | Explicit choice (exact device ID)| that ID (TECH-03 policy if absent) |
/// | `None`        | `Some(text)`   | Legacy choice that could not be migrated | system default + warning |
///
/// `preferred_*_label` is a display-only snapshot, never an identity.
/// `migration_pending` is a wire-only flag and is never written to disk.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordingPreferences {
    #[serde(default)]
    pub schema_version: u32,
    pub save_folder: PathBuf,
    pub auto_save: bool,
    pub file_format: String,
    #[serde(default)]
    pub preferred_mic_device: Option<String>,
    #[serde(default)]
    pub preferred_mic_label: Option<String>,
    #[serde(default)]
    pub preferred_system_device: Option<String>,
    #[serde(default)]
    pub preferred_system_label: Option<String>,
    #[serde(default)]
    pub unresolved_mic_device: Option<String>,
    #[serde(default)]
    pub unresolved_system_device: Option<String>,
    #[serde(default)]
    pub migration_pending: bool,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub system_audio_backend: Option<String>,
}

impl Default for RecordingPreferences {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            save_folder: get_default_recordings_folder(),
            auto_save: true,
            file_format: "mp4".to_string(),
            preferred_mic_device: None,
            preferred_mic_label: None,
            preferred_system_device: None,
            preferred_system_label: None,
            unresolved_mic_device: None,
            unresolved_system_device: None,
            migration_pending: false,
            #[cfg(target_os = "macos")]
            system_audio_backend: Some("coreaudio".to_string()),
        }
    }
}

/// Get the default recordings folder based on platform
pub fn get_default_recordings_folder() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        // Windows: %USERPROFILE%\Music\meetily-recordings
        if let Some(music_dir) = dirs::audio_dir() {
            music_dir.join("meetily-recordings")
        } else {
            // Fallback to Documents if Music folder is not available
            dirs::document_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("meetily-recordings")
        }
    }

    #[cfg(target_os = "macos")]
    {
        // macOS: ~/Movies/meetily-recordings
        if let Some(movies_dir) = dirs::video_dir() {
            movies_dir.join("meetily-recordings")
        } else {
            // Fallback to Documents if Movies folder is not available
            dirs::document_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("meetily-recordings")
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // Linux/Others: ~/Documents/meetily-recordings
        dirs::document_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("meetily-recordings")
    }
}

/// Ensure the recordings directory exists
pub fn ensure_recordings_directory(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
        info!("Created recordings directory: {:?}", path);
    }
    Ok(())
}

/// Generate a unique filename for a recording
pub fn generate_recording_filename(format: &str) -> String {
    let now = chrono::Utc::now();
    let timestamp = now.format("%Y%m%d_%H%M%S");
    format!("recording_{}.{}", timestamp, format)
}

/// Version of a stored preferences object: no `schema_version` means v1.
fn stored_version(raw: &Value) -> u32 {
    raw.get("schema_version")
        .and_then(Value::as_u64)
        .filter(|v| *v >= 1)
        .map(|v| v.min(u32::MAX as u64) as u32)
        .unwrap_or(1)
}

fn role_keys(role: &str) -> (String, String, String) {
    (
        format!("preferred_{}_device", role),
        format!("preferred_{}_label", role),
        format!("unresolved_{}_device", role),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: String,
    /// Raw server description, not the disambiguated display label.
    pub description: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RoleCandidates {
    Known(Vec<Candidate>),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MigrationInventory {
    pub microphone: RoleCandidates,
    pub system: RoleCandidates,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MigrationOutcome {
    Migrated(Value),
    Deferred,
}

enum RoleResolution {
    Resolved { id: String, label: String },
    Unresolved(String),
}

fn resolve_role(value: &str, is_system: bool, candidates: &[Candidate]) -> RoleResolution {
    if let Some(candidate) = candidates.iter().find(|c| c.id == value) {
        return RoleResolution::Resolved {
            id: candidate.id.clone(),
            label: candidate.label.clone(),
        };
    }
    let normalized = if is_system {
        value.strip_suffix(SYSTEM_AUDIO_SUFFIX).unwrap_or(value)
    } else {
        value
    };
    let mut matches = candidates.iter().filter(|c| c.description == normalized);
    match (matches.next(), matches.next()) {
        (Some(candidate), None) => RoleResolution::Resolved {
            id: candidate.id.clone(),
            label: candidate.label.clone(),
        },
        _ => RoleResolution::Unresolved(value.to_string()),
    }
}

fn string_or_null(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |v| Value::String(v.to_string()))
}

/// Pure v1 -> v2 migration (Linux rules). `Deferred` when a role holds a
/// value but its inventory is unknown: nothing is migrated partially.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn migrate_v1(raw: &Value, inventory: &MigrationInventory) -> MigrationOutcome {
    let Some(object) = raw.as_object() else {
        return MigrationOutcome::Deferred;
    };
    let mut out = object.clone();
    let roles = [
        ("mic", false, &inventory.microphone),
        ("system", true, &inventory.system),
    ];
    for (role, is_system, candidates) in roles {
        let (preferred_key, label_key, unresolved_key) = role_keys(role);
        let value = object.get(&preferred_key).and_then(Value::as_str);
        let (preferred, label, unresolved) = match (value, candidates) {
            (None, _) => (Value::Null, Value::Null, Value::Null),
            (Some(_), RoleCandidates::Unknown) => return MigrationOutcome::Deferred,
            (Some(v), RoleCandidates::Known(list)) => match resolve_role(v, is_system, list) {
                RoleResolution::Resolved { id, label } => {
                    (Value::String(id), Value::String(label), Value::Null)
                }
                RoleResolution::Unresolved(legacy) => {
                    (Value::Null, Value::Null, Value::String(legacy))
                }
            },
        };
        out.insert(preferred_key, preferred);
        out.insert(label_key, label);
        out.insert(unresolved_key, unresolved);
    }
    out.insert("schema_version".into(), json!(SCHEMA_VERSION));
    MigrationOutcome::Migrated(Value::Object(out))
}

/// Windows/macOS: v1 values were already device IDs, only the version stamp
/// changes.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn stamp_v1_as_v2(raw: &Value) -> Value {
    let mut out = raw.as_object().cloned().unwrap_or_default();
    for role in ["mic", "system"] {
        let (preferred_key, label_key, unresolved_key) = role_keys(role);
        let preferred = out.get(&preferred_key).cloned().unwrap_or(Value::Null);
        out.insert(preferred_key, preferred);
        out.insert(label_key, Value::Null);
        out.insert(unresolved_key, Value::Null);
    }
    out.insert("schema_version".into(), json!(SCHEMA_VERSION));
    Value::Object(out)
}

/// In-memory v2 view of a v1 object whose migration is deferred: every
/// non-null v1 device value is kept as an unresolved legacy choice.
fn deferred_view(raw: &Value) -> Value {
    let mut out = raw.as_object().cloned().unwrap_or_default();
    for role in ["mic", "system"] {
        let (preferred_key, label_key, unresolved_key) = role_keys(role);
        let legacy = out.get(&preferred_key).and_then(Value::as_str).map(str::to_string);
        out.insert(preferred_key, Value::Null);
        out.insert(label_key, Value::Null);
        out.insert(unresolved_key, string_or_null(legacy.as_deref()));
    }
    out.insert("schema_version".into(), json!(SCHEMA_VERSION));
    Value::Object(out)
}

fn migration_outcome(raw: &Value) -> MigrationOutcome {
    #[cfg(target_os = "linux")]
    {
        let inventory = crate::audio::devices::platform::linux::migration_inventory();
        migrate_v1(raw, &inventory)
    }
    #[cfg(not(target_os = "linux"))]
    {
        MigrationOutcome::Migrated(stamp_v1_as_v2(raw))
    }
}

fn parse_role(role: &str) -> Result<&'static str, String> {
    match role {
        "microphone" => Ok("mic"),
        "system" => Ok("system"),
        other => Err(format!("Invalid audio role: {}", other)),
    }
}

/// Sets the explicit choice of one role and clears its unresolved legacy
/// value. The other role is untouched.
fn apply_device_choice(
    object: &mut Map<String, Value>,
    role: &str,
    device_id: Option<&str>,
    label: Option<&str>,
) -> Result<(), String> {
    let (preferred_key, label_key, unresolved_key) = role_keys(parse_role(role)?);
    let label = if device_id.is_some() { label } else { None };
    object.insert(preferred_key, string_or_null(device_id));
    object.insert(label_key, string_or_null(label));
    object.insert(unresolved_key, Value::Null);
    Ok(())
}

/// Copies only the general fields of `prefs` over the stored object; device
/// fields, unresolved values and unknown keys are preserved.
fn patch_general_fields(raw: &Value, prefs: &RecordingPreferences) -> Value {
    let mut out = raw.as_object().cloned().unwrap_or_default();
    if let Ok(incoming) = serde_json::to_value(prefs) {
        for key in GENERAL_KEYS {
            if let Some(value) = incoming.get(key) {
                out.insert(key.to_string(), value.clone());
            }
        }
    }
    Value::Object(out)
}

#[derive(Debug, PartialEq, Eq)]
pub enum BackupResult {
    Written,
    AlreadyPresent,
}

/// Writes `{ "preferences": raw }` atomically and never overwrites an
/// existing file: the first backup is the original.
pub fn write_backup_if_absent(dir: &Path, file_name: &str, raw: &Value) -> Result<BackupResult> {
    let target = dir.join(file_name);
    if target.exists() {
        return Ok(BackupResult::AlreadyPresent);
    }
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!("{}.tmp", file_name));
    let body = serde_json::to_vec_pretty(&json!({ STORE_KEY: raw }))?;
    std::fs::write(&temp, body)?;
    if let Err(e) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(e.into());
    }
    Ok(BackupResult::Written)
}

fn backup_raw<R: Runtime>(app: &AppHandle<R>, file_name: &str, raw: &Value) -> Result<()> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| anyhow::anyhow!("Failed to resolve app data dir: {}", e))?;
    if write_backup_if_absent(&dir, file_name, raw)? == BackupResult::Written {
        info!("Backed up stored recording preferences to {}", file_name);
    }
    Ok(())
}

fn defaults_object() -> Map<String, Value> {
    match serde_json::to_value(RecordingPreferences::default()) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Runs the v1 migration. Returns the v2 object and whether it is only an
/// in-memory view (deferred, or the backup failed): in that case nothing may
/// be written as a final v2 without a successful backup.
fn upgrade_v1<R: Runtime>(app: &AppHandle<R>, raw: &Value) -> (Value, bool) {
    match migration_outcome(raw) {
        MigrationOutcome::Migrated(migrated) => match backup_raw(app, V1_BACKUP_FILE, raw) {
            Ok(()) => (migrated, false),
            Err(e) => {
                warn!("Preferences backup failed ({}); deferring migration", e);
                (deferred_view(raw), true)
            }
        },
        MigrationOutcome::Deferred => (deferred_view(raw), true),
    }
}

fn from_stored(value: Value, migration_pending: bool) -> Result<RecordingPreferences> {
    let mut prefs: RecordingPreferences = serde_json::from_value(value)?;
    prefs.migration_pending = migration_pending;
    #[cfg(target_os = "macos")]
    {
        let backend = crate::audio::capture::get_current_backend();
        prefs.system_audio_backend = Some(backend.to_string());
    }
    Ok(prefs)
}

/// Load recording preferences from store, migrating a v1 object on the way.
pub async fn load_recording_preferences<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<RecordingPreferences> {
    let _guard = PREFERENCES_LOCK.lock().await;

    let store = match app.store(STORE_FILE) {
        Ok(store) => store,
        Err(e) => {
            warn!("Failed to access store: {}, using defaults", e);
            return Ok(RecordingPreferences::default());
        }
    };

    let prefs = match store.get(STORE_KEY) {
        None => {
            info!("No stored preferences found, using defaults");
            RecordingPreferences::default()
        }
        Some(raw) if !raw.is_object() => {
            warn!("Stored preferences are not an object, using defaults");
            RecordingPreferences::default()
        }
        Some(raw) => {
            let version = stored_version(&raw);
            let (view, pending) = if version == 1 {
                let (view, pending) = upgrade_v1(app, &raw);
                if !pending {
                    store.set(STORE_KEY, view.clone());
                    if let Err(e) = store.save() {
                        warn!("Failed to persist migrated preferences: {}", e);
                    }
                }
                (view, pending)
            } else {
                if version > SCHEMA_VERSION {
                    warn!("Stored preferences use future schema version {}", version);
                }
                (raw, false)
            };
            match from_stored(view, pending) {
                Ok(prefs) => {
                    info!("Loaded recording preferences from store");
                    prefs
                }
                Err(e) => {
                    warn!("Failed to deserialize preferences: {}, using defaults", e);
                    RecordingPreferences::default()
                }
            }
        }
    };

    info!("Loaded recording preferences: save_folder={:?}, auto_save={}, format={}, mic={:?}, system={:?}",
          prefs.save_folder, prefs.auto_save, prefs.file_format,
          prefs.preferred_mic_device, prefs.preferred_system_device);
    Ok(prefs)
}

/// Base object for a write that replaces nothing but some keys: a stored
/// non-object or non-deserializable value is backed up first.
fn writable_base<R: Runtime>(app: &AppHandle<R>, raw: Option<Value>) -> Result<Value> {
    match raw {
        None => Ok(Value::Object(defaults_object())),
        Some(value) if !value.is_object() => {
            backup_raw(app, INVALID_BACKUP_FILE, &value)?;
            Ok(Value::Object(defaults_object()))
        }
        Some(value) => {
            if serde_json::from_value::<RecordingPreferences>(value.clone()).is_err() {
                backup_raw(app, INVALID_BACKUP_FILE, &value)?;
            }
            Ok(value)
        }
    }
}

/// Persist the general fields only (see `patch_general_fields`).
async fn save_general_preferences<R: Runtime>(
    app: &AppHandle<R>,
    preferences: &RecordingPreferences,
) -> Result<()> {
    info!(
        "Saving general recording preferences: save_folder={:?}, auto_save={}, format={}",
        preferences.save_folder, preferences.auto_save, preferences.file_format
    );

    let _guard = PREFERENCES_LOCK.lock().await;

    let store = app
        .store(STORE_FILE)
        .map_err(|e| anyhow::anyhow!("Failed to access store: {}", e))?;

    let base = writable_base(app, store.get(STORE_KEY))?;
    store.set(STORE_KEY, patch_general_fields(&base, preferences));
    store
        .save()
        .map_err(|e| anyhow::anyhow!("Failed to save store to disk: {}", e))?;

    info!("Successfully persisted recording preferences to disk");

    // Save backend preference to global config
    #[cfg(target_os = "macos")]
    if let Some(backend_str) = &preferences.system_audio_backend {
        if let Some(backend) = AudioCaptureBackend::from_string(backend_str) {
            info!("Setting audio capture backend to: {:?}", backend);
            crate::audio::capture::set_current_backend(backend);
        }
    }

    // Ensure the directory exists
    ensure_recordings_directory(&preferences.save_folder)?;

    Ok(())
}

/// Base object for a device choice: a v1 object is migrated first (a deferred
/// migration still becomes v2, keeping legacy values as unresolved, but only
/// once its backup is safely on disk).
fn device_choice_base<R: Runtime>(app: &AppHandle<R>, raw: Option<Value>) -> Result<Value> {
    match raw {
        Some(value) if value.is_object() && stored_version(&value) == 1 => {
            let (view, pending) = upgrade_v1(app, &value);
            if pending {
                backup_raw(app, V1_BACKUP_FILE, &value)?;
            }
            Ok(view)
        }
        other => writable_base(app, other),
    }
}

/// Tauri commands for recording preferences
#[tauri::command]
pub async fn get_recording_preferences<R: Runtime>(
    app: AppHandle<R>,
) -> Result<RecordingPreferences, String> {
    load_recording_preferences(&app)
        .await
        .map_err(|e| format!("Failed to load recording preferences: {}", e))
}

/// Saves the general fields (`save_folder`, `auto_save`, `file_format`,
/// `system_audio_backend`) only. Device fields sent by the caller are ignored:
/// use `set_preferred_audio_device` to change a device choice.
#[tauri::command]
pub async fn set_recording_preferences<R: Runtime>(
    app: AppHandle<R>,
    preferences: RecordingPreferences,
) -> Result<(), String> {
    save_general_preferences(&app, &preferences)
        .await
        .map_err(|e| format!("Failed to save recording preferences: {}", e))
}

/// Records the user's explicit choice for one role (`"microphone"` or
/// `"system"`); `device_id = None` means the real system default. Returns the
/// resulting preferences.
#[tauri::command]
pub async fn set_preferred_audio_device<R: Runtime>(
    app: AppHandle<R>,
    role: String,
    device_id: Option<String>,
    label: Option<String>,
) -> Result<RecordingPreferences, String> {
    parse_role(&role)?;

    let _guard = PREFERENCES_LOCK.lock().await;

    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to access store: {}", e))?;

    let base = device_choice_base(&app, store.get(STORE_KEY))
        .map_err(|e| format!("Failed to prepare recording preferences: {}", e))?;
    let mut object = base.as_object().cloned().unwrap_or_default();
    apply_device_choice(&mut object, &role, device_id.as_deref(), label.as_deref())?;
    let updated = Value::Object(object);

    store.set(STORE_KEY, updated.clone());
    store
        .save()
        .map_err(|e| format!("Failed to save store to disk: {}", e))?;

    from_stored(updated, false)
        .map_err(|e| format!("Failed to read back recording preferences: {}", e))
}

#[tauri::command]
pub async fn get_default_recordings_folder_path() -> Result<String, String> {
    let path = get_default_recordings_folder();
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn open_recordings_folder<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let preferences = load_recording_preferences(&app)
        .await
        .map_err(|e| format!("Failed to load preferences: {}", e))?;

    // Ensure directory exists before trying to open it
    ensure_recordings_directory(&preferences.save_folder)
        .map_err(|e| format!("Failed to create directory: {}", e))?;

    let folder_path = preferences.save_folder.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    info!("Opened recordings folder: {}", folder_path);
    Ok(())
}

#[tauri::command]
pub async fn select_recording_folder<R: Runtime>(
    _app: AppHandle<R>,
) -> Result<Option<String>, String> {
    // Use Tauri's dialog to select folder
    // For now, return None - this would need to be implemented with tauri-plugin-dialog
    // when it's available in the Cargo.toml
    warn!("Folder selection not yet implemented - using dialog plugin");
    Ok(None)
}

// Backend selection commands

/// Get available audio capture backends for the current platform
#[tauri::command]
pub async fn get_available_audio_backends() -> Result<Vec<String>, String> {
    #[cfg(target_os = "macos")]
    {
        let backends = crate::audio::capture::get_available_backends();
        Ok(backends.iter().map(|b| b.to_string()).collect())
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Only ScreenCaptureKit available on non-macOS
        Ok(vec!["screencapturekit".to_string()])
    }
}

/// Get current audio capture backend
#[tauri::command]
pub async fn get_current_audio_backend() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        let backend = crate::audio::capture::get_current_backend();
        Ok(backend.to_string())
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok("screencapturekit".to_string())
    }
}

/// Set audio capture backend
#[tauri::command]
pub async fn set_audio_backend(backend: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use crate::audio::capture::AudioCaptureBackend;
        use crate::audio::permissions::{
            check_screen_recording_permission, request_screen_recording_permission,
        };

        let backend_enum = AudioCaptureBackend::from_string(&backend)
            .ok_or_else(|| format!("Invalid backend: {}", backend))?;

        // If switching to Core Audio, log information about Audio Capture permission
        if backend_enum == AudioCaptureBackend::CoreAudio {
            info!("🔐 Core Audio backend requires Audio Capture permission (macOS 14.4+)");
            info!("📍 Permission dialog will appear automatically when recording starts");

            // Check if permission is already granted (this is informational only)
            if !check_screen_recording_permission() {
                warn!("⚠️  Audio Capture permission may not be granted");

                // Attempt to open System Settings (opens System Settings)
                if let Err(e) = request_screen_recording_permission() {
                    error!("Failed to open System Settings: {}", e);
                }

                return Err(
                    "Core Audio requires Audio Capture permission. \
                    The permission dialog will appear when you start recording. \
                    If already denied, enable it in System Settings → Privacy & Security → Audio Capture, \
                    then restart the app.".to_string()
                );
            }

            info!(
                "✅ Core Audio backend selected - permission check will occur at recording start"
            );
        }

        info!("Setting audio backend to: {:?}", backend_enum);
        crate::audio::capture::set_current_backend(backend_enum);
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        if backend != "screencapturekit" {
            return Err(format!(
                "Backend {} not available on this platform",
                backend
            ));
        }
        Ok(())
    }
}

/// Get backend information (name and description)
#[derive(Serialize)]
pub struct BackendInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[tauri::command]
pub async fn get_audio_backend_info() -> Result<Vec<BackendInfo>, String> {
    #[cfg(target_os = "macos")]
    {
        use crate::audio::capture::AudioCaptureBackend;

        let backends = vec![
            BackendInfo {
                id: AudioCaptureBackend::ScreenCaptureKit.to_string(),
                name: AudioCaptureBackend::ScreenCaptureKit.name().to_string(),
                description: AudioCaptureBackend::ScreenCaptureKit
                    .description()
                    .to_string(),
            },
            BackendInfo {
                id: AudioCaptureBackend::CoreAudio.to_string(),
                name: AudioCaptureBackend::CoreAudio.name().to_string(),
                description: AudioCaptureBackend::CoreAudio.description().to_string(),
            },
        ];
        Ok(backends)
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(vec![BackendInfo {
            id: "screencapturekit".to_string(),
            name: "ScreenCaptureKit".to_string(),
            description: "Default system audio capture".to_string(),
        }])
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, description: &str) -> Candidate {
        Candidate {
            id: id.to_string(),
            description: description.to_string(),
            label: format!("label:{}", id),
        }
    }

    fn inventory(mics: Vec<Candidate>, sinks: Vec<Candidate>) -> MigrationInventory {
        MigrationInventory {
            microphone: RoleCandidates::Known(mics),
            system: RoleCandidates::Known(sinks),
        }
    }

    fn unknown_inventory() -> MigrationInventory {
        MigrationInventory {
            microphone: RoleCandidates::Unknown,
            system: RoleCandidates::Unknown,
        }
    }

    fn v1(mic: Option<&str>, system: Option<&str>) -> Value {
        json!({
            "save_folder": "/rec",
            "auto_save": false,
            "file_format": "wav",
            "preferred_mic_device": mic,
            "preferred_system_device": system,
            "future_key": { "kept": true },
        })
    }

    fn migrated(outcome: MigrationOutcome) -> Value {
        match outcome {
            MigrationOutcome::Migrated(value) => value,
            MigrationOutcome::Deferred => panic!("expected a migrated outcome"),
        }
    }

    #[test]
    fn version_detection() {
        assert_eq!(stored_version(&json!({ "save_folder": "/x" })), 1);
        assert_eq!(stored_version(&json!({ "schema_version": 2 })), 2);
        assert_eq!(stored_version(&json!({ "schema_version": 3 })), 3);
    }

    #[test]
    fn null_roles_migrate_even_with_unknown_inventory() {
        let out = migrated(migrate_v1(&v1(None, None), &unknown_inventory()));
        assert_eq!(out["schema_version"], 2);
        for key in [
            "preferred_mic_device",
            "preferred_mic_label",
            "preferred_system_device",
            "preferred_system_label",
            "unresolved_mic_device",
            "unresolved_system_device",
        ] {
            assert_eq!(out[key], Value::Null, "{}", key);
        }
    }

    #[test]
    fn valid_id_wins_even_when_equal_to_another_description() {
        let inv = inventory(
            vec![candidate("mic_a", "mic_b"), candidate("mic_b", "Other")],
            vec![],
        );
        let out = migrated(migrate_v1(&v1(Some("mic_b"), None), &inv));
        assert_eq!(out["preferred_mic_device"], "mic_b");
        assert_eq!(out["preferred_mic_label"], "label:mic_b");
        assert_eq!(out["unresolved_mic_device"], Value::Null);
    }

    #[test]
    fn unique_mic_description_migrates_to_id_and_label() {
        let inv = inventory(vec![candidate("mic_a", "USB Mic"), candidate("mic_b", "Built-in")], vec![]);
        let out = migrated(migrate_v1(&v1(Some("USB Mic"), None), &inv));
        assert_eq!(out["preferred_mic_device"], "mic_a");
        assert_eq!(out["preferred_mic_label"], "label:mic_a");
    }

    #[test]
    fn system_value_migrates_with_or_without_suffix() {
        let inv = inventory(vec![], vec![candidate("sink_a", "Speakers")]);
        let with = migrated(migrate_v1(&v1(None, Some("Speakers (System Audio)")), &inv));
        assert_eq!(with["preferred_system_device"], "sink_a");
        assert_eq!(with["preferred_system_label"], "label:sink_a");
        let without = migrated(migrate_v1(&v1(None, Some("Speakers")), &inv));
        assert_eq!(without["preferred_system_device"], "sink_a");
    }

    #[test]
    fn ambiguous_description_stays_unresolved_with_original_text() {
        let inv = inventory(
            vec![candidate("mic_a", "USB Mic"), candidate("mic_b", "USB Mic")],
            vec![],
        );
        let out = migrated(migrate_v1(&v1(Some("USB Mic"), None), &inv));
        assert_eq!(out["preferred_mic_device"], Value::Null);
        assert_eq!(out["unresolved_mic_device"], "USB Mic");
    }

    #[test]
    fn unresolved_system_keeps_non_normalized_value() {
        let inv = inventory(vec![], vec![candidate("sink_a", "Speakers")]);
        let out = migrated(migrate_v1(&v1(None, Some("Gone (System Audio)")), &inv));
        assert_eq!(out["preferred_system_device"], Value::Null);
        assert_eq!(out["unresolved_system_device"], "Gone (System Audio)");
    }

    #[test]
    fn no_match_is_unresolved() {
        let inv = inventory(vec![candidate("mic_a", "USB Mic")], vec![]);
        let out = migrated(migrate_v1(&v1(Some("default"), None), &inv));
        assert_eq!(out["preferred_mic_device"], Value::Null);
        assert_eq!(out["unresolved_mic_device"], "default");
    }

    #[test]
    fn value_matching_only_the_other_role_is_unresolved() {
        let inv = inventory(vec![candidate("mic_a", "USB Mic")], vec![candidate("sink_a", "Headset")]);
        let out = migrated(migrate_v1(&v1(Some("Headset"), None), &inv));
        assert_eq!(out["preferred_mic_device"], Value::Null);
        assert_eq!(out["unresolved_mic_device"], "Headset");
    }

    #[test]
    fn non_null_role_with_unknown_inventory_defers_everything() {
        let inv = MigrationInventory {
            microphone: RoleCandidates::Known(vec![candidate("mic_a", "USB Mic")]),
            system: RoleCandidates::Unknown,
        };
        assert_eq!(
            migrate_v1(&v1(Some("USB Mic"), Some("Speakers")), &inv),
            MigrationOutcome::Deferred
        );
    }

    #[test]
    fn general_fields_and_unknown_keys_are_copied() {
        let out = migrated(migrate_v1(&v1(None, None), &inventory(vec![], vec![])));
        assert_eq!(out["save_folder"], "/rec");
        assert_eq!(out["auto_save"], false);
        assert_eq!(out["file_format"], "wav");
        assert_eq!(out["future_key"], json!({ "kept": true }));
    }

    #[test]
    fn stamp_keeps_values_and_clears_labels() {
        let out = stamp_v1_as_v2(&v1(Some("Mic"), Some("Out")));
        assert_eq!(out["schema_version"], 2);
        assert_eq!(out["preferred_mic_device"], "Mic");
        assert_eq!(out["preferred_system_device"], "Out");
        assert_eq!(out["preferred_mic_label"], Value::Null);
        assert_eq!(out["preferred_system_label"], Value::Null);
        assert_eq!(out["unresolved_mic_device"], Value::Null);
        assert_eq!(out["unresolved_system_device"], Value::Null);
        assert_eq!(out["future_key"], json!({ "kept": true }));
    }

    #[test]
    fn deferred_view_keeps_legacy_values_as_unresolved() {
        let out = deferred_view(&v1(Some("USB Mic"), None));
        assert_eq!(out["schema_version"], 2);
        assert_eq!(out["preferred_mic_device"], Value::Null);
        assert_eq!(out["unresolved_mic_device"], "USB Mic");
        assert_eq!(out["unresolved_system_device"], Value::Null);
        let prefs: RecordingPreferences = serde_json::from_value(out).unwrap();
        assert_eq!(prefs.unresolved_mic_device.as_deref(), Some("USB Mic"));
    }

    fn v2_object() -> Value {
        json!({
            "schema_version": 2,
            "save_folder": "/old",
            "auto_save": true,
            "file_format": "mp4",
            "preferred_mic_device": "mic_a",
            "preferred_mic_label": "Mic A",
            "preferred_system_device": null,
            "preferred_system_label": null,
            "unresolved_mic_device": null,
            "unresolved_system_device": "Legacy sink",
            "future_key": 1,
        })
    }

    #[test]
    fn general_patch_preserves_device_fields_and_unknown_keys() {
        let mut prefs = RecordingPreferences::default();
        prefs.save_folder = PathBuf::from("/new");
        prefs.auto_save = false;
        prefs.preferred_mic_device = Some("ignored".into());
        prefs.unresolved_system_device = None;
        let out = patch_general_fields(&v2_object(), &prefs);
        assert_eq!(out["save_folder"], "/new");
        assert_eq!(out["auto_save"], false);
        assert_eq!(out["preferred_mic_device"], "mic_a");
        assert_eq!(out["preferred_mic_label"], "Mic A");
        assert_eq!(out["unresolved_system_device"], "Legacy sink");
        assert_eq!(out["future_key"], 1);
        assert_eq!(out["schema_version"], 2);
    }

    #[test]
    fn general_patch_leaves_a_v1_object_v1() {
        let prefs = RecordingPreferences::default();
        let out = patch_general_fields(&v1(Some("Old mic"), None), &prefs);
        assert!(out.get("schema_version").is_none());
        assert_eq!(stored_version(&out), 1);
        assert_eq!(out["preferred_mic_device"], "Old mic");
    }

    #[test]
    fn device_choice_touches_only_the_targeted_role() {
        let mut object = v2_object().as_object().cloned().unwrap();
        apply_device_choice(&mut object, "system", Some("sink_a"), Some("Speakers")).unwrap();
        assert_eq!(object["preferred_system_device"], "sink_a");
        assert_eq!(object["preferred_system_label"], "Speakers");
        assert_eq!(object["unresolved_system_device"], Value::Null);
        assert_eq!(object["preferred_mic_device"], "mic_a");
        assert_eq!(object["preferred_mic_label"], "Mic A");
    }

    #[test]
    fn default_choice_forces_label_to_null_and_clears_unresolved() {
        let mut object = v2_object().as_object().cloned().unwrap();
        apply_device_choice(&mut object, "system", None, Some("stale")).unwrap();
        assert_eq!(object["preferred_system_device"], Value::Null);
        assert_eq!(object["preferred_system_label"], Value::Null);
        assert_eq!(object["unresolved_system_device"], Value::Null);
    }

    #[test]
    fn invalid_role_is_an_error() {
        let mut object = v2_object().as_object().cloned().unwrap();
        assert!(apply_device_choice(&mut object, "mic", None, None).is_err());
        assert!(apply_device_choice(&mut object, "speaker", None, None).is_err());
    }

    #[test]
    fn backup_is_written_once_in_store_shape() {
        let dir = tempfile::tempdir().unwrap();
        let first = v1(Some("Original"), None);
        assert_eq!(
            write_backup_if_absent(dir.path(), V1_BACKUP_FILE, &first).unwrap(),
            BackupResult::Written
        );
        let content: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(V1_BACKUP_FILE)).unwrap()).unwrap();
        assert_eq!(content, json!({ "preferences": first }));

        let second = v1(Some("Changed"), None);
        assert_eq!(
            write_backup_if_absent(dir.path(), V1_BACKUP_FILE, &second).unwrap(),
            BackupResult::AlreadyPresent
        );
        let content: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(V1_BACKUP_FILE)).unwrap()).unwrap();
        assert_eq!(content["preferences"]["preferred_mic_device"], "Original");
        assert!(!dir.path().join(format!("{}.tmp", V1_BACKUP_FILE)).exists());
    }
}
