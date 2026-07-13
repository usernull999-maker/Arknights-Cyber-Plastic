#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arknights_super_contract::{AppBootstrap, StatusChanged, TimelineUpdate, PROTOCOL_VERSION};
use arknights_super_core::recorder::{
    enable_dpi_awareness, format_time_from_frames, parse_virtual_key, relaunch_as_admin_if_needed,
    run_calibration, start_recorder, AppEvent, BattleStateCode, CalibrationOptions,
    CalibrationUpdate, CaptureStateCode, ClockEvent, RecorderDetailPreview, RecorderHandle,
    RecorderOptions, RelativeRectF32, TileCoord, UserAction, CALIBRATION_CANCELLED,
    DEFAULT_OPER_NAME_ROI_RELATIVE_RECT, FRAMES_PER_SECOND,
};
use arknights_super_ruler_bridge::{
    inspect_profile, save_target_configuration, TargetConfiguration,
};
use base64::Engine;
use chrono::{Datelike, Local, Timelike};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessesToUpdate, System};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, LogicalSize, Manager, Size, State, WebviewWindow};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible,
};

const STAGE_NAME: &str = "墨染书阁";
const STATUS_EVENT: &str = "app://status";
const TIMELINE_EVENT: &str = "app://timeline";
const TIMELINE_INTERVAL: Duration = Duration::from_millis(50);
const FINALIZE_DEBOUNCE_MS: u64 = 1500;
const RUNTIME_RECOVER_RETRY_MS: u64 = 1000;
const LEGACY_STORAGE_IDENTIFIER: &str = "com.zhangjue.arknights-super";
const MAIN_WINDOW_DEFAULT_WIDTH: f64 = 404.0;
const MAIN_WINDOW_DEFAULT_HEIGHT: f64 = 860.0;
const MAIN_WINDOW_MIN_WIDTH: f64 = 360.0;
const MAIN_WINDOW_MIN_HEIGHT: f64 = 640.0;
const RESOURCE_ROOT_ENV: &str = "ARKNIGHTS_SUPER_RESOURCE_ROOT";

#[derive(Clone, Serialize, Deserialize)]
struct RoiBounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Default for RoiBounds {
    fn default() -> Self {
        Self::from_rect(DEFAULT_OPER_NAME_ROI_RELATIVE_RECT)
    }
}

impl RoiBounds {
    fn from_rect(rect: RelativeRectF32) -> Self {
        Self {
            left: rect.x,
            top: rect.y,
            right: (1.0 - rect.x - rect.width).max(0.0),
            bottom: (1.0 - rect.y - rect.height).max(0.0),
        }
    }

    fn normalized(&self) -> Self {
        let left = self.left.clamp(0.0, 0.95);
        let top = self.top.clamp(0.0, 0.95);
        let right = self.right.clamp(0.0, 0.95);
        let bottom = self.bottom.clamp(0.0, 0.95);
        let width = (1.0 - left - right).max(0.02);
        let height = (1.0 - top - bottom).max(0.02);
        Self {
            left,
            top,
            right: (1.0 - left - width).clamp(0.0, 0.98),
            bottom: (1.0 - top - height).clamp(0.0, 0.98),
        }
    }

    fn to_rect(&self) -> RelativeRectF32 {
        let normalized = self.normalized();
        RelativeRectF32 {
            x: normalized.left,
            y: normalized.top,
            width: (1.0 - normalized.left - normalized.right).max(0.02),
            height: (1.0 - normalized.top - normalized.bottom).max(0.02),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct AppSettings {
    config_path: String,
    skill_key: String,
    retreat_key: String,
    window_opacity: f64,
    always_on_top: bool,
    operation_prompt_enabled: bool,
    operation_prompt_lead_frames: i32,
    operator_name_roi: RoiBounds,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            config_path: "config.json".to_string(),
            skill_key: "SPACE".to_string(),
            retreat_key: "2".to_string(),
            window_opacity: 1.0,
            always_on_top: false,
            operation_prompt_enabled: false,
            operation_prompt_lead_frames: FRAMES_PER_SECOND * 3,
            operator_name_roi: RoiBounds::default(),
        }
    }
}

impl AppSettings {
    fn default_with_config_path(config_path: PathBuf) -> Self {
        Self {
            config_path: config_path.to_string_lossy().to_string(),
            ..Self::default()
        }
    }

    fn normalized(&self) -> Self {
        Self {
            config_path: self.config_path.trim().to_string(),
            skill_key: self.skill_key.trim().to_uppercase(),
            retreat_key: self.retreat_key.trim().to_uppercase(),
            window_opacity: self.window_opacity.clamp(0.20, 1.0),
            always_on_top: self.always_on_top,
            operation_prompt_enabled: self.operation_prompt_enabled,
            operation_prompt_lead_frames: self.operation_prompt_lead_frames.max(0),
            operator_name_roi: self.operator_name_roi.normalized(),
        }
    }

    fn to_recorder_options(&self, logs_root: PathBuf) -> Result<RecorderOptions, String> {
        let settings = self.normalized();
        Ok(RecorderOptions {
            config_path: PathBuf::from(settings.config_path),
            skill_vk: parse_optional_virtual_key(&settings.skill_key)?,
            retreat_vk: parse_optional_virtual_key(&settings.retreat_key)?,
            detail_name_roi: settings.operator_name_roi.to_rect(),
            visible_console: false,
            logs_root,
        })
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct RecordManifest {
    records: Vec<RecordMeta>,
}

#[derive(Clone, Serialize, Deserialize)]
struct RecordMeta {
    id: String,
    title: String,
    stage_name: String,
    file_name: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    deleted: bool,
    operation_count: usize,
    duration_frames: i32,
}

#[derive(Clone, Serialize)]
struct RecordSummary {
    id: String,
    title: String,
    stage_name: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    deleted: bool,
    operation_count: usize,
    duration_frames: i32,
}

#[derive(Clone, Serialize)]
struct UiOperationLine {
    index: usize,
    frames: i32,
    time: String,
    code: String,
    operator_name: String,
    tile: Option<TileCoord>,
    tile_label: String,
    line_text: String,
}

#[derive(Clone)]
struct ActiveBattle {
    id: String,
    title: String,
    stage_name: String,
    created_at_ms: u64,
    elapsed_frames: i32,
    battle_state: String,
    raw_content: String,
    operations: Vec<UiOperationLine>,
    pending_finalize_since: Option<Instant>,
}

#[derive(Clone, Serialize)]
struct CurrentBattleSnapshot {
    id: String,
    title: String,
    stage_name: String,
    created_at_ms: u64,
    elapsed_frames: i32,
    battle_state: String,
    raw_content: String,
    operations: Vec<UiOperationLine>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum SetupPhase {
    Ready,
    NeedsTarget,
    NeedsCalibration,
    InvalidConfig,
}

#[derive(Clone, Serialize)]
struct SetupTarget {
    fingerprint: String,
    name: String,
    detail: String,
}

#[derive(Clone, Serialize)]
struct SetupSnapshot {
    phase: SetupPhase,
    message: String,
    config_path: String,
    calibration_path: Option<String>,
    selected_target_fingerprint: Option<String>,
    targets: Vec<SetupTarget>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum CalibrationPhase {
    Idle,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Serialize)]
struct CalibrationSnapshot {
    phase: CalibrationPhase,
    progress_percent: f32,
    message: String,
    session_dir: Option<String>,
}

#[derive(Clone, Serialize)]
struct UiSnapshot {
    settings: AppSettings,
    setup: SetupSnapshot,
    calibration: CalibrationSnapshot,
    records: Vec<RecordSummary>,
    current_battle: Option<CurrentBattleSnapshot>,
    capture_state: String,
    logs_dir: String,
    recorder_error: Option<String>,
}

#[derive(Clone, Serialize)]
struct DetailPreviewSnapshot {
    elapsed_frames: i32,
    width: u32,
    height: u32,
    data_url: String,
}

#[derive(Serialize)]
struct RecordDocument {
    meta: RecordSummary,
    raw_content: String,
    operations: Vec<UiOperationLine>,
}

#[derive(Serialize)]
struct ImportRecordResponse {
    snapshot: UiSnapshot,
    document: RecordDocument,
}

#[derive(Clone)]
struct StoragePaths {
    settings_path: PathBuf,
    manifest_path: PathBuf,
    records_dir: PathBuf,
    logs_root: PathBuf,
    default_config_path: PathBuf,
}

struct ControllerInner {
    storage: StoragePaths,
    settings: AppSettings,
    manifest: RecordManifest,
    setup: SetupSnapshot,
    calibration: CalibrationSnapshot,
    current_battle: Option<ActiveBattle>,
    recorder_error: Option<String>,
    logs_dir: PathBuf,
    recorder_handle: Option<RecorderHandle>,
    calibration_cancel: Option<Arc<AtomicBool>>,
    calibration_thread: Option<thread::JoinHandle<()>>,
    last_runtime_recover_attempt: Option<Instant>,
    runtime_reconcile_in_progress: bool,
    runtime_session_id: Option<String>,
    timeline_sequence: u64,
}

struct AppController {
    app: AppHandle,
    inner: Mutex<ControllerInner>,
    shutting_down: AtomicBool,
    state_revision: AtomicU64,
    started_at: Instant,
}

#[derive(Clone)]
struct DiscoveredTarget {
    setup: SetupTarget,
    target: TargetConfiguration,
}

#[derive(Clone, Debug)]
struct ProcessInfo {
    name: String,
    process_id: u32,
    executable_path: Option<String>,
    command_line: Option<String>,
}

#[derive(Clone, Debug)]
struct CandidateWindowInfo {
    hwnd: isize,
    title: String,
    class_name: String,
}

const MUMU_DEVICE_LAYOUT_VERSIONS: &[&str] = &["12.0"];

impl AppController {
    fn bootstrap(app: AppHandle) -> Result<Arc<Self>, String> {
        configure_runtime_resource_root(&app);
        let storage = prepare_storage(&app)?;
        let settings = load_settings(&storage.settings_path, &storage.default_config_path)?;
        let manifest = load_manifest(&storage.manifest_path)?;
        let controller = Arc::new(Self {
            app,
            inner: Mutex::new(ControllerInner {
                storage,
                settings,
                manifest,
                setup: SetupSnapshot {
                    phase: SetupPhase::InvalidConfig,
                    message: "正在检查配置".to_string(),
                    config_path: String::new(),
                    calibration_path: None,
                    selected_target_fingerprint: None,
                    targets: Vec::new(),
                },
                calibration: CalibrationSnapshot {
                    phase: CalibrationPhase::Idle,
                    progress_percent: 0.0,
                    message: "尚未开始校准".to_string(),
                    session_dir: None,
                },
                current_battle: None,
                recorder_error: None,
                logs_dir: PathBuf::new(),
                recorder_handle: None,
                calibration_cancel: None,
                calibration_thread: None,
                last_runtime_recover_attempt: None,
                runtime_reconcile_in_progress: false,
                runtime_session_id: None,
                timeline_sequence: 0,
            }),
            shutting_down: AtomicBool::new(false),
            state_revision: AtomicU64::new(0),
            started_at: Instant::now(),
        });
        controller.emit_snapshot();
        Ok(controller)
    }

    fn snapshot(&self) -> UiSnapshot {
        let mut inner = self.inner.lock().expect("app controller poisoned");
        sync_live_clock_locked(&mut inner);
        build_snapshot(&inner)
    }

    fn bootstrap_snapshot(&self) -> AppBootstrap<UiSnapshot> {
        let session_id = self
            .inner
            .lock()
            .expect("app controller poisoned")
            .runtime_session_id
            .clone();
        AppBootstrap {
            protocol_version: PROTOCOL_VERSION,
            revision: self.state_revision.load(Ordering::Acquire),
            session_id,
            state: self.snapshot(),
        }
    }

    fn maybe_recover_runtime(self: &Arc<Self>) {
        let should_retry = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            if inner.recorder_handle.is_some() {
                return;
            }
            if inner.runtime_reconcile_in_progress {
                return;
            }
            if !matches!(inner.setup.phase, SetupPhase::Ready) {
                return;
            }
            if calibration_is_active(&inner.calibration) || inner.calibration_cancel.is_some() {
                return;
            }
            if !inner
                .recorder_error
                .as_deref()
                .is_some_and(is_window_missing_runtime_error)
            {
                return;
            }
            let now = Instant::now();
            if inner.last_runtime_recover_attempt.is_some_and(|last| {
                now.duration_since(last) < Duration::from_millis(RUNTIME_RECOVER_RETRY_MS)
            }) {
                return;
            }
            inner.last_runtime_recover_attempt = Some(now);
            true
        };

        if should_retry {
            self.request_reconcile_runtime();
        }
    }

    fn request_reconcile_runtime(self: &Arc<Self>) {
        let should_spawn = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            if inner.runtime_reconcile_in_progress {
                false
            } else {
                inner.runtime_reconcile_in_progress = true;
                true
            }
        };
        if !should_spawn {
            return;
        }

        let controller = Arc::clone(self);
        thread::spawn(move || {
            let result = controller.reconcile_runtime();
            {
                let mut inner = controller.inner.lock().expect("app controller poisoned");
                inner.runtime_reconcile_in_progress = false;
            }
            if let Err(error) = result {
                controller.set_recorder_error(Some(error));
                controller.emit_snapshot();
            }
        });
    }

    fn emit_snapshot(&self) {
        let snapshot = self.snapshot();
        let revision = self.state_revision.fetch_add(1, Ordering::AcqRel) + 1;
        let session_id = self
            .inner
            .lock()
            .expect("app controller poisoned")
            .runtime_session_id
            .clone();
        let _ = self.app.emit(
            STATUS_EVENT,
            StatusChanged {
                revision,
                session_id,
                state: snapshot,
            },
        );
    }

    fn emit_timeline(&self) {
        let update = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            let Some(session_id) = inner.runtime_session_id.clone() else {
                return;
            };
            let Some(handle) = inner.recorder_handle.as_ref() else {
                return;
            };
            let clock = handle.clock_snapshot();
            inner.timeline_sequence = inner.timeline_sequence.saturating_add(1);
            TimelineUpdate {
                session_id,
                sequence: inner.timeline_sequence,
                observed_at_ns: u64::try_from(self.started_at.elapsed().as_nanos())
                    .unwrap_or(u64::MAX),
                elapsed_frames: clock.elapsed_frames,
                logical_frame: clock.logical_frame,
                battle_state: clock.battle_state.as_str().to_string(),
                capture_state: clock.capture_state.as_str().to_string(),
            }
        };
        let _ = self.app.emit(TIMELINE_EVENT, update);
    }

    fn update_calibration(&self, next: CalibrationSnapshot) {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.calibration = next;
        }
        self.emit_snapshot();
    }

    fn note_calibration_update(&self, update: CalibrationUpdate) {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.logs_dir = update
                .session_dir
                .as_ref()
                .cloned()
                .unwrap_or_else(|| inner.logs_dir.clone());
            inner.calibration = CalibrationSnapshot {
                phase: match inner.calibration.phase {
                    CalibrationPhase::Cancelling => CalibrationPhase::Cancelling,
                    _ => CalibrationPhase::Running,
                },
                progress_percent: update.progress_percent.clamp(0.0, 100.0),
                message: update.message,
                session_dir: update
                    .session_dir
                    .map(|path| path.to_string_lossy().to_string())
                    .or_else(|| inner.calibration.session_dir.clone()),
            };
        }
        self.emit_snapshot();
    }

    fn start_calibration(self: &Arc<Self>) -> Result<(), String> {
        let (settings, storage, cancel, previous_recorder) = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            ensure_calibration_idle_locked(&mut inner)?;
            inner.current_battle = None;
            inner.recorder_error = None;
            inner.calibration = CalibrationSnapshot {
                phase: CalibrationPhase::Running,
                progress_percent: 0.0,
                message: "正在启动校准".to_string(),
                session_dir: None,
            };
            let cancel = Arc::new(AtomicBool::new(false));
            inner.calibration_cancel = Some(Arc::clone(&cancel));
            (
                inner.settings.clone(),
                inner.storage.clone(),
                cancel,
                inner.recorder_handle.take(),
            )
        };

        let controller = Arc::clone(self);
        let worker = thread::spawn(move || {
            if let Some(handle) = previous_recorder {
                controller.update_calibration(CalibrationSnapshot {
                    phase: CalibrationPhase::Running,
                    progress_percent: 0.0,
                    message: "正在关闭当前捕捉".to_string(),
                    session_dir: None,
                });
                if let Err(error) = handle.shutdown() {
                    controller.finish_calibration(Err(error));
                    return;
                }
            }
            if cancel.load(Ordering::SeqCst) {
                controller.finish_calibration(Err(CALIBRATION_CANCELLED.to_string()));
                return;
            }
            let result = run_calibration(
                CalibrationOptions {
                    config_path: PathBuf::from(settings.config_path),
                    logs_root: storage.logs_root.clone(),
                },
                |update| controller.note_calibration_update(update),
                || cancel.load(Ordering::SeqCst),
            );
            controller.finish_calibration(result);
        });

        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.calibration_thread = Some(worker);
        }

        self.emit_snapshot();
        Ok(())
    }

    fn cancel_calibration(&self) -> Result<(), String> {
        let next = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            ensure_calibration_thread_state_locked(&mut inner)?;
            let Some(cancel) = inner.calibration_cancel.as_ref() else {
                return Err("当前没有正在进行的校准".to_string());
            };
            cancel.store(true, Ordering::SeqCst);
            CalibrationSnapshot {
                phase: CalibrationPhase::Cancelling,
                progress_percent: inner.calibration.progress_percent,
                message: "正在取消校准".to_string(),
                session_dir: inner.calibration.session_dir.clone(),
            }
        };
        self.update_calibration(next);
        Ok(())
    }

    fn finish_calibration(
        self: &Arc<Self>,
        result: Result<arknights_super_core::recorder::CalibrationOutcome, String>,
    ) {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.calibration_cancel = None;
            inner.calibration = match result {
                Ok(ref outcome) => CalibrationSnapshot {
                    phase: CalibrationPhase::Succeeded,
                    progress_percent: 100.0,
                    message: "校准完成".to_string(),
                    session_dir: Some(outcome.session_dir.to_string_lossy().to_string()),
                },
                Err(ref error) if error == CALIBRATION_CANCELLED => CalibrationSnapshot {
                    phase: CalibrationPhase::Cancelled,
                    progress_percent: inner.calibration.progress_percent,
                    message: "校准已取消".to_string(),
                    session_dir: inner.calibration.session_dir.clone(),
                },
                Err(ref error) => CalibrationSnapshot {
                    phase: CalibrationPhase::Failed,
                    progress_percent: inner.calibration.progress_percent,
                    message: error.clone(),
                    session_dir: inner.calibration.session_dir.clone(),
                },
            };
        }
        self.emit_snapshot();
        self.request_reconcile_runtime();
    }

    fn apply_window_settings(&self, window: &WebviewWindow) -> Result<(), String> {
        let settings = {
            let inner = self.inner.lock().expect("app controller poisoned");
            inner.settings.clone()
        };
        window
            .set_min_size(Some(Size::Logical(LogicalSize::new(
                MAIN_WINDOW_MIN_WIDTH,
                MAIN_WINDOW_MIN_HEIGHT,
            ))))
            .map_err(|e| format!("failed to set min window size: {e}"))?;
        let current_size = window
            .inner_size()
            .map_err(|e| format!("failed to get current window size: {e}"))?;
        if current_size.width < MAIN_WINDOW_DEFAULT_WIDTH as u32
            || current_size.height < MAIN_WINDOW_DEFAULT_HEIGHT as u32
        {
            window
                .set_size(Size::Logical(LogicalSize::new(
                    MAIN_WINDOW_DEFAULT_WIDTH,
                    MAIN_WINDOW_DEFAULT_HEIGHT,
                )))
                .map_err(|e| format!("failed to set default window size: {e}"))?;
        }
        window
            .set_always_on_top(settings.always_on_top)
            .map_err(|e| format!("failed to set always-on-top: {e}"))?;
        Ok(())
    }

    fn set_always_on_top(&self, window: &WebviewWindow, value: bool) -> Result<(), String> {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.settings.always_on_top = value;
            save_settings(&inner.storage.settings_path, &inner.settings)?;
        }
        window
            .set_always_on_top(value)
            .map_err(|e| format!("failed to set always-on-top: {e}"))?;
        self.emit_snapshot();
        Ok(())
    }

    fn save_settings_and_reconcile(
        self: &Arc<Self>,
        window: &WebviewWindow,
        next_settings: AppSettings,
    ) -> Result<(), String> {
        let normalized = next_settings.normalized();
        let changed = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            ensure_calibration_idle_locked(&mut inner)?;
            let changed = inner.settings.config_path != normalized.config_path
                || inner.settings.skill_key != normalized.skill_key
                || inner.settings.retreat_key != normalized.retreat_key
                || inner.settings.operator_name_roi.left != normalized.operator_name_roi.left
                || inner.settings.operator_name_roi.top != normalized.operator_name_roi.top
                || inner.settings.operator_name_roi.right != normalized.operator_name_roi.right
                || inner.settings.operator_name_roi.bottom != normalized.operator_name_roi.bottom;
            inner.settings = normalized.clone();
            save_settings(&inner.storage.settings_path, &inner.settings)?;
            changed
        };

        window
            .set_always_on_top(normalized.always_on_top)
            .map_err(|e| format!("failed to set always-on-top: {e}"))?;

        if changed {
            self.emit_snapshot();
            self.request_reconcile_runtime();
        } else {
            self.emit_snapshot();
        }
        Ok(())
    }

    fn reconcile_runtime(self: &Arc<Self>) -> Result<(), String> {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            ensure_calibration_thread_state_locked(&mut inner)?;
        }

        let old_handle = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.current_battle = None;
            inner.recorder_error = None;
            inner.last_runtime_recover_attempt = Some(Instant::now());
            inner.runtime_session_id = None;
            inner.recorder_handle.take()
        };

        if let Some(handle) = old_handle {
            let _ = handle.shutdown();
        }

        let (settings, storage) = {
            let inner = self.inner.lock().expect("app controller poisoned");
            (inner.settings.clone(), inner.storage.clone())
        };

        let setup = inspect_setup(&settings);
        let targets = discover_setup_targets();

        let calibration_active = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.setup = SetupSnapshot { targets, ..setup };
            inner.logs_dir = storage.logs_root.clone();
            calibration_is_active(&inner.calibration) || inner.calibration_cancel.is_some()
        };

        if calibration_active {
            self.emit_snapshot();
            return Ok(());
        }

        if !matches!(
            self.inner
                .lock()
                .expect("app controller poisoned")
                .setup
                .phase,
            SetupPhase::Ready
        ) {
            self.emit_snapshot();
            return Ok(());
        }

        let recorder_options = match settings.to_recorder_options(storage.logs_root.clone()) {
            Ok(options) => options,
            Err(error) => {
                self.set_recorder_error(Some(error.clone()));
                self.emit_snapshot();
                return Err(error);
            }
        };

        {
            let inner = self.inner.lock().expect("app controller poisoned");
            if calibration_is_active(&inner.calibration) || inner.calibration_cancel.is_some() {
                self.emit_snapshot();
                return Ok(());
            }
        }

        match start_recorder(recorder_options) {
            Ok(launch) => {
                {
                    let mut inner = self.inner.lock().expect("app controller poisoned");
                    inner.logs_dir = launch.log_paths.session_dir.clone();
                    inner.recorder_error = None;
                    inner.recorder_handle = Some(launch.handle);
                    inner.last_runtime_recover_attempt = None;
                    inner.timeline_sequence = 0;
                    inner.runtime_session_id = Some(format!(
                        "runtime-{}-{}",
                        now_millis(),
                        self.state_revision.load(Ordering::Acquire)
                    ));
                }
                self.emit_snapshot();
                let controller = Arc::clone(self);
                thread::spawn(move || controller.run_recorder_loop(launch.receiver));
                Ok(())
            }
            Err(error) => {
                let normalized = normalize_runtime_error_message(&error);
                self.set_recorder_error(Some(normalized.clone()));
                self.emit_snapshot();
                Err(normalized)
            }
        }
    }

    fn run_recorder_loop(self: Arc<Self>, receiver: crossbeam_channel::Receiver<AppEvent>) {
        let mut next_timeline_emit = Instant::now();
        loop {
            let timeout = next_timeline_emit.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(timeout) {
                Ok(event) => {
                    self.handle_recorder_event(event);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if let Err(error) = self.finalize_if_inactive() {
                        self.set_recorder_error(Some(error));
                        self.emit_snapshot();
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    let _ = self.finalize_if_inactive();
                    break;
                }
            }
            if Instant::now() >= next_timeline_emit {
                self.emit_timeline();
                next_timeline_emit = Instant::now() + TIMELINE_INTERVAL;
            }
        }
    }

    fn handle_recorder_event(&self, event: AppEvent) {
        {
            let inner = self.inner.lock().expect("app controller poisoned");
            if calibration_is_active(&inner.calibration) || inner.calibration_cancel.is_some() {
                return;
            }
        }
        match event {
            AppEvent::Clock(clock) => self.handle_clock(clock),
            AppEvent::Operation(operation) => {
                if let Some(action) = operation.to_user_action() {
                    self.handle_user_action(action, operation.battle_state);
                }
            }
            AppEvent::Warning(message) => {
                self.set_recorder_error(Some(message));
                self.emit_snapshot();
            }
            AppEvent::Info(_) | AppEvent::RawInput(_) => {}
        }
    }

    fn handle_clock(&self, clock: ClockEvent) {
        let emit = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            if inner
                .recorder_error
                .as_deref()
                .is_some_and(|message| message.starts_with("capture failed:"))
            {
                inner.recorder_error = None;
            }
            apply_clock_update_locked(&mut inner, clock)
        };
        if emit {
            self.emit_snapshot();
        }
    }

    fn handle_user_action(&self, action: UserAction, battle_state: BattleStateCode) {
        let mut emit = false;
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            if inner.current_battle.is_none() {
                start_new_battle_locked(&mut inner, action.elapsed_frames, battle_state);
            }
            if let Some(battle) = inner.current_battle.as_mut() {
                battle.elapsed_frames = battle.elapsed_frames.max(action.elapsed_frames);
                battle.battle_state = battle_state.as_str().to_string();
                battle.pending_finalize_since = None;
                let next_index = battle.operations.len();
                let line = user_action_to_ui_line(next_index, &action);
                battle.raw_content.push_str(&line.line_text);
                battle.raw_content.push('\n');
                battle.operations.push(line);
                emit = true;
            }
        }
        if emit {
            self.emit_snapshot();
        }
    }

    fn finalize_if_inactive(&self) -> Result<(), String> {
        let should_finalize = {
            let inner = self.inner.lock().expect("app controller poisoned");
            let pending_ocr = inner
                .recorder_handle
                .as_ref()
                .map(|handle| handle.pending_ocr_count())
                .unwrap_or(0);
            inner
                .current_battle
                .as_ref()
                .and_then(|battle| battle.pending_finalize_since)
                .is_some_and(|pending| {
                    pending.elapsed() >= Duration::from_millis(FINALIZE_DEBOUNCE_MS)
                        && pending_ocr == 0
                })
        };
        if !should_finalize {
            return Ok(());
        }

        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            finalize_battle_locked(&mut inner)?;
        }
        self.emit_snapshot();
        Ok(())
    }

    fn load_record_document(&self, record_id: &str) -> Result<RecordDocument, String> {
        let (meta, path) = {
            let inner = self.inner.lock().expect("app controller poisoned");
            let meta = inner
                .manifest
                .records
                .iter()
                .find(|record| record.id == record_id)
                .cloned()
                .ok_or_else(|| format!("record '{record_id}' not found"))?;
            let path = inner.storage.records_dir.join(&meta.file_name);
            (meta, path)
        };

        let raw_content = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
        let operations = parse_record_content(&raw_content);
        Ok(RecordDocument {
            meta: summarize_record(&meta),
            raw_content,
            operations,
        })
    }

    fn save_record(
        &self,
        record_id: &str,
        title: String,
        raw_content: String,
    ) -> Result<(), String> {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            let storage = inner.storage.clone();
            let record = inner
                .manifest
                .records
                .iter_mut()
                .find(|record| record.id == record_id)
                .ok_or_else(|| format!("record '{record_id}' not found"))?;
            let parsed = parse_record_content(&raw_content);
            record.title = title.trim().to_string();
            record.updated_at_ms = now_millis();
            record.operation_count = parsed.len();
            record.duration_frames = parsed.last().map(|line| line.frames).unwrap_or(0);
            let path = storage.records_dir.join(&record.file_name);
            fs::write(&path, raw_content.as_bytes())
                .map_err(|e| format!("failed to write '{}': {e}", path.display()))?;
            save_manifest(&storage.manifest_path, &inner.manifest)?;
        }
        self.emit_snapshot();
        Ok(())
    }

    fn import_record(&self, title: String, raw_content: String) -> Result<RecordDocument, String> {
        let document = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            let storage = inner.storage.clone();
            let created_at_ms = now_millis();
            let record_id = unique_record_id(&inner.manifest, "record", created_at_ms);
            let normalized_title = title.trim().to_string();
            let parsed = parse_record_content(&raw_content);
            let file_name = unique_record_file_name(&storage.records_dir, &normalized_title);
            let path = storage.records_dir.join(&file_name);
            fs::write(&path, raw_content.as_bytes())
                .map_err(|e| format!("failed to write '{}': {e}", path.display()))?;

            let meta = RecordMeta {
                id: record_id,
                title: normalized_title,
                stage_name: STAGE_NAME.to_string(),
                file_name,
                created_at_ms,
                updated_at_ms: created_at_ms,
                deleted: false,
                operation_count: parsed.len(),
                duration_frames: parsed.last().map(|line| line.frames).unwrap_or(0),
            };
            inner.manifest.records.push(meta.clone());
            save_manifest(&storage.manifest_path, &inner.manifest)?;

            RecordDocument {
                meta: summarize_record(&meta),
                raw_content,
                operations: parsed,
            }
        };
        self.emit_snapshot();
        Ok(document)
    }

    fn save_current_battle_text(
        &self,
        battle_id: &str,
        raw_content: String,
        base_snapshot_text: String,
    ) -> Result<(), String> {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            let battle = inner
                .current_battle
                .as_mut()
                .ok_or_else(|| "current battle not found".to_string())?;
            if battle.id != battle_id {
                return Err(format!("current battle '{battle_id}' not found"));
            }
            if battle.raw_content == base_snapshot_text {
                battle.raw_content = raw_content;
            } else if battle.raw_content.starts_with(&base_snapshot_text) {
                let suffix = &battle.raw_content[base_snapshot_text.len()..];
                battle.raw_content = format!("{raw_content}{suffix}");
            } else {
                battle.raw_content = raw_content;
            }
        }
        self.emit_snapshot();
        Ok(())
    }

    fn set_record_deleted(&self, record_id: &str, deleted: bool) -> Result<(), String> {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            let storage = inner.storage.clone();
            let record = inner
                .manifest
                .records
                .iter_mut()
                .find(|record| record.id == record_id)
                .ok_or_else(|| format!("record '{record_id}' not found"))?;
            record.deleted = deleted;
            record.updated_at_ms = now_millis();
            save_manifest(&storage.manifest_path, &inner.manifest)?;
        }
        self.emit_snapshot();
        Ok(())
    }

    fn set_recorder_error(&self, error: Option<String>) {
        let mut inner = self.inner.lock().expect("app controller poisoned");
        inner.recorder_error = error;
    }

    fn detail_preview(&self, roi_bounds: RoiBounds) -> Option<DetailPreviewSnapshot> {
        let preview = {
            let inner = self.inner.lock().expect("app controller poisoned");
            let handle = inner.recorder_handle.as_ref()?;
            handle.detail_name_preview(roi_bounds.normalized().to_rect())?
        };
        detail_preview_snapshot(preview).ok()
    }

    fn save_selected_target(self: &Arc<Self>, fingerprint: &str) -> Result<(), String> {
        let (settings, config_path) = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            ensure_calibration_idle_locked(&mut inner)?;
            (
                inner.settings.clone(),
                PathBuf::from(inner.settings.config_path.clone()),
            )
        };
        let target = discover_setup_target_configs()
            .into_iter()
            .find(|candidate| candidate.setup.fingerprint == fingerprint)
            .ok_or_else(|| format!("target '{fingerprint}' not found"))?;
        save_target_configuration(&config_path, &target.target)
            .map_err(|error| error.to_string())?;

        let normalized = settings.normalized();
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.settings = normalized;
            save_settings(&inner.storage.settings_path, &inner.settings)?;
        }
        self.emit_snapshot();
        self.request_reconcile_runtime();
        Ok(())
    }

    fn refresh_setup(self: &Arc<Self>) -> Result<(), String> {
        {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            ensure_calibration_idle_locked(&mut inner)?;
        }
        self.emit_snapshot();
        self.request_reconcile_runtime();
        Ok(())
    }

    fn shutdown(&self) {
        let (handle, calibration_cancel, calibration_thread) = {
            let mut inner = self.inner.lock().expect("app controller poisoned");
            inner.runtime_session_id = None;
            (
                inner.recorder_handle.take(),
                inner.calibration_cancel.take(),
                inner.calibration_thread.take(),
            )
        };
        if let Some(handle) = handle {
            let _ = handle.shutdown();
        }
        if let Some(cancel) = calibration_cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        if let Some(thread) = calibration_thread {
            let _ = thread.join();
        }
    }

    fn request_shutdown(self: &Arc<Self>) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }

        let controller = Arc::clone(self);
        let app = self.app.clone();
        thread::spawn(move || {
            controller.shutdown();
            app.exit(0);
        });
    }
}

fn configure_runtime_resource_root(app: &AppHandle) {
    let resolved = app
        .path()
        .resolve(".", BaseDirectory::Resource)
        .ok()
        .or_else(|| app.path().resource_dir().ok());
    let Some(path) = resolved else {
        return;
    };
    if path.exists() {
        std::env::set_var(RESOURCE_ROOT_ENV, &path);
    }
}

fn calibration_is_active(snapshot: &CalibrationSnapshot) -> bool {
    matches!(
        snapshot.phase,
        CalibrationPhase::Running | CalibrationPhase::Cancelling
    )
}

fn ensure_calibration_thread_state_locked(inner: &mut ControllerInner) -> Result<(), String> {
    if inner
        .calibration_thread
        .as_ref()
        .is_some_and(thread::JoinHandle::is_finished)
    {
        let handle = inner
            .calibration_thread
            .take()
            .expect("finished calibration thread must exist");
        handle
            .join()
            .map_err(|_| "calibration thread panicked".to_string())?;
    }
    Ok(())
}

fn ensure_calibration_idle_locked(inner: &mut ControllerInner) -> Result<(), String> {
    ensure_calibration_thread_state_locked(inner)?;
    if calibration_is_active(&inner.calibration) || inner.calibration_cancel.is_some() {
        return Err("校准进行中，请先等待完成或取消".to_string());
    }
    Ok(())
}

fn inspect_setup(settings: &AppSettings) -> SetupSnapshot {
    let config_path = PathBuf::from(settings.config_path.trim());
    let config_path_text = config_path.to_string_lossy().to_string();
    if !config_path.is_file() {
        return SetupSnapshot {
            phase: SetupPhase::NeedsTarget,
            message: "还没有基础配置。请先选择明日方舟窗口或 MuMu 模拟器。".to_string(),
            config_path: config_path_text,
            calibration_path: None,
            selected_target_fingerprint: None,
            targets: Vec::new(),
        };
    }

    let profile = match inspect_profile(&config_path) {
        Ok(profile) => profile,
        Err(error) => {
            return SetupSnapshot {
                phase: SetupPhase::InvalidConfig,
                message: error.to_string(),
                config_path: config_path_text,
                calibration_path: None,
                selected_target_fingerprint: None,
                targets: Vec::new(),
            };
        }
    };

    if profile.capture_type != "window" && profile.capture_type != "mumu" {
        return SetupSnapshot {
            phase: SetupPhase::InvalidConfig,
            message: "当前版本只支持 PC 窗口捕获和 MuMu 原生捕获，请重新选择 Windows 明日方舟窗口或 MuMu 模拟器。".to_string(),
            config_path: config_path_text,
            calibration_path: None,
            selected_target_fingerprint: profile.target_fingerprint.clone(),
            targets: Vec::new(),
        };
    }

    let Some(calibration_path) = profile.calibration_path else {
        let calibration_dir = config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("calibration");
        return SetupSnapshot {
            phase: SetupPhase::NeedsCalibration,
            message: "目标窗口已经配置，但还没有校准 profile。请像费用尺一样先完成一次校准。"
                .to_string(),
            config_path: config_path_text,
            calibration_path: Some(calibration_dir.to_string_lossy().to_string()),
            selected_target_fingerprint: profile.target_fingerprint.clone(),
            targets: Vec::new(),
        };
    };

    if !calibration_path.is_file() {
        return SetupSnapshot {
            phase: SetupPhase::NeedsCalibration,
            message: "基础配置已经存在，但指定的 calibration profile 文件不存在。".to_string(),
            config_path: config_path_text,
            calibration_path: Some(calibration_path.to_string_lossy().to_string()),
            selected_target_fingerprint: profile.target_fingerprint.clone(),
            targets: Vec::new(),
        };
    }

    SetupSnapshot {
        phase: SetupPhase::Ready,
        message: "基础配置已就绪。".to_string(),
        config_path: config_path_text,
        calibration_path: Some(calibration_path.to_string_lossy().to_string()),
        selected_target_fingerprint: profile.target_fingerprint,
        targets: Vec::new(),
    }
}

fn discover_setup_targets() -> Vec<SetupTarget> {
    discover_setup_target_configs()
        .into_iter()
        .map(|candidate| candidate.setup)
        .collect()
}

fn discover_setup_target_configs() -> Vec<DiscoveredTarget> {
    let processes = query_processes();
    let mut targets = Vec::new();
    discover_mumu_setup_targets(&processes, &mut targets);
    for process in processes
        .iter()
        .filter(|process| process.name.eq_ignore_ascii_case("Arknights.exe"))
    {
        for window in windows_for_pid(process.process_id) {
            let executable = process.executable_path.as_deref().unwrap_or_default();
            let fingerprint = format!(
                "window:{}:{}:{}",
                stable_path(Path::new(executable)),
                window.title,
                window.class_name
            );
            let target = build_window_target(
                Some(window.hwnd),
                Some(window.title.clone()),
                Some(window.class_name.clone()),
                fingerprint.clone(),
            );
            targets.push(DiscoveredTarget {
                setup: SetupTarget {
                    fingerprint,
                    name: "Windows 明日方舟".to_string(),
                    detail: format!("{} [{}]", window.title, window.class_name),
                },
                target,
            });
        }
    }
    targets.sort_by(|lhs, rhs| {
        lhs.setup
            .name
            .cmp(&rhs.setup.name)
            .then(lhs.setup.detail.cmp(&rhs.setup.detail))
    });
    targets.dedup_by(|lhs, rhs| lhs.setup.fingerprint == rhs.setup.fingerprint);
    targets
}

fn build_window_target(
    window_handle: Option<isize>,
    window_title: Option<String>,
    window_class: Option<String>,
    fingerprint: String,
) -> TargetConfiguration {
    TargetConfiguration::Window {
        fingerprint,
        window_handle,
        window_title,
        window_class,
    }
}

fn discover_mumu_setup_targets(processes: &[ProcessInfo], targets: &mut Vec<DiscoveredTarget>) {
    for process in processes
        .iter()
        .filter(|process| is_mumu_process_name(&process.name))
    {
        let Some(install_path) = process
            .executable_path
            .as_deref()
            .and_then(resolve_mumu_install_path)
        else {
            continue;
        };
        let instance_index = parse_mumu_instance(process).unwrap_or(0);
        let fingerprint = format!("mumu:{}:{}", stable_path(&install_path), instance_index);
        let detail = format!("MuMu #{} | {}", instance_index, install_path.display());
        targets.push(DiscoveredTarget {
            setup: SetupTarget {
                fingerprint: fingerprint.clone(),
                name: "MuMu 模拟器".to_string(),
                detail,
            },
            target: build_mumu_target(install_path, instance_index, fingerprint),
        });
    }
}

fn build_mumu_target(
    install_path: PathBuf,
    instance_index: u32,
    fingerprint: String,
) -> TargetConfiguration {
    TargetConfiguration::Mumu {
        fingerprint,
        install_path,
        instance_index,
    }
}

fn is_mumu_process_name(name: &str) -> bool {
    [
        "MuMuPlayer.exe",
        "MuMuNxDevice.exe",
        "MuMuVMMHeadless.exe",
        "NemuPlayer.exe",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn resolve_mumu_install_path(executable_path: &str) -> Option<PathBuf> {
    let mut current = Path::new(executable_path).parent()?;
    let mut found = None;
    loop {
        if mumu_dll_exists(current) {
            found = Some(current.to_path_buf());
        }
        let Some(parent) = current.parent() else {
            return found;
        };
        current = parent;
    }
}

fn mumu_dll_exists(base: &Path) -> bool {
    MUMU_DEVICE_LAYOUT_VERSIONS
        .iter()
        .map(|version| format!("nx_device/{version}/shell/sdk/external_renderer_ipc.dll"))
        .chain(
            [
                "nx_main/sdk/external_renderer_ipc.dll",
                "shell/sdk/external_renderer_ipc.dll",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .any(|relative| base.join(relative).exists())
}

fn parse_mumu_instance(process: &ProcessInfo) -> Option<u32> {
    let text = format!(
        "{} {}",
        process.command_line.as_deref().unwrap_or_default(),
        process.executable_path.as_deref().unwrap_or_default()
    );
    parse_named_number(&text, &["instance", "instance_index", "index", "playerid"]).or_else(|| {
        MUMU_DEVICE_LAYOUT_VERSIONS
            .iter()
            .find_map(|version| parse_trailing_segment_number(&text, &format!("{version}-")))
    })
}

fn parse_named_number(text: &str, names: &[&str]) -> Option<u32> {
    for name in names {
        let patterns = [
            format!("{name}="),
            format!("{name}:"),
            format!("--{name}="),
            format!("/{name}="),
        ];
        for pattern in patterns {
            let Some(found) = text.find(&pattern) else {
                continue;
            };
            let start = found + pattern.len();
            let digits = text[start..]
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            if let Ok(value) = digits.parse::<u32>() {
                return Some(value);
            }
        }
    }
    None
}

fn parse_trailing_segment_number(text: &str, marker: &str) -> Option<u32> {
    let start = text.find(marker)? + marker.len();
    let digits = text[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse::<u32>().ok()
}

fn stable_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn query_processes() -> Vec<ProcessInfo> {
    let mut system = System::new_all();
    let _ = system.refresh_processes(ProcessesToUpdate::All, true);
    system
        .processes()
        .values()
        .map(|process| ProcessInfo {
            name: process.name().to_string_lossy().into_owned(),
            process_id: process.pid().as_u32(),
            executable_path: process
                .exe()
                .map(|path| path.to_string_lossy().into_owned()),
            command_line: (!process.cmd().is_empty()).then(|| {
                process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
        })
        .collect()
}

fn windows_for_pid(pid: u32) -> Vec<CandidateWindowInfo> {
    let mut context = WindowSearchContext {
        pid,
        windows: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_window_proc),
            LPARAM((&mut context as *mut WindowSearchContext) as isize),
        );
    }
    context.windows.sort_by_key(window_priority);
    context.windows
}

struct WindowSearchContext {
    pid: u32,
    windows: Vec<CandidateWindowInfo>,
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let context = &mut *(lparam.0 as *mut WindowSearchContext);
    let mut pid = 0u32;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != context.pid {
        return BOOL(1);
    }
    let title = window_text(hwnd);
    if title.trim().is_empty() {
        return BOOL(1);
    }
    context.windows.push(CandidateWindowInfo {
        hwnd: hwnd.0 as isize,
        title,
        class_name: window_class(hwnd),
    });
    BOOL(1)
}

fn window_priority(candidate: &CandidateWindowInfo) -> (u8, String) {
    let title = candidate.title.to_ascii_lowercase();
    let class_name = candidate.class_name.to_ascii_lowercase();
    let priority = if candidate.title == "明日方舟" {
        0
    } else if candidate.title.contains("明日方舟") {
        1
    } else if title.contains("arknights") {
        2
    } else if class_name == "unitywndclass" || class_name == "unityhwndclass" {
        3
    } else {
        4
    };
    (priority, candidate.title.clone())
}

unsafe fn window_text(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let count = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..count as usize])
}

unsafe fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let count = GetClassNameW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..count as usize])
}

fn prepare_storage(app: &AppHandle) -> Result<StoragePaths, String> {
    let root_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("failed to resolve app local data dir: {e}"))?;
    migrate_legacy_storage_dir(&root_dir)?;
    let settings_path = root_dir.join("app-settings.json");
    let manifest_path = root_dir.join("records").join("index.json");
    let records_dir = root_dir.join("records").join("files");
    let logs_root = root_dir.join("logs");
    let default_config_path = root_dir.join("state").join("config.json");

    fs::create_dir_all(&records_dir)
        .map_err(|e| format!("failed to create '{}': {e}", records_dir.display()))?;
    fs::create_dir_all(&logs_root)
        .map_err(|e| format!("failed to create '{}': {e}", logs_root.display()))?;
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create '{}': {e}", parent.display()))?;
    }
    if let Some(parent) = default_config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create '{}': {e}", parent.display()))?;
    }

    Ok(StoragePaths {
        settings_path,
        manifest_path,
        records_dir,
        logs_root,
        default_config_path,
    })
}

fn migrate_legacy_storage_dir(root_dir: &Path) -> Result<(), String> {
    if root_dir.exists() {
        return Ok(());
    }
    let Some(parent) = root_dir.parent() else {
        return Ok(());
    };
    let legacy_root = parent.join(LEGACY_STORAGE_IDENTIFIER);
    if !legacy_root.is_dir() {
        return Ok(());
    }

    match fs::rename(&legacy_root, root_dir) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            copy_dir_recursive(&legacy_root, root_dir).map_err(|copy_error| {
                format!(
                    "failed to migrate legacy app data '{}' -> '{}': rename error: {rename_error}; copy error: {copy_error}",
                    legacy_root.display(),
                    root_dir.display()
                )
            })?;
            fs::remove_dir_all(&legacy_root).map_err(|e| {
                format!(
                    "failed to remove migrated legacy app data '{}': {e}",
                    legacy_root.display()
                )
            })
        }
    }
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|e| format!("failed to create '{}': {e}", destination.display()))?;
    for entry in
        fs::read_dir(source).map_err(|e| format!("failed to read '{}': {e}", source.display()))?
    {
        let entry =
            entry.map_err(|e| format!("failed to enumerate '{}': {e}", source.display()))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(|e| {
            format!(
                "failed to read file type for '{}': {e}",
                source_path.display()
            )
        })?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path).map_err(|e| {
                format!(
                    "failed to copy '{}' -> '{}': {e}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn load_settings(path: &Path, default_config_path: &Path) -> Result<AppSettings, String> {
    if !path.exists() {
        let settings = AppSettings::default_with_config_path(default_config_path.to_path_buf());
        save_settings(path, &settings)?;
        return Ok(settings);
    }

    let content = fs::read_to_string(path)
        .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
    serde_json::from_str::<AppSettings>(&content)
        .map(|value| value.normalized())
        .map_err(|e| format!("failed to parse '{}': {e}", path.display()))
}

fn save_settings(path: &Path, settings: &AppSettings) -> Result<(), String> {
    let content = serde_json::to_string_pretty(&settings.normalized())
        .map_err(|e| format!("failed to serialize settings: {e}"))?;
    fs::write(path, content.as_bytes())
        .map_err(|e| format!("failed to write '{}': {e}", path.display()))
}

fn load_manifest(path: &Path) -> Result<RecordManifest, String> {
    if !path.exists() {
        let manifest = RecordManifest::default();
        save_manifest(path, &manifest)?;
        return Ok(manifest);
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
    serde_json::from_str::<RecordManifest>(&content)
        .map_err(|e| format!("failed to parse '{}': {e}", path.display()))
}

fn save_manifest(path: &Path, manifest: &RecordManifest) -> Result<(), String> {
    let content = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("failed to serialize record manifest: {e}"))?;
    fs::write(path, content.as_bytes())
        .map_err(|e| format!("failed to write '{}': {e}", path.display()))
}

fn start_new_battle_locked(
    inner: &mut ControllerInner,
    elapsed_frames: i32,
    state: BattleStateCode,
) {
    let created_at_ms = now_millis();
    let battle_id = format!("battle-{}", created_at_ms);
    let title = format_default_battle_title();
    inner.current_battle = Some(ActiveBattle {
        id: battle_id.clone(),
        title,
        stage_name: STAGE_NAME.to_string(),
        created_at_ms,
        elapsed_frames,
        battle_state: state.as_str().to_string(),
        raw_content: String::new(),
        operations: Vec::new(),
        pending_finalize_since: None,
    });
}

fn apply_clock_update_locked(inner: &mut ControllerInner, clock: ClockEvent) -> bool {
    if !clock.battle_state.is_active_battle() {
        if let Some(battle) = inner.current_battle.as_mut() {
            battle.elapsed_frames = battle.elapsed_frames.max(clock.elapsed_frames);
            battle.battle_state = clock.battle_state.as_str().to_string();
            battle
                .pending_finalize_since
                .get_or_insert_with(Instant::now);
            return true;
        }
        return false;
    }

    if let Some(battle) = inner.current_battle.as_mut() {
        if clock.elapsed_frames + FRAMES_PER_SECOND < battle.elapsed_frames {
            if finalize_battle_locked(inner).is_err() {
                inner.recorder_error = Some("failed to finalize previous battle".to_string());
            }
            start_new_battle_locked(inner, clock.elapsed_frames, clock.battle_state);
        } else if let Some(battle) = inner.current_battle.as_mut() {
            battle.elapsed_frames = clock.elapsed_frames;
            battle.battle_state = clock.battle_state.as_str().to_string();
            battle.pending_finalize_since = None;
        }
        true
    } else {
        start_new_battle_locked(inner, clock.elapsed_frames, clock.battle_state);
        true
    }
}

fn sync_live_clock_locked(inner: &mut ControllerInner) {
    let Some(handle) = inner.recorder_handle.as_ref() else {
        return;
    };
    let live = handle.clock_snapshot();
    let clock = ClockEvent {
        elapsed_frames: live.elapsed_frames,
        battle_state: live.battle_state,
    };
    let _ = apply_clock_update_locked(inner, clock);
}

fn finalize_battle_locked(inner: &mut ControllerInner) -> Result<(), String> {
    let Some(battle) = inner.current_battle.take() else {
        return Ok(());
    };
    if battle.operations.is_empty() {
        return Ok(());
    }

    let raw_content = battle.raw_content;
    let last_operation_frames = battle
        .operations
        .last()
        .map(|operation| operation.frames)
        .unwrap_or(0);
    let final_title = format_final_battle_title(&battle.title, last_operation_frames);
    let file_name = unique_record_file_name(&inner.storage.records_dir, &final_title);
    let path = inner.storage.records_dir.join(&file_name);
    fs::write(&path, raw_content.as_bytes())
        .map_err(|e| format!("failed to write '{}': {e}", path.display()))?;

    inner.manifest.records.push(RecordMeta {
        id: battle.id,
        title: final_title,
        stage_name: battle.stage_name,
        file_name,
        created_at_ms: battle.created_at_ms,
        updated_at_ms: now_millis(),
        deleted: false,
        operation_count: battle.operations.len(),
        duration_frames: battle.elapsed_frames,
    });
    save_manifest(&inner.storage.manifest_path, &inner.manifest)
}

fn build_snapshot(inner: &ControllerInner) -> UiSnapshot {
    let mut records = inner
        .manifest
        .records
        .iter()
        .filter(|record| !record.deleted)
        .map(summarize_record)
        .collect::<Vec<_>>();
    records.sort_by(|lhs, rhs| rhs.updated_at_ms.cmp(&lhs.updated_at_ms));
    let capture_state = inner
        .recorder_handle
        .as_ref()
        .map(|handle| handle.clock_snapshot().capture_state.as_str().to_string())
        .unwrap_or_else(|| {
            if inner
                .recorder_error
                .as_deref()
                .is_some_and(is_window_missing_runtime_error)
            {
                CaptureStateCode::WindowMissing.as_str().to_string()
            } else if inner.recorder_error.is_some() {
                CaptureStateCode::Error.as_str().to_string()
            } else {
                CaptureStateCode::Initializing.as_str().to_string()
            }
        });

    UiSnapshot {
        settings: inner.settings.clone(),
        setup: inner.setup.clone(),
        calibration: inner.calibration.clone(),
        records,
        current_battle: inner
            .current_battle
            .as_ref()
            .map(|battle| CurrentBattleSnapshot {
                id: battle.id.clone(),
                title: battle.title.clone(),
                stage_name: battle.stage_name.clone(),
                created_at_ms: battle.created_at_ms,
                elapsed_frames: battle.elapsed_frames,
                battle_state: battle.battle_state.clone(),
                raw_content: battle.raw_content.clone(),
                operations: battle.operations.clone(),
            }),
        capture_state,
        logs_dir: inner.logs_dir.to_string_lossy().to_string(),
        recorder_error: inner.recorder_error.clone(),
    }
}

fn summarize_record(record: &RecordMeta) -> RecordSummary {
    RecordSummary {
        id: record.id.clone(),
        title: record.title.clone(),
        stage_name: record.stage_name.clone(),
        created_at_ms: record.created_at_ms,
        updated_at_ms: record.updated_at_ms,
        deleted: record.deleted,
        operation_count: record.operation_count,
        duration_frames: record.duration_frames,
    }
}

fn user_action_to_ui_line(index: usize, action: &UserAction) -> UiOperationLine {
    let operator_name = action
        .operator_name
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "?".to_string());
    let tile_label = action
        .tile
        .map(|tile| format!("({}, {})", tile.x, tile.y))
        .unwrap_or_else(|| "-".to_string());
    let line_text = match action.tile {
        Some(tile) => format!(
            "{} {} {} ({}, {})",
            format_time_from_frames(action.elapsed_frames),
            action.code.as_record_code(),
            operator_name,
            tile.x,
            tile.y
        ),
        None => format!(
            "{} {} {}",
            format_time_from_frames(action.elapsed_frames),
            action.code.as_record_code(),
            operator_name
        ),
    };

    UiOperationLine {
        index,
        frames: action.elapsed_frames,
        time: format_time_from_frames(action.elapsed_frames),
        code: action.code.as_record_code().to_string(),
        operator_name,
        tile: action.tile,
        tile_label,
        line_text,
    }
}

fn parse_record_content(raw_content: &str) -> Vec<UiOperationLine> {
    let regex = Regex::new(
        r"^\s*(\d{2}):(\d{2}):(\d{2})\s+(DP|US|RT)(?:\s+(.+?))?(?:\s+\(([-]?\d+)\s*,\s*([-]?\d+)\))?\s*$",
    )
    .expect("record line regex must compile");

    raw_content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let captures = regex.captures(line.trim())?;
            let minutes = captures.get(1)?.as_str().parse::<i32>().ok()?;
            let seconds = captures.get(2)?.as_str().parse::<i32>().ok()?;
            let frames = captures.get(3)?.as_str().parse::<i32>().ok()?;
            let total_frames = (minutes * 60 + seconds) * FRAMES_PER_SECOND + frames;
            let operator_name = captures
                .get(5)
                .map(|value| value.as_str().trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "?".to_string());
            let tile = match (captures.get(6), captures.get(7)) {
                (Some(x), Some(y)) => Some(TileCoord {
                    x: x.as_str().parse::<i32>().ok()?,
                    y: y.as_str().parse::<i32>().ok()?,
                }),
                _ => None,
            };
            let tile_label = tile
                .map(|value| format!("({}, {})", value.x, value.y))
                .unwrap_or_else(|| "-".to_string());
            Some(UiOperationLine {
                index,
                frames: total_frames,
                time: format_time_from_frames(total_frames),
                code: captures.get(4)?.as_str().to_string(),
                operator_name,
                tile,
                tile_label,
                line_text: line.trim().to_string(),
            })
        })
        .collect()
}

fn parse_optional_virtual_key(value: &str) -> Result<Option<u32>, String> {
    if value.trim().is_empty() {
        Ok(None)
    } else {
        parse_virtual_key(value).map(Some)
    }
}

fn is_window_missing_runtime_error(error: &str) -> bool {
    error.contains("Invalid target client size: 0x0")
        || error.contains("no frame arrived within timeout")
        || error == "目标窗口当前不可见。请恢复游戏窗口，系统会自动重连。"
}

fn normalize_runtime_error_message(error: &str) -> String {
    if is_window_missing_runtime_error(error) {
        "目标窗口当前不可见。请恢复游戏窗口，系统会自动重连。".to_string()
    } else {
        error.to_string()
    }
}

fn format_default_battle_title() -> String {
    let now = Local::now();
    format!(
        "{}.{} {:02}:{:02}:{:02}",
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn format_final_battle_title(start_title: &str, elapsed_frames: i32) -> String {
    let start_title = start_title.trim();
    let end_time = format_end_time_from_frames(elapsed_frames);
    if start_title.is_empty() {
        end_time
    } else {
        format!("{start_title} {end_time}")
    }
}

fn format_end_time_from_frames(total_frames: i32) -> String {
    let safe_frames = total_frames.max(0);
    let frames = safe_frames % FRAMES_PER_SECOND;
    let total_seconds = safe_frames / FRAMES_PER_SECOND;
    format!("{total_seconds}s{frames:02}f")
}

fn sanitize_file_stem(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => ch,
        })
        .collect()
}

fn unique_record_file_name(records_dir: &Path, battle_title: &str) -> String {
    let stem = sanitize_file_stem(battle_title.trim());
    let base = if stem.is_empty() {
        Local::now().format("%m月%d日_%H时%M分%S秒").to_string()
    } else {
        stem
    };
    let mut candidate = format!("{base}.ops");
    let mut index = 2usize;
    while records_dir.join(&candidate).exists() {
        candidate = format!("{base}_{index}.ops");
        index += 1;
    }
    candidate
}

fn unique_record_id(manifest: &RecordManifest, prefix: &str, seed: u64) -> String {
    let mut candidate = format!("{prefix}-{seed}");
    let mut index = 2usize;
    while manifest.records.iter().any(|record| record.id == candidate) {
        candidate = format!("{prefix}-{seed}-{index}");
        index += 1;
    }
    candidate
}

fn detail_preview_snapshot(
    preview: RecorderDetailPreview,
) -> Result<DetailPreviewSnapshot, String> {
    let bmp = encode_bgr_bitmap_bytes(preview.width, preview.height, &preview.data_bgr)?;
    let data_url = format!(
        "data:image/bmp;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bmp)
    );
    Ok(DetailPreviewSnapshot {
        elapsed_frames: preview.elapsed_frames,
        width: preview.width,
        height: preview.height,
        data_url,
    })
}

fn encode_bgr_bitmap_bytes(width: u32, height: u32, data_bgr: &[u8]) -> Result<Vec<u8>, String> {
    let expected_len = width as usize * height as usize * 3;
    if data_bgr.len() != expected_len {
        return Err(format!(
            "invalid preview size: expected {} bytes, got {}",
            expected_len,
            data_bgr.len()
        ));
    }

    let row_stride = width as usize * 3;
    let padded_row_stride = (row_stride + 3) & !3;
    let pixel_array_size = padded_row_stride * height as usize;
    let file_size = 14usize + 40usize + pixel_array_size;
    let mut bytes = Vec::with_capacity(file_size);

    bytes.extend_from_slice(b"BM");
    bytes.extend_from_slice(&(file_size as u32).to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&(54u32).to_le_bytes());

    bytes.extend_from_slice(&(40u32).to_le_bytes());
    bytes.extend_from_slice(&(width as i32).to_le_bytes());
    bytes.extend_from_slice(&(height as i32).to_le_bytes());
    bytes.extend_from_slice(&(1u16).to_le_bytes());
    bytes.extend_from_slice(&(24u16).to_le_bytes());
    bytes.extend_from_slice(&(0u32).to_le_bytes());
    bytes.extend_from_slice(&(pixel_array_size as u32).to_le_bytes());
    bytes.extend_from_slice(&(0i32).to_le_bytes());
    bytes.extend_from_slice(&(0i32).to_le_bytes());
    bytes.extend_from_slice(&(0u32).to_le_bytes());
    bytes.extend_from_slice(&(0u32).to_le_bytes());

    let padding = [0u8; 3];
    for row in (0..height as usize).rev() {
        let start = row * row_stride;
        let end = start + row_stride;
        bytes.extend_from_slice(&data_bgr[start..end]);
        if padded_row_stride > row_stride {
            bytes.extend_from_slice(&padding[..padded_row_stride - row_stride]);
        }
    }

    Ok(bytes)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[tauri::command]
fn get_snapshot(state: State<'_, Arc<AppController>>) -> UiSnapshot {
    state.maybe_recover_runtime();
    state.snapshot()
}

#[tauri::command]
fn get_bootstrap_snapshot(state: State<'_, Arc<AppController>>) -> AppBootstrap<UiSnapshot> {
    state.maybe_recover_runtime();
    state.bootstrap_snapshot()
}

#[tauri::command]
fn get_detail_preview(
    roi: RoiBounds,
    state: State<'_, Arc<AppController>>,
) -> Option<DetailPreviewSnapshot> {
    state.detail_preview(roi)
}

#[tauri::command]
fn load_record(
    record_id: String,
    state: State<'_, Arc<AppController>>,
) -> Result<RecordDocument, String> {
    state.load_record_document(&record_id)
}

#[tauri::command]
fn save_record(
    record_id: String,
    title: String,
    raw_content: String,
    state: State<'_, Arc<AppController>>,
) -> Result<UiSnapshot, String> {
    state.save_record(&record_id, title, raw_content)?;
    Ok(state.snapshot())
}

#[tauri::command]
fn import_record(
    title: String,
    raw_content: String,
    state: State<'_, Arc<AppController>>,
) -> Result<ImportRecordResponse, String> {
    let document = state.import_record(title, raw_content)?;
    Ok(ImportRecordResponse {
        snapshot: state.snapshot(),
        document,
    })
}

#[tauri::command]
fn save_current_battle_text(
    battle_id: String,
    raw_content: String,
    base_snapshot_text: String,
    state: State<'_, Arc<AppController>>,
) -> Result<UiSnapshot, String> {
    state.save_current_battle_text(&battle_id, raw_content, base_snapshot_text)?;
    Ok(state.snapshot())
}

#[tauri::command]
fn set_record_deleted(
    record_id: String,
    deleted: bool,
    state: State<'_, Arc<AppController>>,
) -> Result<UiSnapshot, String> {
    state.set_record_deleted(&record_id, deleted)?;
    Ok(state.snapshot())
}

#[tauri::command]
fn save_app_settings(
    settings: AppSettings,
    window: WebviewWindow,
    state: State<'_, Arc<AppController>>,
) -> Result<UiSnapshot, String> {
    state.save_settings_and_reconcile(&window, settings)?;
    Ok(state.snapshot())
}

#[tauri::command]
fn set_window_pinned(
    pinned: bool,
    window: WebviewWindow,
    state: State<'_, Arc<AppController>>,
) -> Result<UiSnapshot, String> {
    state.set_always_on_top(&window, pinned)?;
    Ok(state.snapshot())
}

#[tauri::command]
fn refresh_setup(state: State<'_, Arc<AppController>>) -> Result<UiSnapshot, String> {
    state.refresh_setup()?;
    Ok(state.snapshot())
}

#[tauri::command]
fn save_selected_target(
    fingerprint: String,
    state: State<'_, Arc<AppController>>,
) -> Result<UiSnapshot, String> {
    state.save_selected_target(&fingerprint)?;
    Ok(state.snapshot())
}

#[tauri::command]
fn start_calibration(state: State<'_, Arc<AppController>>) -> Result<UiSnapshot, String> {
    state.start_calibration()?;
    Ok(state.snapshot())
}

#[tauri::command]
fn cancel_calibration(state: State<'_, Arc<AppController>>) -> Result<UiSnapshot, String> {
    state.cancel_calibration()?;
    Ok(state.snapshot())
}

fn main() {
    enable_dpi_awareness();
    match relaunch_as_admin_if_needed() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    }

    tauri::Builder::default()
        .setup(|app| {
            let controller = AppController::bootstrap(app.handle().clone())?;
            app.manage(Arc::clone(&controller));
            if let Some(window) = app.get_webview_window("main") {
                controller.apply_window_settings(&window)?;
            }
            controller.request_reconcile_runtime();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_bootstrap_snapshot,
            get_detail_preview,
            load_record,
            save_record,
            import_record,
            save_current_battle_text,
            set_record_deleted,
            save_app_settings,
            set_window_pinned,
            refresh_setup,
            save_selected_target,
            start_calibration,
            cancel_calibration
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                let state = window.app_handle().state::<Arc<AppController>>();
                state.request_shutdown();
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to run Tauri application");
}
