use crate::runtime::{FrameTimeline, InputStamp, ObservationStamp};
use std::any::Any;
use std::ffi::{c_void, OsStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{
    fs::{self, File},
    io::{LineWriter, Write},
};

use arknights_super_ruler_bridge::legacy as ruler_compat;
use arknights_super_ruler_bridge::legacy::analysis::{
    calibration::infer_calibration_from_samples_with_ui_scaler_and_total_bar_width,
    roi::{cost_bar_width_frac_with_ui_scaler, find_cost_bar_roi_with_ui_scaler},
    scanner,
};
use arknights_super_ruler_bridge::legacy::capture::{
    create_backend, CaptureBackend, CapturedFrame, WindowInfo,
};
use arknights_super_ruler_bridge::legacy::{Analyzer, BattleState, PixelFormat, RulerConfig};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use ort::{session::Session, value::TensorRef};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, REG_BINARY, REG_SZ, REG_VALUE_TYPE, RRF_RT_ANY,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, EnumChildWindows, EnumWindows, GetAncestor, GetClassNameW,
    GetClientRect, GetForegroundWindow, GetMessageW, GetWindowTextLengthW, GetWindowTextW,
    IsWindow, IsWindowVisible, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WindowFromPoint, GA_ROOT, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG,
    MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

pub const FRAMES_PER_SECOND: i32 = 30;
pub const CALIBRATION_CANCELLED: &str = "calibration_cancelled";
const DRAG_THRESHOLD_PX: i32 = 8;
const BATTLEFIELD_TILE_SNAP_MAX_DISTANCE_PX: f64 = 72.0;
const OPER_NAME_PREPROCESS_THRESHOLD: u8 = 160;
const OPER_NAME_OCR_INPUT_HEIGHT: usize = 48;
const OPER_NAME_OCR_BASE_INPUT_WIDTH: usize = 320;
const OPER_NAME_OCR_MAX_INPUT_WIDTH: usize = 1024;
const CURSOR_SHOWING: u32 = 0x0000_0001;
const PC_CURSOR_REF_WIDTH: f64 = 1280.0;
const PC_CURSOR_REF_HEIGHT: f64 = 720.0;
const PC_CURSOR_REF_ASPECT_RATIO: f64 = PC_CURSOR_REF_WIDTH / PC_CURSOR_REF_HEIGHT;
const PC_CURSOR_WIDTH_REF: f64 = 72.0;
const PC_CURSOR_HEIGHT_REF: f64 = 81.0;
const PC_CURSOR_HOTSPOT_X_REF: f64 = 8.0;
const PC_CURSOR_HOTSPOT_Y_REF: f64 = 8.0;
const PC_CURSOR_MIN_SIZE_FACTOR: f64 = 0.6;
const COST_SIGN_LEFT_OFFSET_FROM_RIGHT_REF: f64 = 70.0;
const COST_SIGN_RIGHT_OFFSET_FROM_RIGHT_REF: f64 = 42.0;
const COST_SIGN_TOP_OFFSET_FROM_BOTTOM_REF: f64 = 208.0;
const COST_SIGN_BOTTOM_OFFSET_FROM_BOTTOM_REF: f64 = 201.0;
const OPER_NAME_LINE_CROP_RELATIVE_RECT: RelativeRectF32 = RelativeRectF32 {
    x: 0.0,
    y: 0.30,
    width: 0.80,
    height: 0.54,
};
const MAA_TASKS_JSON_PATH: &str = "ocr/tasks/tasks.json";
const MAA_PADDLE_OCR_REC_MODEL_PATH: &str = "ocr/rec/inference.onnx";
const MAA_PADDLE_OCR_REC_KEYS_PATH: &str = "ocr/rec/keys.txt";
const RESOURCE_ROOT_ENV: &str = "ARKNIGHTS_SUPER_RESOURCE_ROOT";
pub const DEFAULT_OPER_NAME_ROI_RELATIVE_RECT: RelativeRectF32 = RelativeRectF32 {
    // Based on the in-battle selected-operator detail panel shown on the left:
    // the operator name sits around the upper-left quarter of that panel.
    x: 0.0,
    y: 260.0 / 1068.0,
    width: 300.0 / 1907.0,
    height: 80.0 / 1068.0,
};
const MUMU_CHILD_WINDOW_TITLE: &str = "MuMuPlayer";

static HOOK_RUNTIME: OnceLock<Mutex<Option<HookRuntime>>> = OnceLock::new();

fn hook_runtime_slot() -> &'static Mutex<Option<HookRuntime>> {
    HOOK_RUNTIME.get_or_init(|| Mutex::new(None))
}

#[derive(Clone, Debug)]
pub struct RecorderOptions {
    pub config_path: PathBuf,
    pub skill_vk: Option<u32>,
    pub retreat_vk: Option<u32>,
    pub detail_name_roi: RelativeRectF32,
    pub visible_console: bool,
    pub logs_root: PathBuf,
}

impl Default for RecorderOptions {
    fn default() -> Self {
        Self {
            config_path: PathBuf::from("config.json"),
            skill_vk: None,
            retreat_vk: None,
            detail_name_roi: DEFAULT_OPER_NAME_ROI_RELATIVE_RECT,
            visible_console: false,
            logs_root: PathBuf::from("logs"),
        }
    }
}

impl RecorderOptions {
    pub fn for_cli() -> Self {
        Self {
            visible_console: true,
            ..Self::default()
        }
    }

    fn parse() -> Result<Self, String> {
        let mut options = Self::for_cli();

        let args: Vec<String> = std::env::args().collect();
        let mut index = 1;
        while index < args.len() {
            match args[index].as_str() {
                "--config" | "-c" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "missing value after --config".to_string())?;
                    options.config_path = PathBuf::from(value);
                }
                "--skill-key" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "missing value after --skill-key".to_string())?;
                    options.skill_vk = Some(parse_virtual_key(value)?);
                }
                "--retreat-key" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| "missing value after --retreat-key".to_string())?;
                    options.retreat_vk = Some(parse_virtual_key(value)?);
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown option: {other}")),
            }
            index += 1;
        }

        Ok(options)
    }
}

#[derive(Clone, Debug)]
pub struct CalibrationOptions {
    pub config_path: PathBuf,
    pub logs_root: PathBuf,
}

impl Default for CalibrationOptions {
    fn default() -> Self {
        Self {
            config_path: PathBuf::from("config.json"),
            logs_root: PathBuf::from("logs"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalibrationStage {
    Starting,
    Sampling,
    Inferring,
    Saving,
}

#[derive(Clone, Debug)]
pub struct CalibrationUpdate {
    pub stage: CalibrationStage,
    pub progress_percent: f32,
    pub message: String,
    pub session_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct CalibrationOutcome {
    pub session_dir: PathBuf,
    pub profile_path: PathBuf,
    pub profile_filename: String,
}

fn print_help() {
    println!("Usage: arknights-super [OPTIONS]");
    println!("  -c, --config PATH      CostBarRuler config.json path (default: config.json)");
    println!("      --skill-key KEY    Skill hotkey, e.g. Q / F / F1");
    println!("      --retreat-key KEY  Retreat hotkey, e.g. E / R / F2");
    println!("  -h, --help             Show this help");
}

pub fn parse_virtual_key(value: &str) -> Result<u32, String> {
    let upper = value.trim().to_ascii_uppercase();
    if upper.len() == 1 {
        let ch = upper.as_bytes()[0];
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            return Ok(ch as u32);
        }
    }

    match upper.as_str() {
        "SPACE" => Ok(0x20),
        "TAB" => Ok(0x09),
        "ESC" | "ESCAPE" => Ok(0x1B),
        "ENTER" => Ok(0x0D),
        "SHIFT" => Ok(0x10),
        "CTRL" | "CONTROL" => Ok(0x11),
        "ALT" => Ok(0x12),
        _ if upper.starts_with('F') => {
            let suffix = &upper[1..];
            let number = suffix
                .parse::<u32>()
                .map_err(|_| format!("unsupported key: {value}"))?;
            if (1..=24).contains(&number) {
                Ok(0x70 + (number - 1))
            } else {
                Err(format!("unsupported function key: {value}"))
            }
        }
        _ => Err(format!("unsupported key: {value}")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BattleStateCode {
    Unknown,
    PointTwoXRunning,
    OneXRunning,
    TwoXRunning,
    PointTwoXPaused,
    OneXPaused,
    TwoXPaused,
    DeployingOperator,
    AdjustingOperatorFacing,
    BattleBegin,
    BeforeOrAfterBattle,
    NotInBattle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CaptureStateCode {
    Initializing,
    Active,
    WindowMissing,
    Error,
}

impl CaptureStateCode {
    fn as_u8(self) -> u8 {
        match self {
            Self::Initializing => 0,
            Self::Active => 1,
            Self::WindowMissing => 2,
            Self::Error => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Active,
            2 => Self::WindowMissing,
            3 => Self::Error,
            _ => Self::Initializing,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initializing => "initializing",
            Self::Active => "active",
            Self::WindowMissing => "window_missing",
            Self::Error => "error",
        }
    }
}

impl BattleStateCode {
    fn from_ruler(value: BattleState) -> Self {
        match value {
            BattleState::PointTwoXRunning => Self::PointTwoXRunning,
            BattleState::OneXRunning => Self::OneXRunning,
            BattleState::TwoXRunning => Self::TwoXRunning,
            BattleState::PointTwoXPaused => Self::PointTwoXPaused,
            BattleState::OneXPaused => Self::OneXPaused,
            BattleState::TwoXPaused => Self::TwoXPaused,
            BattleState::DeployingOperator => Self::DeployingOperator,
            BattleState::AdjustingOperatorFacing => Self::AdjustingOperatorFacing,
            BattleState::BattleBegin => Self::BattleBegin,
            BattleState::BeforeOrAfterBattle => Self::BeforeOrAfterBattle,
            BattleState::NotInBattle => Self::NotInBattle,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::PointTwoXRunning,
            2 => Self::OneXRunning,
            3 => Self::TwoXRunning,
            4 => Self::PointTwoXPaused,
            5 => Self::OneXPaused,
            6 => Self::TwoXPaused,
            7 => Self::DeployingOperator,
            8 => Self::AdjustingOperatorFacing,
            9 => Self::BattleBegin,
            10 => Self::BeforeOrAfterBattle,
            11 => Self::NotInBattle,
            _ => Self::Unknown,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::PointTwoXRunning => 1,
            Self::OneXRunning => 2,
            Self::TwoXRunning => 3,
            Self::PointTwoXPaused => 4,
            Self::OneXPaused => 5,
            Self::TwoXPaused => 6,
            Self::DeployingOperator => 7,
            Self::AdjustingOperatorFacing => 8,
            Self::BattleBegin => 9,
            Self::BeforeOrAfterBattle => 10,
            Self::NotInBattle => 11,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::PointTwoXRunning => "0.2x_running",
            Self::OneXRunning => "1x_running",
            Self::TwoXRunning => "2x_running",
            Self::PointTwoXPaused => "0.2x_paused",
            Self::OneXPaused => "1x_paused",
            Self::TwoXPaused => "2x_paused",
            Self::DeployingOperator => "deploying_operator",
            Self::AdjustingOperatorFacing => "adjusting_operator_facing",
            Self::BattleBegin => "battle_begin",
            Self::BeforeOrAfterBattle => "before_or_after_battle",
            Self::NotInBattle => "not_in_battle",
        }
    }

    fn is_deploy_related(self) -> bool {
        matches!(
            self,
            Self::DeployingOperator | Self::AdjustingOperatorFacing
        )
    }

    fn allows_battlefield_selection(self) -> bool {
        matches!(
            self,
            Self::PointTwoXRunning
                | Self::OneXRunning
                | Self::TwoXRunning
                | Self::PointTwoXPaused
                | Self::OneXPaused
                | Self::TwoXPaused
        )
    }

    pub fn is_active_battle(self) -> bool {
        !matches!(
            self,
            Self::Unknown | Self::BeforeOrAfterBattle | Self::NotInBattle
        )
    }
}

#[derive(Debug)]
struct SharedClockState {
    elapsed_frames: AtomicI32,
    logical_frame: AtomicI32,
    total_frames_in_cycle: AtomicI32,
    battle_state: AtomicU8,
    capture_state: AtomicU8,
    target_hwnd: AtomicIsize,
    client_left: AtomicI32,
    client_top: AtomicI32,
    width: AtomicU32,
    height: AtomicU32,
    latest_detail_name_crop: Mutex<Option<DetailNameCrop>>,
    latest_frame: Mutex<Option<LatestCapturedFrame>>,
    detail_name_roi: RelativeRectF32,
    timeline: Mutex<FrameTimeline>,
    timeline_sequence: AtomicU64,
    session_started: Instant,
}

impl SharedClockState {
    fn new(detail_name_roi: RelativeRectF32) -> Self {
        Self {
            elapsed_frames: AtomicI32::new(0),
            logical_frame: AtomicI32::new(-1),
            total_frames_in_cycle: AtomicI32::new(0),
            battle_state: AtomicU8::new(BattleStateCode::Unknown.as_u8()),
            capture_state: AtomicU8::new(CaptureStateCode::Initializing.as_u8()),
            target_hwnd: AtomicIsize::new(0),
            client_left: AtomicI32::new(0),
            client_top: AtomicI32::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            latest_detail_name_crop: Mutex::new(None),
            latest_frame: Mutex::new(None),
            detail_name_roi,
            // Six seconds covers a blocked worker or a paused game without
            // creating unbounded memory pressure in the capture hot path.
            timeline: Mutex::new(FrameTimeline::new(180)),
            timeline_sequence: AtomicU64::new(0),
            session_started: Instant::now(),
        }
    }

    fn update_timing(
        &self,
        elapsed_frames: i32,
        logical_frame: Option<i32>,
        total_frames: i32,
        battle_state: BattleStateCode,
    ) {
        self.elapsed_frames.store(elapsed_frames, Ordering::Relaxed);
        self.logical_frame
            .store(logical_frame.unwrap_or(-1), Ordering::Relaxed);
        self.total_frames_in_cycle
            .store(total_frames, Ordering::Relaxed);
        self.battle_state
            .store(battle_state.as_u8(), Ordering::Relaxed);
        let observed_at_ns =
            u64::try_from(self.session_started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let sequence = self.timeline_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.timeline
            .lock()
            .expect("frame timeline poisoned")
            .push(ObservationStamp {
                sequence,
                observed_at_ns,
                elapsed_frames,
                logical_frame,
            });
    }

    fn update_window(&self, info: WindowInfo) {
        self.target_hwnd.store(info.hwnd, Ordering::Relaxed);
        self.client_left.store(info.client_left, Ordering::Relaxed);
        self.client_top.store(info.client_top, Ordering::Relaxed);
        self.width.store(info.width, Ordering::Relaxed);
        self.height.store(info.height, Ordering::Relaxed);
    }

    fn update_capture_state(&self, state: CaptureStateCode) {
        self.capture_state.store(state.as_u8(), Ordering::Relaxed);
    }

    fn update_detail_name_crop(&self, crop: Option<DetailNameCrop>) {
        let mut guard = self
            .latest_detail_name_crop
            .lock()
            .expect("latest_detail_name_crop poisoned");
        *guard = crop;
    }

    fn update_latest_frame(&self, frame: Option<LatestCapturedFrame>) {
        let mut guard = self.latest_frame.lock().expect("latest_frame poisoned");
        *guard = frame;
    }

    fn detail_name_crop(&self) -> Option<DetailNameCrop> {
        self.latest_detail_name_crop
            .lock()
            .expect("latest_detail_name_crop poisoned")
            .clone()
    }

    fn preview_crop(&self, roi: RelativeRectF32) -> Option<DetailNameCrop> {
        let frame = self
            .latest_frame
            .lock()
            .expect("latest_frame poisoned")
            .clone()?;
        extract_detail_name_crop_from_parts(
            &frame.data,
            frame.width,
            frame.height,
            frame.format,
            frame.elapsed_frames,
            roi,
        )
    }

    fn snapshot(&self) -> ClockSnapshot {
        ClockSnapshot {
            elapsed_frames: self.elapsed_frames.load(Ordering::Relaxed),
            logical_frame: match self.logical_frame.load(Ordering::Relaxed) {
                value if value >= 0 => Some(value),
                _ => None,
            },
            total_frames_in_cycle: self.total_frames_in_cycle.load(Ordering::Relaxed),
            battle_state: BattleStateCode::from_u8(self.battle_state.load(Ordering::Relaxed)),
            capture_state: CaptureStateCode::from_u8(self.capture_state.load(Ordering::Relaxed)),
            target_hwnd: self.target_hwnd.load(Ordering::Relaxed),
            client_left: self.client_left.load(Ordering::Relaxed),
            client_top: self.client_top.load(Ordering::Relaxed),
            width: self.width.load(Ordering::Relaxed),
            height: self.height.load(Ordering::Relaxed),
        }
    }

    fn snapshot_for_input(&self) -> ClockSnapshot {
        let input_at_ns =
            u64::try_from(self.session_started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let mut snapshot = self.snapshot();
        if let Some(correlation) = self
            .timeline
            .lock()
            .expect("frame timeline poisoned")
            .correlate(InputStamp {
                observed_at_ns: input_at_ns,
            })
        {
            snapshot.elapsed_frames = correlation.observation.elapsed_frames;
            snapshot.logical_frame = correlation.observation.logical_frame;
        }
        snapshot
    }
}

#[derive(Clone, Copy, Debug)]
struct ClockSnapshot {
    elapsed_frames: i32,
    logical_frame: Option<i32>,
    total_frames_in_cycle: i32,
    battle_state: BattleStateCode,
    capture_state: CaptureStateCode,
    target_hwnd: isize,
    client_left: i32,
    client_top: i32,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PointI32 {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug)]
struct RectI32 {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[repr(C)]
struct NativeCursorInfo {
    cb_size: u32,
    flags: u32,
    h_cursor: *mut c_void,
    pt_screen_pos: POINT,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CursorGuardRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl CursorGuardRect {
    fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }

    fn clamped(self, max_x: i32, max_y: i32) -> Self {
        Self {
            left: self.left.clamp(0, max_x),
            top: self.top.clamp(0, max_y),
            right: self.right.clamp(0, max_x),
            bottom: self.bottom.clamp(0, max_y),
        }
    }
}

#[derive(Debug)]
struct SelfDrawnCursorGuard {
    ui_scaler: f64,
    cursor_size: f64,
    ever_saw_hidden: bool,
    self_drawn_enabled: bool,
}

impl SelfDrawnCursorGuard {
    fn new(ui_scaler: f64, cursor_size: f64) -> Self {
        let ui_scaler =
            finite_or_default(ui_scaler, ruler_compat::analysis::roi::DEFAULT_UI_SCALER)
                .clamp(0.0, 1.0);
        let cursor_size = finite_or_default(cursor_size, 1.0).clamp(0.0, 1.0);
        Self {
            ui_scaler,
            cursor_size,
            ever_saw_hidden: false,
            self_drawn_enabled: false,
        }
    }

    fn should_pause_for_frame(
        &mut self,
        window: Option<WindowInfo>,
        roi: (i32, i32, i32),
        battle_state: BattleState,
    ) -> bool {
        if matches!(
            battle_state,
            BattleState::NotInBattle | BattleState::AdjustingOperatorFacing
        ) {
            return false;
        }

        let Some(window) = window else {
            return false;
        };
        let Some(cursor) = cursor_state_in_client(window) else {
            return false;
        };

        if cursor.uncovered {
            self.observe_native_cursor(cursor.native_visible);
        }

        if !self.self_drawn_enabled {
            return false;
        }

        let cursor_rect = self.cursor_rect(
            cursor.client_x,
            cursor.client_y,
            window.width,
            window.height,
        );
        let cost_bar_rect = cost_bar_rect(roi, window.width, window.height, self.ui_scaler);
        let cost_area_rect = cost_area_rect(window.width, window.height, self.ui_scaler);
        cursor_rect.intersects(cost_bar_rect) || cursor_rect.intersects(cost_area_rect)
    }

    fn observe_native_cursor(&mut self, native_visible: bool) {
        if self.ever_saw_hidden {
            return;
        }

        if native_visible {
            self.self_drawn_enabled = false;
        } else {
            self.ever_saw_hidden = true;
            self.self_drawn_enabled = true;
            log::info!(
                "Arknights PC self-drawn cursor detected; cursor guard stays enabled for this run"
            );
        }
    }

    fn cursor_rect(
        &self,
        client_x: i32,
        client_y: i32,
        width: u32,
        height: u32,
    ) -> CursorGuardRect {
        let scale = cursor_layout_scale(width, height)
            * ruler_compat::analysis::roi::ui_edge_scale(self.ui_scaler)
            * cursor_size_factor(self.cursor_size);
        let left = client_x - (PC_CURSOR_HOTSPOT_X_REF * scale).round() as i32;
        let top = client_y - (PC_CURSOR_HOTSPOT_Y_REF * scale).round() as i32;
        let cursor_w = (PC_CURSOR_WIDTH_REF * scale).round().max(1.0) as i32;
        let cursor_h = (PC_CURSOR_HEIGHT_REF * scale).round().max(1.0) as i32;

        CursorGuardRect {
            left,
            top,
            right: left + cursor_w,
            bottom: top + cursor_h,
        }
    }
}

struct CursorState {
    client_x: i32,
    client_y: i32,
    native_visible: bool,
    uncovered: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RelativeRectF32 {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug)]
struct StageTileSpec {
    x: i32,
    y: i32,
    buildable_type: i32,
    height_type: i32,
    tile_key: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct StageMap {
    width: i32,
    height: i32,
    normal_view: [f64; 3],
    side_view: [f64; 3],
    tiles: &'static [StageTileSpec],
}

#[derive(Clone, Copy, Debug)]
struct TileMatch {
    x: i32,
    y: i32,
    buildable_type: i32,
    tile_key: &'static str,
    screen: PointI32,
    distance_px: f64,
}

#[derive(Clone, Debug)]
struct DetailNameCrop {
    elapsed_frames: i32,
    width: u32,
    height: u32,
    data_bgr: Vec<u8>,
}

#[derive(Clone, Debug)]
struct LatestCapturedFrame {
    elapsed_frames: i32,
    width: u32,
    height: u32,
    format: PixelFormat,
    data: Vec<u8>,
}

#[derive(Clone, Debug)]
struct DetailCapture {
    relative_path: String,
    captured_elapsed_frames: i32,
    width: u32,
    height: u32,
    crop: DetailNameCrop,
}

struct OperatorNameRecognizer {
    engine: Option<Mutex<OnnxOcrEngine>>,
    replace_rules: Vec<ReplaceRule>,
}

struct OnnxOcrEngine {
    session: Session,
    dictionary: Vec<String>,
    input_height: usize,
    base_input_width: usize,
    max_input_width: usize,
}

struct ReplaceRule {
    pattern: Regex,
    replacement: String,
}

#[derive(Clone, Copy, Debug)]
struct PendingDrag {
    client: PointI32,
    frame_at_down: i32,
    state_at_down: BattleStateCode,
}

#[derive(Clone, Debug)]
struct PendingDeploy {
    tile: TileMatch,
    end_client: PointI32,
    drop_elapsed_frames: i32,
    drop_logical_frame: Option<i32>,
    drop_total_frames_in_cycle: i32,
    drop_battle_state: BattleStateCode,
    detail_capture: Option<DetailCapture>,
    saw_adjusting: bool,
}

const DEPLOY_FACING_START_SNAP_PX: i32 = 64;
const DEPLOY_FACING_CONFIRM_WINDOW_FRAMES: i32 = FRAMES_PER_SECOND * 2;

struct HookRuntime {
    tx: Sender<AppEvent>,
    clock: Arc<SharedClockState>,
    skill_vk: Option<u32>,
    retreat_vk: Option<u32>,
    pending_drag: Mutex<Option<PendingDrag>>,
    selected_tile: Mutex<Option<TileMatch>>,
    pending_deploy: Mutex<Option<PendingDeploy>>,
    capture_dir: PathBuf,
    capture_sequence: AtomicU32,
}

enum ExternalWindowTracker {
    MuMu(MuMuWindowTracker),
}

impl ExternalWindowTracker {
    fn from_config(config: &RulerConfig) -> Option<Self> {
        match config.capture_type.as_str() {
            "mumu" => Some(Self::MuMu(MuMuWindowTracker::new())),
            _ => None,
        }
    }

    fn current_window(&self) -> Option<WindowInfo> {
        match self {
            Self::MuMu(tracker) => tracker.current_window(),
        }
    }
}

#[derive(Clone, Copy)]
struct MuMuWindowMatch {
    client_hwnd: isize,
}

struct MuMuWindowTracker {
    cached: Mutex<Option<MuMuWindowMatch>>,
}

impl MuMuWindowTracker {
    fn new() -> Self {
        Self {
            cached: Mutex::new(None),
        }
    }

    fn current_window(&self) -> Option<WindowInfo> {
        // MuMu may expose multiple visible top-level windows, and the one the user
        // is actually interacting with can change after startup. If we keep the
        // first match forever, timing still works through native capture but all
        // input events get filtered out by `foreground_match=no`.
        if let Some(matched) = discover_mumu_window_match() {
            if let Some(info) = window_info_for_client_hwnd(matched.client_hwnd) {
                *self.cached.lock().expect("mumu window cache poisoned") = Some(matched);
                return Some(info);
            }
        }

        if let Some(matched) = *self.cached.lock().expect("mumu window cache poisoned") {
            if let Some(info) = window_info_for_client_hwnd(matched.client_hwnd) {
                return Some(info);
            }
        }
        None
    }
}

#[derive(Debug)]
pub enum AppEvent {
    Clock(ClockEvent),
    Operation(OperationEvent),
    Info(String),
    RawInput(RawInputEvent),
    Warning(String),
}

#[derive(Debug)]
pub struct ClockEvent {
    pub elapsed_frames: i32,
    pub battle_state: BattleStateCode,
}

#[derive(Debug)]
pub struct RawInputEvent {
    elapsed_frames: i32,
    logical_frame: Option<i32>,
    total_frames_in_cycle: i32,
    battle_state: BattleStateCode,
    kind: RawInputKind,
}

#[derive(Clone, Copy, Debug)]
enum RawInputKind {
    Key {
        action: KeyAction,
        vk: u32,
        scan_code: u32,
        extended: bool,
        injected: bool,
        foreground_match: bool,
        skill_match: bool,
        retreat_match: bool,
        target_hwnd: isize,
        target_root_hwnd: isize,
        foreground_hwnd: isize,
        foreground_root_hwnd: isize,
    },
    MouseLeft {
        action: MouseAction,
        screen: PointI32,
        client: Option<PointI32>,
        point_root_hwnd: isize,
        foreground_match: bool,
        target_hwnd: isize,
        foreground_hwnd: isize,
        foreground_root_hwnd: isize,
    },
}

#[derive(Clone, Copy, Debug)]
enum KeyAction {
    Down,
    Up,
}

impl KeyAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum MouseAction {
    Down,
    Up,
}

impl MouseAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
        }
    }
}

#[derive(Debug)]
pub struct OperationEvent {
    pub elapsed_frames: i32,
    pub logical_frame: Option<i32>,
    pub total_frames_in_cycle: i32,
    pub battle_state: BattleStateCode,
    kind: OperationKind,
}

#[derive(Debug)]
enum OperationKind {
    LeftClick {
        client: PointI32,
        selected_tile: Option<TileMatch>,
    },
    Drag {
        start_client: PointI32,
        end_client: PointI32,
        start_frame: i32,
        start_state: BattleStateCode,
        deploy_like: bool,
        deploy_end_tile: Option<TileMatch>,
    },
    DeployResolved {
        tile: TileMatch,
        end_client: PointI32,
        detail_capture: Option<DetailCapture>,
        detail_name: Option<String>,
    },
    SkillHotkey {
        vk: u32,
        selected_tile: Option<TileMatch>,
        detail_capture: Option<DetailCapture>,
        detail_name: Option<String>,
    },
    RetreatHotkey {
        vk: u32,
        selected_tile: Option<TileMatch>,
        detail_capture: Option<DetailCapture>,
        detail_name: Option<String>,
    },
}

impl OperationEvent {
    fn from_timing(
        kind: OperationKind,
        elapsed_frames: i32,
        logical_frame: Option<i32>,
        total_frames_in_cycle: i32,
        battle_state: BattleStateCode,
    ) -> Self {
        Self {
            elapsed_frames,
            logical_frame,
            total_frames_in_cycle,
            battle_state,
            kind,
        }
    }

    fn from_clock(kind: OperationKind, snapshot: ClockSnapshot) -> Self {
        Self::from_timing(
            kind,
            snapshot.elapsed_frames,
            snapshot.logical_frame,
            snapshot.total_frames_in_cycle,
            snapshot.battle_state,
        )
    }

    pub fn to_user_action(&self) -> Option<UserAction> {
        match &self.kind {
            OperationKind::DeployResolved {
                tile, detail_name, ..
            } => Some(UserAction {
                elapsed_frames: self.elapsed_frames,
                code: UserActionCode::Deploy,
                operator_name: detail_name.clone(),
                tile: Some(TileCoord {
                    x: tile.x,
                    y: tile.y,
                }),
            }),
            OperationKind::SkillHotkey {
                selected_tile,
                detail_name,
                ..
            } => Some(UserAction {
                elapsed_frames: self.elapsed_frames,
                code: UserActionCode::UseSkill,
                operator_name: detail_name.clone(),
                tile: selected_tile.map(|tile| TileCoord {
                    x: tile.x,
                    y: tile.y,
                }),
            }),
            OperationKind::RetreatHotkey {
                selected_tile,
                detail_name,
                ..
            } => Some(UserAction {
                elapsed_frames: self.elapsed_frames,
                code: UserActionCode::Retreat,
                operator_name: detail_name.clone(),
                tile: selected_tile.map(|tile| TileCoord {
                    x: tile.x,
                    y: tile.y,
                }),
            }),
            _ => None,
        }
    }

    fn needs_operator_name_resolution(&self) -> bool {
        match &self.kind {
            OperationKind::DeployResolved {
                detail_capture,
                detail_name,
                ..
            }
            | OperationKind::SkillHotkey {
                detail_capture,
                detail_name,
                ..
            }
            | OperationKind::RetreatHotkey {
                detail_capture,
                detail_name,
                ..
            } => detail_name.is_none() && detail_capture.is_some(),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub enum UserActionCode {
    Deploy,
    UseSkill,
    Retreat,
}

impl UserActionCode {
    pub fn as_record_code(self) -> &'static str {
        match self {
            Self::Deploy => "DP",
            Self::UseSkill => "US",
            Self::Retreat => "RT",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct TileCoord {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserAction {
    pub elapsed_frames: i32,
    pub code: UserActionCode,
    pub operator_name: Option<String>,
    pub tile: Option<TileCoord>,
}

const STAGE_MORAN_SHUGE_TILES: [StageTileSpec; 96] = [
    StageTileSpec {
        x: 0,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 1,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 2,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 3,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 4,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 5,
        y: 0,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_end",
    },
    StageTileSpec {
        x: 6,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 7,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 8,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 9,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 10,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 11,
        y: 0,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 0,
        y: 1,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 1,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 2,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 3,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 4,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 5,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 6,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 7,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 8,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 9,
        y: 1,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 10,
        y: 1,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_floor",
    },
    StageTileSpec {
        x: 11,
        y: 1,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_start",
    },
    StageTileSpec {
        x: 0,
        y: 2,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 1,
        y: 2,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_telout",
    },
    StageTileSpec {
        x: 2,
        y: 2,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 3,
        y: 2,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 4,
        y: 2,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 5,
        y: 2,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 6,
        y: 2,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 7,
        y: 2,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_fence_bound",
    },
    StageTileSpec {
        x: 8,
        y: 2,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 9,
        y: 2,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 10,
        y: 2,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 11,
        y: 2,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 0,
        y: 3,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 1,
        y: 3,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_fence_bound",
    },
    StageTileSpec {
        x: 2,
        y: 3,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 3,
        y: 3,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 4,
        y: 3,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 5,
        y: 3,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 6,
        y: 3,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 7,
        y: 3,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 8,
        y: 3,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 9,
        y: 3,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 10,
        y: 3,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 11,
        y: 3,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_start",
    },
    StageTileSpec {
        x: 0,
        y: 4,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 1,
        y: 4,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 2,
        y: 4,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_hole",
    },
    StageTileSpec {
        x: 3,
        y: 4,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 4,
        y: 4,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 5,
        y: 4,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 6,
        y: 4,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 7,
        y: 4,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_floor",
    },
    StageTileSpec {
        x: 8,
        y: 4,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 9,
        y: 4,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 10,
        y: 4,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 11,
        y: 4,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 0,
        y: 5,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 1,
        y: 5,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 2,
        y: 5,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_hole",
    },
    StageTileSpec {
        x: 3,
        y: 5,
        buildable_type: 2,
        height_type: 1,
        tile_key: "tile_wall",
    },
    StageTileSpec {
        x: 4,
        y: 5,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 5,
        y: 5,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 6,
        y: 5,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 7,
        y: 5,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_floor",
    },
    StageTileSpec {
        x: 8,
        y: 5,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 9,
        y: 5,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_flystart",
    },
    StageTileSpec {
        x: 10,
        y: 5,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 11,
        y: 5,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_telin",
    },
    StageTileSpec {
        x: 0,
        y: 6,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 1,
        y: 6,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 2,
        y: 6,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_hole",
    },
    StageTileSpec {
        x: 3,
        y: 6,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 4,
        y: 6,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_end",
    },
    StageTileSpec {
        x: 5,
        y: 6,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 6,
        y: 6,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_start",
    },
    StageTileSpec {
        x: 7,
        y: 6,
        buildable_type: 0,
        height_type: 0,
        tile_key: "tile_floor",
    },
    StageTileSpec {
        x: 8,
        y: 6,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 9,
        y: 6,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_road",
    },
    StageTileSpec {
        x: 10,
        y: 6,
        buildable_type: 1,
        height_type: 0,
        tile_key: "tile_character",
    },
    StageTileSpec {
        x: 11,
        y: 6,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 0,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 1,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 2,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 3,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 4,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 5,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 6,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 7,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 8,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 9,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 10,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
    StageTileSpec {
        x: 11,
        y: 7,
        buildable_type: 0,
        height_type: 1,
        tile_key: "tile_forbidden",
    },
];

const STAGE_MORAN_SHUGE: StageMap = StageMap {
    width: 12,
    height: 8,
    normal_view: [0.0, -6.1, -9.76],
    side_view: [0.9448062140292399, -6.6, -10.61172366939915],
    tiles: &STAGE_MORAN_SHUGE_TILES,
};

fn current_stage_map() -> &'static StageMap {
    &STAGE_MORAN_SHUGE
}

fn project_stage_tile(
    stage: &StageMap,
    tile: StageTileSpec,
    width: f64,
    height: f64,
    side: bool,
) -> Option<PointI32> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    let ratio = height / width;
    let from_ratio = 9.0 / 16.0;
    let to_ratio = 3.0 / 4.0;
    let t = (from_ratio - ratio) / (from_ratio - to_ratio);
    let view = if side {
        stage.side_view
    } else {
        stage.normal_view
    };
    let camera_x = view[0] - 1.4 * t;
    let camera_y = view[1] - 2.8 * t;
    let camera_z = view[2];

    let mut x = tile.x as f64 - (stage.width as f64 - 1.0) / 2.0 - camera_x;
    let mut y = (stage.height as f64 - 1.0) / 2.0 - tile.y as f64 - camera_y;
    let mut z = tile.height_type as f64 * -0.4 - camera_z;

    if side {
        let yaw = 10.0_f64.to_radians();
        let cos_yaw = yaw.cos();
        let sin_yaw = yaw.sin();
        let next_x = cos_yaw * x + sin_yaw * z;
        let next_z = -sin_yaw * x + cos_yaw * z;
        x = next_x;
        z = next_z;
    }

    let pitch = 30.0_f64.to_radians();
    let cos_pitch = pitch.cos();
    let sin_pitch = pitch.sin();
    let next_y = cos_pitch * y - sin_pitch * z;
    let next_z = -sin_pitch * y - cos_pitch * z;
    y = next_y;
    z = next_z;

    if z == 0.0 {
        return None;
    }

    let tan_fov = 20.0_f64.to_radians().tan();
    let clip_x = (ratio / tan_fov) * x / -z;
    let clip_y = (1.0 / tan_fov) * y / -z;
    let screen_x = ((clip_x + 1.0) * 0.5 * width).round() as i32;
    let screen_y = ((1.0 - ((clip_y + 1.0) * 0.5)) * height).round() as i32;
    Some(PointI32 {
        x: screen_x,
        y: screen_y,
    })
}

fn snap_client_to_stage_tile(
    stage: &StageMap,
    client: PointI32,
    width: u32,
    height: u32,
    side: bool,
) -> Option<TileMatch> {
    let mut best: Option<TileMatch> = None;
    for tile in stage
        .tiles
        .iter()
        .copied()
        .filter(|tile| tile.tile_key != "tile_forbidden")
    {
        let Some(screen) = project_stage_tile(stage, tile, width as f64, height as f64, side)
        else {
            continue;
        };
        let dx = screen.x - client.x;
        let dy = screen.y - client.y;
        let distance_px = ((dx * dx + dy * dy) as f64).sqrt();
        let candidate = TileMatch {
            x: tile.x,
            y: tile.y,
            buildable_type: tile.buildable_type,
            tile_key: tile.tile_key,
            screen,
            distance_px,
        };
        if best
            .map(|current| candidate.distance_px < current.distance_px)
            .unwrap_or(true)
        {
            best = Some(candidate);
        }
    }
    best
}

fn snap_deploy_end_tile(client: PointI32, snapshot: ClockSnapshot) -> Option<TileMatch> {
    snap_client_to_stage_tile(
        current_stage_map(),
        client,
        snapshot.width,
        snapshot.height,
        true,
    )
}

fn snap_battlefield_selection_tile(client: PointI32, snapshot: ClockSnapshot) -> Option<TileMatch> {
    if !snapshot.battle_state.allows_battlefield_selection() {
        return None;
    }
    let normal = snap_client_to_stage_tile(
        current_stage_map(),
        client,
        snapshot.width,
        snapshot.height,
        false,
    );
    let side = snap_client_to_stage_tile(
        current_stage_map(),
        client,
        snapshot.width,
        snapshot.height,
        true,
    );
    let tile = match (normal, side) {
        (Some(lhs), Some(rhs)) => {
            if lhs.distance_px <= rhs.distance_px {
                lhs
            } else {
                rhs
            }
        }
        (Some(tile), None) | (None, Some(tile)) => tile,
        (None, None) => return None,
    };
    (tile.distance_px <= BATTLEFIELD_TILE_SNAP_MAX_DISTANCE_PX).then_some(tile)
}

fn scaled_oper_name_rect(width: u32, height: u32, roi: RelativeRectF32) -> Option<RectI32> {
    if width == 0 || height == 0 {
        return None;
    }

    let scaled = RectI32 {
        x: (roi.x * width as f32).round() as i32,
        y: (roi.y * height as f32).round() as i32,
        width: (roi.width * width as f32).round().max(1.0) as i32,
        height: (roi.height * height as f32).round().max(1.0) as i32,
    };

    let max_width = width as i32 - scaled.x;
    let max_height = height as i32 - scaled.y;
    if scaled.x < 0 || scaled.y < 0 || max_width <= 0 || max_height <= 0 {
        return None;
    }

    Some(RectI32 {
        x: scaled.x,
        y: scaled.y,
        width: scaled.width.min(max_width),
        height: scaled.height.min(max_height),
    })
}

fn extract_detail_name_crop(
    frame: &CapturedFrame,
    elapsed_frames: i32,
    roi: RelativeRectF32,
) -> Option<DetailNameCrop> {
    extract_detail_name_crop_from_parts(
        &frame.data,
        frame.width,
        frame.height,
        frame.format,
        elapsed_frames,
        roi,
    )
}

fn extract_detail_name_crop_from_parts(
    frame_data: &[u8],
    frame_width: u32,
    frame_height: u32,
    frame_format: PixelFormat,
    elapsed_frames: i32,
    roi: RelativeRectF32,
) -> Option<DetailNameCrop> {
    let rect = scaled_oper_name_rect(frame_width, frame_height, roi)?;
    let width = rect.width as u32;
    let height = rect.height as u32;
    let bpp = bytes_per_pixel(frame_format);
    let mut data_bgr = Vec::with_capacity(width as usize * height as usize * 3);
    for y in rect.y..(rect.y + rect.height) {
        let source_y = frame_height as i32 - 1 - y;
        if source_y < 0 {
            return None;
        }
        for x in rect.x..(rect.x + rect.width) {
            let offset = ((source_y as u32 * frame_width + x as u32) as usize) * bpp;
            if offset + bpp > frame_data.len() {
                return None;
            }
            match frame_format {
                PixelFormat::Bgr => {
                    data_bgr.extend_from_slice(&frame_data[offset..offset + 3]);
                }
                PixelFormat::Bgra => {
                    data_bgr.extend_from_slice(&frame_data[offset..offset + 3]);
                }
                PixelFormat::Rgba => {
                    data_bgr.push(frame_data[offset + 2]);
                    data_bgr.push(frame_data[offset + 1]);
                    data_bgr.push(frame_data[offset]);
                }
            }
        }
    }

    Some(DetailNameCrop {
        elapsed_frames,
        width,
        height,
        data_bgr,
    })
}

fn bytes_per_pixel(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Bgr => 3,
        PixelFormat::Bgra | PixelFormat::Rgba => 4,
    }
}

fn dump_detail_capture(
    runtime: &HookRuntime,
    label: &str,
    snapshot: ClockSnapshot,
) -> Option<DetailCapture> {
    let crop = runtime.clock.detail_name_crop()?;
    let sequence = runtime.capture_sequence.fetch_add(1, Ordering::Relaxed) + 1;
    let filename = format!(
        "{sequence:06}_{}_{}_f{}.bmp",
        label,
        format_time_slug_from_frames(snapshot.elapsed_frames),
        snapshot.elapsed_frames
    );
    let path = runtime.capture_dir.join(&filename);
    if let Err(error) = write_bgr_bitmap(&path, crop.width, crop.height, &crop.data_bgr) {
        let _ = runtime.tx.send(AppEvent::Warning(format!(
            "failed to write detail capture '{}': {error}",
            path.display()
        )));
        return None;
    }

    Some(DetailCapture {
        relative_path: format!("captures/{filename}"),
        captured_elapsed_frames: crop.elapsed_frames,
        width: crop.width,
        height: crop.height,
        crop,
    })
}

fn reconcile_pending_deploy_for_state_change(tx: &Sender<AppEvent>, battle_state: BattleStateCode) {
    let runtime_guard = hook_runtime_slot()
        .lock()
        .expect("hook runtime slot poisoned");
    let Some(runtime) = runtime_guard.as_ref() else {
        return;
    };

    let mut pending_guard = runtime
        .pending_deploy
        .lock()
        .expect("pending_deploy poisoned");
    let Some(pending) = pending_guard.as_mut() else {
        return;
    };

    if battle_state == BattleStateCode::AdjustingOperatorFacing {
        pending.saw_adjusting = true;
        return;
    }

    if battle_state.is_deploy_related() {
        return;
    }

    if pending.saw_adjusting {
        return;
    }

    if battle_state.allows_battlefield_selection() {
        let pending = pending_guard.take().expect("pending deploy must exist");
        *runtime
            .selected_tile
            .lock()
            .expect("selected_tile poisoned") = Some(pending.tile);
        let event = OperationEvent::from_timing(
            OperationKind::DeployResolved {
                tile: pending.tile,
                end_client: pending.end_client,
                detail_capture: pending.detail_capture,
                detail_name: None,
            },
            pending.drop_elapsed_frames,
            pending.drop_logical_frame,
            pending.drop_total_frames_in_cycle,
            pending.drop_battle_state,
        );
        let _ = tx.send(AppEvent::Operation(event));
        return;
    }
}

fn is_probable_facing_drag(
    pending_drag: PendingDrag,
    pending_deploy: &PendingDeploy,
    end_client: PointI32,
) -> bool {
    let start_dx = pending_drag.client.x - pending_deploy.end_client.x;
    let start_dy = pending_drag.client.y - pending_deploy.end_client.y;
    let end_dx = end_client.x - pending_drag.client.x;
    let end_dy = end_client.y - pending_drag.client.y;
    let elapsed_since_drop = pending_drag.frame_at_down - pending_deploy.drop_elapsed_frames;

    start_dx.abs().max(start_dy.abs()) <= DEPLOY_FACING_START_SNAP_PX
        && end_dx.abs().max(end_dy.abs()) >= DRAG_THRESHOLD_PX
        && (0..=DEPLOY_FACING_CONFIRM_WINDOW_FRAMES).contains(&elapsed_since_drop)
}

fn write_bgr_bitmap(path: &Path, width: u32, height: u32, data_bgr: &[u8]) -> Result<(), String> {
    let expected_len = width as usize * height as usize * 3;
    if data_bgr.len() != expected_len {
        return Err(format!(
            "invalid crop size: expected {} bytes, got {}",
            expected_len,
            data_bgr.len()
        ));
    }

    let row_stride = width as usize * 3;
    let padded_row_stride = (row_stride + 3) & !3;
    let pixel_array_size = padded_row_stride * height as usize;
    let file_size = 14usize + 40usize + pixel_array_size;
    let mut file =
        File::create(path).map_err(|e| format!("create failed for '{}': {e}", path.display()))?;

    file.write_all(b"BM")
        .map_err(|e| format!("write header failed for '{}': {e}", path.display()))?;
    file.write_all(&(file_size as u32).to_le_bytes())
        .map_err(|e| format!("write size failed for '{}': {e}", path.display()))?;
    file.write_all(&0u16.to_le_bytes())
        .and_then(|_| file.write_all(&0u16.to_le_bytes()))
        .and_then(|_| file.write_all(&(54u32).to_le_bytes()))
        .map_err(|e| format!("write file header failed for '{}': {e}", path.display()))?;

    file.write_all(&(40u32).to_le_bytes())
        .and_then(|_| file.write_all(&(width as i32).to_le_bytes()))
        .and_then(|_| file.write_all(&(height as i32).to_le_bytes()))
        .and_then(|_| file.write_all(&(1u16).to_le_bytes()))
        .and_then(|_| file.write_all(&(24u16).to_le_bytes()))
        .and_then(|_| file.write_all(&(0u32).to_le_bytes()))
        .and_then(|_| file.write_all(&(pixel_array_size as u32).to_le_bytes()))
        .and_then(|_| file.write_all(&(0i32).to_le_bytes()))
        .and_then(|_| file.write_all(&(0i32).to_le_bytes()))
        .and_then(|_| file.write_all(&(0u32).to_le_bytes()))
        .and_then(|_| file.write_all(&(0u32).to_le_bytes()))
        .map_err(|e| format!("write info header failed for '{}': {e}", path.display()))?;

    let padding = [0u8; 3];
    for row in (0..height as usize).rev() {
        let start = row * row_stride;
        let end = start + row_stride;
        file.write_all(&data_bgr[start..end])
            .map_err(|e| format!("write pixel data failed for '{}': {e}", path.display()))?;
        if padded_row_stride > row_stride {
            file.write_all(&padding[..padded_row_stride - row_stride])
                .map_err(|e| format!("write row padding failed for '{}': {e}", path.display()))?;
        }
    }

    Ok(())
}

impl OperatorNameRecognizer {
    fn new() -> Self {
        let tasks_json_path = resolve_runtime_path(MAA_TASKS_JSON_PATH);
        let replace_rules = load_name_replace_rules_from_tasks_json(&tasks_json_path)
            .map_err(|error| {
                log::warn!(
                    "failed to load MAA name replace rules from '{}': {error}",
                    tasks_json_path.display()
                );
                error
            })
            .unwrap_or_default();
        let engine = initialize_ocr_engine().map(Mutex::new);
        Self {
            engine,
            replace_rules,
        }
    }

    fn enrich_operation(&self, event: &mut OperationEvent) {
        match &mut event.kind {
            OperationKind::DeployResolved {
                detail_capture,
                detail_name,
                ..
            }
            | OperationKind::SkillHotkey {
                detail_capture,
                detail_name,
                ..
            }
            | OperationKind::RetreatHotkey {
                detail_capture,
                detail_name,
                ..
            } => {
                if detail_name.is_none() {
                    *detail_name = detail_capture
                        .as_ref()
                        .and_then(|capture| self.recognize_detail_name(&capture.crop));
                    if detail_name.is_none() {
                        if let Some(capture) = detail_capture.as_ref() {
                            log::warn!(
                                "operator OCR returned no name for '{}'",
                                capture.relative_path
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn recognize_detail_name(&self, crop: &DetailNameCrop) -> Option<String> {
        let mut engine = match self.engine.as_ref()?.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::warn!("operator OCR engine mutex was poisoned; reusing inner engine state");
                poisoned.into_inner()
            }
        };
        let mut best: Option<(i32, String)> = None;
        for (variant_index, sample) in detail_crop_variants(crop).into_iter().enumerate() {
            for binary in [false, true] {
                let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let raw = self.run_ocr(&mut engine, &sample, binary);
                    let normalized = raw
                        .as_deref()
                        .and_then(|text| self.normalize_ocr_text(text));
                    (raw, normalized)
                }));
                let (raw, candidate) = match attempt {
                    Ok(value) => value,
                    Err(payload) => {
                        log::warn!(
                            "operator OCR candidate panicked: variant={} binary={} crop={}x{} payload={}",
                            variant_index,
                            if binary { "yes" } else { "no" },
                            sample.width,
                            sample.height,
                            describe_panic_payload(&payload)
                        );
                        continue;
                    }
                };
                if let Some(text) = raw.as_ref() {
                    log::info!(
                        "operator OCR candidate: variant={} binary={} crop={}x{} raw='{}'",
                        variant_index,
                        if binary { "yes" } else { "no" },
                        sample.width,
                        sample.height,
                        text.replace('\'', "\\'")
                    );
                }
                let Some(candidate) = candidate else {
                    continue;
                };
                let score = score_name_candidate(&candidate);
                if score <= 0 {
                    continue;
                }
                let replace = best
                    .as_ref()
                    .map(|(current_score, current)| {
                        score > *current_score
                            || (score == *current_score && candidate.len() > current.len())
                    })
                    .unwrap_or(true);
                if replace {
                    best = Some((score, candidate));
                }
            }
        }
        let resolved = best.map(|(_, text)| text);
        if let Some(name) = resolved.as_ref() {
            log::info!("operator OCR resolved '{}'", name.replace('\'', "\\'"));
        }
        resolved
    }

    fn run_ocr(
        &self,
        engine: &mut OnnxOcrEngine,
        crop: &DetailNameCrop,
        binary: bool,
    ) -> Option<String> {
        engine.recognize(crop, binary)
    }

    fn normalize_ocr_text(&self, text: &str) -> Option<String> {
        let mut value = text
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        if value.is_empty() {
            return None;
        }

        for rule in &self.replace_rules {
            value = rule
                .pattern
                .replace_all(&value, rule.replacement.as_str())
                .into_owned();
        }

        let cleaned = value
            .trim_matches(|ch: char| {
                matches!(
                    ch,
                    '_' | '<' | '>' | '"' | '\\' | '/' | '|' | '[' | ']' | '{' | '}'
                )
            })
            .chars()
            .filter(|ch| !ch.is_control())
            .collect::<String>();
        let cleaned = strip_ascii_noise_around_cjk_name(&cleaned);

        (!cleaned.is_empty()).then_some(cleaned)
    }
}

fn initialize_ocr_engine() -> Option<OnnxOcrEngine> {
    let model_path = resolve_runtime_path(MAA_PADDLE_OCR_REC_MODEL_PATH);
    let keys_path = resolve_runtime_path(MAA_PADDLE_OCR_REC_KEYS_PATH);
    let dictionary = load_ocr_dictionary(&keys_path)
        .map_err(|error| {
            log::warn!(
                "failed to load ONNX OCR dictionary from '{}': {error}",
                keys_path.display()
            );
            error
        })
        .ok()?;

    let _ = ort::init().commit();
    let threads = determine_ocr_cpu_threads();
    let mut builder = Session::builder()
        .map_err(|error| {
            log::warn!("failed to create ONNX OCR session builder: {error}");
            error
        })
        .ok()?;
    builder = builder
        .with_intra_threads(threads)
        .map_err(|error| {
            log::warn!("failed to configure ONNX OCR session threads: {error}");
            error
        })
        .ok()?;

    let session = builder
        .commit_from_file(&model_path)
        .map_err(|error| {
            log::warn!(
                "failed to load ONNX OCR model from '{}': {error}",
                model_path.display()
            );
            error
        })
        .ok()?;

    let mut engine = OnnxOcrEngine {
        session,
        dictionary,
        input_height: OPER_NAME_OCR_INPUT_HEIGHT,
        base_input_width: OPER_NAME_OCR_BASE_INPUT_WIDTH,
        max_input_width: OPER_NAME_OCR_MAX_INPUT_WIDTH,
    };
    warm_up_ocr_engine(&mut engine);
    log::info!(
        "operator OCR initialized with ONNX model='{}' keys={} threads={}",
        model_path.display(),
        engine.dictionary.len(),
        threads
    );
    Some(engine)
}

fn warm_up_ocr_engine(engine: &mut OnnxOcrEngine) {
    let empty = DetailNameCrop {
        elapsed_frames: 0,
        width: 16,
        height: 16,
        data_bgr: vec![255u8; 16 * 16 * 3],
    };
    let _ = engine.recognize(&empty, false);
}

fn load_ocr_dictionary(path: &Path) -> Result<Vec<String>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("read failed for '{}': {e}", path.display()))?;
    let dictionary = content
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if dictionary.is_empty() {
        return Err(format!("dictionary '{}' is empty", path.display()));
    }
    Ok(dictionary)
}

fn determine_ocr_cpu_threads() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    if logical <= 2 {
        1
    } else if logical <= 4 {
        2
    } else if logical <= 12 {
        3
    } else {
        4
    }
}

fn resolve_runtime_path(relative: &str) -> PathBuf {
    let direct = PathBuf::from(relative);
    if direct.exists() {
        return direct;
    }

    // Development runs use the repository resource directory; packaged runs
    // resolve the same relative path from Tauri's resource root below.
    let development = Path::new("resources").join(relative);
    if development.exists() {
        return development;
    }

    if let Some(root) = std::env::var_os(RESOURCE_ROOT_ENV).map(PathBuf::from) {
        let candidate = root.join(relative);
        if candidate.exists() {
            return candidate;
        }
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            for base in exe_dir.ancestors() {
                let candidate = base.join(relative);
                if candidate.exists() {
                    return candidate;
                }
            }
        }
    }

    direct
}

fn load_name_replace_rules_from_tasks_json(path: &Path) -> Result<Vec<ReplaceRule>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("read failed for '{}': {e}", path.display()))?;
    let root: Value = serde_json::from_str(&content)
        .map_err(|e| format!("json parse failed for '{}': {e}", path.display()))?;
    let rules = root
        .get("CharsNameOcrReplace")
        .and_then(|value| value.get("ocrReplace"))
        .and_then(Value::as_array)
        .ok_or_else(|| "CharsNameOcrReplace.ocrReplace is missing".to_string())?;

    let mut compiled = Vec::new();
    let mut skipped = 0usize;
    for rule in rules {
        let pair = rule
            .as_array()
            .ok_or_else(|| "ocrReplace entry is not an array".to_string())?;
        if pair.len() != 2 {
            return Err("ocrReplace entry does not contain exactly two values".to_string());
        }
        let pattern = pair[0]
            .as_str()
            .ok_or_else(|| "ocrReplace pattern is not a string".to_string())?;
        let replacement = pair[1]
            .as_str()
            .ok_or_else(|| "ocrReplace replacement is not a string".to_string())?;
        let regex = match Regex::new(pattern) {
            Ok(value) => value,
            Err(error) => {
                skipped += 1;
                log::warn!(
                    "skipping unsupported MAA operator-name regex '{}': {}",
                    pattern,
                    error
                );
                continue;
            }
        };
        compiled.push(ReplaceRule {
            pattern: regex,
            replacement: replacement.to_string(),
        });
    }

    log::info!(
        "loaded {} operator-name replace rules from MAA (skipped {})",
        compiled.len(),
        skipped
    );
    Ok(compiled)
}

impl OnnxOcrEngine {
    fn recognize(&mut self, crop: &DetailNameCrop, binary: bool) -> Option<String> {
        let (input_tensor, input_width) = preprocess_detail_crop_for_paddle(
            crop,
            binary,
            self.input_height,
            self.base_input_width,
            self.max_input_width,
        )?;
        let dictionary = &self.dictionary;
        let session = &mut self.session;
        let input = TensorRef::from_array_view((
            [1usize, 3, self.input_height, input_width],
            &*input_tensor,
        ))
        .ok()?;
        let outputs = session.run(ort::inputs![input]).ok()?;
        let tensor = outputs.values().next()?;
        let (shape, logits) = tensor.try_extract_tensor::<f32>().ok()?;
        Self::decode_ctc_output(dictionary, shape, logits)
    }

    fn decode_ctc_output(dictionary: &[String], shape: &[i64], logits: &[f32]) -> Option<String> {
        if shape.len() != 3 {
            log::warn!(
                "unexpected ONNX OCR output rank: expected 3 dims, got {:?}",
                shape
            );
            return None;
        }

        let batch = usize::try_from(shape[0]).ok()?;
        let steps = usize::try_from(shape[1]).ok()?;
        let classes = usize::try_from(shape[2]).ok()?;
        if batch == 0 || steps == 0 || classes == 0 {
            return None;
        }
        if logits.len() < batch * steps * classes {
            log::warn!(
                "unexpected ONNX OCR output size: expected at least {}, got {}",
                batch * steps * classes,
                logits.len()
            );
            return None;
        }

        let mut text = String::new();
        let mut last_index = usize::MAX;
        for step in 0..steps {
            let offset = step * classes;
            let row = &logits[offset..offset + classes];
            let best_index = row
                .iter()
                .enumerate()
                .max_by(|lhs, rhs| lhs.1.total_cmp(rhs.1))
                .map(|(index, _)| index)?;
            if best_index != 0 && best_index != last_index {
                if let Some(token) = dictionary.get(best_index - 1) {
                    text.push_str(token);
                }
            }
            last_index = best_index;
        }

        Some(text)
    }
}

fn preprocess_detail_crop_for_paddle(
    crop: &DetailNameCrop,
    binary: bool,
    target_height: usize,
    base_width: usize,
    max_width: usize,
) -> Option<(Vec<f32>, usize)> {
    if crop.width == 0
        || crop.height == 0
        || target_height == 0
        || base_width == 0
        || max_width == 0
    {
        return None;
    }

    let source_rgb = if binary {
        preprocess_detail_crop_binary_rgb(crop)
    } else {
        detail_crop_to_rgb(crop)
    };
    let source_width = crop.width as usize;
    let source_height = crop.height as usize;
    let aspect_ratio = source_width as f32 / source_height as f32;
    let resized_width = ((target_height as f32 * aspect_ratio).ceil() as usize).max(1);
    let input_width = resized_width.max(base_width).min(max_width);
    let resized_width = resized_width.min(input_width).max(1);
    let resized = resize_rgb_bilinear(
        &source_rgb,
        source_width,
        source_height,
        resized_width,
        target_height,
    )?;

    let plane_size = target_height * input_width;
    let mut tensor = vec![0.0f32; 3 * plane_size];
    for y in 0..target_height {
        for x in 0..resized_width {
            let src_index = (y * resized_width + x) * 3;
            for channel in 0..3 {
                let value = resized[src_index + channel] as f32 / 255.0;
                let normalized = (value - 0.5) / 0.5;
                let dst_index = channel * plane_size + y * input_width + x;
                tensor[dst_index] = normalized;
            }
        }
    }

    Some((tensor, input_width))
}

fn detail_crop_to_rgb(crop: &DetailNameCrop) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(crop.width as usize * crop.height as usize * 3);
    for pixel in crop.data_bgr.chunks_exact(3) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }
    rgb
}

fn preprocess_detail_crop_binary_rgb(crop: &DetailNameCrop) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(crop.width as usize * crop.height as usize * 3);
    for pixel in crop.data_bgr.chunks_exact(3) {
        let b = pixel[0];
        let g = pixel[1];
        let r = pixel[2];
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let luminance =
            ((u16::from(r) * 299 + u16::from(g) * 587 + u16::from(b) * 114) / 1000) as u8;
        let likely_text =
            luminance >= OPER_NAME_PREPROCESS_THRESHOLD && max.saturating_sub(min) <= 96;
        let value = if likely_text { 0 } else { 255 };
        rgb.extend_from_slice(&[value, value, value]);
    }
    rgb
}

fn resize_rgb_bilinear(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    target_width: usize,
    target_height: usize,
) -> Option<Vec<u8>> {
    if source_width == 0 || source_height == 0 || target_width == 0 || target_height == 0 {
        return None;
    }
    if source.len() != source_width * source_height * 3 {
        return None;
    }

    let mut resized = vec![0u8; target_width * target_height * 3];
    let x_scale = source_width as f32 / target_width as f32;
    let y_scale = source_height as f32 / target_height as f32;

    for y in 0..target_height {
        let src_y = ((y as f32 + 0.5) * y_scale - 0.5).clamp(0.0, (source_height - 1) as f32);
        let y0 = src_y.floor() as usize;
        let y1 = (y0 + 1).min(source_height - 1);
        let wy = src_y - y0 as f32;

        for x in 0..target_width {
            let src_x = ((x as f32 + 0.5) * x_scale - 0.5).clamp(0.0, (source_width - 1) as f32);
            let x0 = src_x.floor() as usize;
            let x1 = (x0 + 1).min(source_width - 1);
            let wx = src_x - x0 as f32;
            let dst_index = (y * target_width + x) * 3;

            for channel in 0..3 {
                let top_left = source[(y0 * source_width + x0) * 3 + channel] as f32;
                let top_right = source[(y0 * source_width + x1) * 3 + channel] as f32;
                let bottom_left = source[(y1 * source_width + x0) * 3 + channel] as f32;
                let bottom_right = source[(y1 * source_width + x1) * 3 + channel] as f32;
                let top = top_left + (top_right - top_left) * wx;
                let bottom = bottom_left + (bottom_right - bottom_left) * wx;
                let value = top + (bottom - top) * wy;
                resized[dst_index + channel] = value.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    Some(resized)
}

fn detail_crop_variants(crop: &DetailNameCrop) -> Vec<DetailNameCrop> {
    let mut variants = Vec::with_capacity(3);
    variants.push(crop.clone());
    if let Some(name_line) = crop_detail_name_line(crop) {
        variants.push(name_line.clone());
        if let Some(name_line_tighter) = crop_detail_name_line(&name_line) {
            variants.push(name_line_tighter);
        }
    }
    variants
}

fn crop_detail_name_line(crop: &DetailNameCrop) -> Option<DetailNameCrop> {
    crop_detail_by_relative_rect(crop, OPER_NAME_LINE_CROP_RELATIVE_RECT)
}

fn crop_detail_by_relative_rect(
    crop: &DetailNameCrop,
    rect: RelativeRectF32,
) -> Option<DetailNameCrop> {
    if crop.width == 0 || crop.height == 0 {
        return None;
    }

    let x = (rect.x * crop.width as f32).round().max(0.0) as usize;
    let y = (rect.y * crop.height as f32).round().max(0.0) as usize;
    let width = (rect.width * crop.width as f32).round().max(1.0) as usize;
    let height = (rect.height * crop.height as f32).round().max(1.0) as usize;

    if x >= crop.width as usize || y >= crop.height as usize {
        return None;
    }

    let clamped_width = width.min(crop.width as usize - x);
    let clamped_height = height.min(crop.height as usize - y);
    let mut data_bgr = Vec::with_capacity(clamped_width * clamped_height * 3);
    let row_stride = crop.width as usize * 3;
    for row in 0..clamped_height {
        let src_start = (y + row) * row_stride + x * 3;
        let src_end = src_start + clamped_width * 3;
        data_bgr.extend_from_slice(&crop.data_bgr[src_start..src_end]);
    }

    Some(DetailNameCrop {
        elapsed_frames: crop.elapsed_frames,
        width: clamped_width as u32,
        height: clamped_height as u32,
        data_bgr,
    })
}

fn score_name_candidate(text: &str) -> i32 {
    let mut score = 0;
    for ch in text.chars() {
        if is_operator_name_char(ch) {
            score += if ch.is_ascii_alphanumeric() { 1 } else { 2 };
        } else {
            score -= 2;
        }
    }
    if text.chars().count() > 20 {
        score -= 10;
    }
    if text.chars().all(|ch| ch.is_ascii_digit()) {
        score -= 20;
    }
    score
}

fn is_operator_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(ch, '-' | '.' | '·' | '?' | '\'' | ':' | '“' | '”' | '²')
        || ('\u{3400}'..='\u{9FFF}').contains(&ch)
}

fn strip_ascii_noise_around_cjk_name(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let first_cjk = chars.iter().position(|ch| is_cjk_name_char(*ch));
    let last_cjk = chars.iter().rposition(|ch| is_cjk_name_char(*ch));
    match (first_cjk, last_cjk) {
        (Some(start), Some(end)) if start <= end => chars[start..=end].iter().collect(),
        _ => text.to_string(),
    }
}

fn is_cjk_name_char(ch: char) -> bool {
    ('\u{3400}'..='\u{9FFF}').contains(&ch) || matches!(ch, '“' | '”' | '·')
}

fn describe_panic_payload(payload: &Box<dyn Any + Send>) -> String {
    if let Some(value) = payload.downcast_ref::<&str>() {
        (*value).to_string()
    } else if let Some(value) = payload.downcast_ref::<String>() {
        value.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

impl RawInputEvent {
    fn from_clock(kind: RawInputKind, snapshot: ClockSnapshot) -> Self {
        Self {
            elapsed_frames: snapshot.elapsed_frames,
            logical_frame: snapshot.logical_frame,
            total_frames_in_cycle: snapshot.total_frames_in_cycle,
            battle_state: snapshot.battle_state,
            kind,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ForegroundStatus {
    target_hwnd: isize,
    target_root_hwnd: isize,
    foreground_hwnd: isize,
    foreground_root_hwnd: isize,
    matches: bool,
}

struct EventLogger {
    path: PathBuf,
    writer: LineWriter<File>,
}

impl EventLogger {
    fn create(path: PathBuf) -> Result<Self, String> {
        let file = File::create(&path)
            .map_err(|e| format!("failed to create event log '{}': {e}", path.display()))?;
        Ok(Self {
            path,
            writer: LineWriter::new(file),
        })
    }

    fn write_line(&mut self, line: &str) {
        if let Err(error) = writeln!(self.writer, "{line}") {
            eprintln!(
                "[warn] failed to write event log '{}': {error}",
                self.path.display()
            );
            return;
        }
        if let Err(error) = self.writer.flush() {
            eprintln!(
                "[warn] failed to flush event log '{}': {error}",
                self.path.display()
            );
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeLogPaths {
    pub session_dir: PathBuf,
    pub system_log_path: PathBuf,
    pub event_log_path: PathBuf,
    pub capture_dir: PathBuf,
}

impl RuntimeLogPaths {
    fn create(root: &Path) -> Result<Self, String> {
        let session_id = unix_timestamp_millis();
        let session_dir = root.join(format!("session-{session_id}"));
        fs::create_dir_all(&session_dir).map_err(|e| {
            format!(
                "failed to create log directory '{}': {e}",
                session_dir.display()
            )
        })?;
        let capture_dir = session_dir.join("captures");
        fs::create_dir_all(&capture_dir).map_err(|e| {
            format!(
                "failed to create capture directory '{}': {e}",
                capture_dir.display()
            )
        })?;

        Ok(Self {
            system_log_path: session_dir.join("system.log"),
            event_log_path: session_dir.join("events.log"),
            session_dir,
            capture_dir,
        })
    }
}

#[derive(Debug)]
struct CalibrationProgress {
    total_bar_width: i32,
    initial_width: Option<i32>,
    last_percent: f32,
}

impl CalibrationProgress {
    fn new(total_bar_width: i32) -> Self {
        Self {
            total_bar_width: total_bar_width.max(1),
            initial_width: None,
            last_percent: 0.0,
        }
    }

    fn update(&mut self, current_width: i32, completed_cycles: usize, collecting: bool) -> f32 {
        const CALIBRATION_CYCLES: usize = 2;

        let current = current_width.clamp(0, self.total_bar_width);
        let initial = *self.initial_width.get_or_insert(current);
        let wait_units = (self.total_bar_width - initial).max(0) as f32;
        let total_bar_width = self.total_bar_width as f32;
        let total_units = wait_units + CALIBRATION_CYCLES as f32 * total_bar_width;

        let completed_units = if collecting {
            let completed_cycles = completed_cycles.min(CALIBRATION_CYCLES);
            let current_cycle_units = if completed_cycles < CALIBRATION_CYCLES {
                current as f32
            } else {
                0.0
            };
            wait_units + completed_cycles as f32 * total_bar_width + current_cycle_units
        } else {
            (current - initial).clamp(0, self.total_bar_width) as f32
        };

        let percent = if total_units > 0.0 {
            completed_units / total_units * 100.0
        } else {
            0.0
        }
        .clamp(0.0, 100.0);
        self.last_percent = self.last_percent.max(percent);
        self.last_percent
    }
}

fn write_calibration_profile(
    config_path: &Path,
    calibration_data: &ruler_compat::analysis::calibration::CalibrationData,
    basename: &str,
) -> Result<(String, PathBuf), String> {
    fn sanitize_profile_base(value: &str) -> String {
        let sanitized: String = value
            .chars()
            .map(|ch| {
                if matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    '_'
                } else {
                    ch
                }
            })
            .collect();
        let trimmed = sanitized.trim();
        if trimmed.is_empty() {
            "profile".to_string()
        } else {
            trimmed.to_string()
        }
    }

    let calibration_dir = config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("calibration");
    fs::create_dir_all(&calibration_dir).map_err(|e| {
        format!(
            "failed to create calibration directory '{}': {e}",
            calibration_dir.display()
        )
    })?;
    let frame_counts = if calibration_data.profiles.is_empty() {
        "0f".to_string()
    } else {
        format!(
            "{}f",
            calibration_data
                .profiles
                .iter()
                .map(|profile| profile.total_frames.to_string())
                .collect::<Vec<_>>()
                .join("-")
        )
    };
    let filename = format!("{}_{}.json", sanitize_profile_base(basename), frame_counts);
    let path = calibration_dir.join(&filename);
    let contents = serde_json::to_string_pretty(calibration_data)
        .map_err(|e| format!("failed to serialize calibration: {e}"))?;
    fs::write(&path, contents)
        .map_err(|e| format!("failed to write calibration '{}': {e}", path.display()))?;
    Ok((filename, path))
}

struct HookThread {
    thread_id: u32,
    join_handle: JoinHandle<()>,
}

impl HookThread {
    fn spawn(
        tx: Sender<AppEvent>,
        clock: Arc<SharedClockState>,
        skill_vk: Option<u32>,
        retreat_vk: Option<u32>,
        capture_dir: PathBuf,
    ) -> Result<Self, String> {
        let runtime = HookRuntime {
            tx,
            clock,
            skill_vk,
            retreat_vk,
            pending_drag: Mutex::new(None),
            selected_tile: Mutex::new(None),
            pending_deploy: Mutex::new(None),
            capture_dir,
            capture_sequence: AtomicU32::new(0),
        };
        let mut runtime_guard = hook_runtime_slot()
            .lock()
            .expect("hook runtime slot poisoned");
        if runtime_guard.is_some() {
            return Err("hook runtime already initialized".to_string());
        }
        *runtime_guard = Some(runtime);
        drop(runtime_guard);

        let (ready_tx, ready_rx) = bounded::<Result<u32, String>>(1);
        let join_handle = thread::spawn(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            match install_hooks() {
                Ok((keyboard_hook, mouse_hook)) => {
                    let _ = ready_tx.send(Ok(thread_id));
                    if let Err(error) = run_hook_loop(keyboard_hook, mouse_hook) {
                        if let Some(runtime) = hook_runtime_slot()
                            .lock()
                            .expect("hook runtime slot poisoned")
                            .as_ref()
                        {
                            let _ = runtime
                                .tx
                                .send(AppEvent::Warning(format!("hook thread exited: {error}")));
                        }
                    }
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            }
        });

        let thread_id = ready_rx
            .recv()
            .map_err(|_| "hook thread initialization channel closed".to_string())??;

        Ok(Self {
            thread_id,
            join_handle,
        })
    }

    fn shutdown(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }

    fn join(self) {
        let _ = self.join_handle.join();
        let mut runtime_guard = hook_runtime_slot()
            .lock()
            .expect("hook runtime slot poisoned");
        *runtime_guard = None;
    }
}

fn install_hooks() -> Result<(HHOOK, HHOOK), String> {
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
    let instance = HINSTANCE(module.0);

    let keyboard_hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), instance, 0) }
            .map_err(|e| format!("SetWindowsHookExW keyboard failed: {e}"))?;

    let mouse_hook =
        match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), instance, 0) } {
            Ok(hook) => hook,
            Err(error) => {
                unsafe {
                    let _ = UnhookWindowsHookEx(keyboard_hook);
                }
                return Err(format!("SetWindowsHookExW mouse failed: {error}"));
            }
        };

    log::info!("hooks installed");
    Ok((keyboard_hook, mouse_hook))
}

fn run_hook_loop(keyboard_hook: HHOOK, mouse_hook: HHOOK) -> Result<(), String> {
    let mut message = MSG::default();
    loop {
        let status = unsafe { GetMessageW(&mut message, HWND(std::ptr::null_mut()), 0, 0) }.0;
        if status == -1 {
            unsafe {
                let _ = UnhookWindowsHookEx(mouse_hook);
                let _ = UnhookWindowsHookEx(keyboard_hook);
            }
            return Err("GetMessageW failed".to_string());
        }
        if status == 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    unsafe {
        let _ = UnhookWindowsHookEx(mouse_hook);
        let _ = UnhookWindowsHookEx(keyboard_hook);
    }
    log::info!("hooks removed");
    Ok(())
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam.0 as u32;
        if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
            let hook = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            handle_keyboard_event(hook, KeyAction::Down);
        } else if message == WM_KEYUP || message == WM_SYSKEYUP {
            let hook = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            handle_keyboard_event(hook, KeyAction::Up);
        }
    }
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let hook = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        match wparam.0 as u32 {
            WM_LBUTTONDOWN => handle_left_button_down(hook.pt),
            WM_LBUTTONUP => handle_left_button_up(hook.pt),
            _ => {}
        }
    }
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

fn handle_keyboard_event(hook: &KBDLLHOOKSTRUCT, action: KeyAction) {
    let runtime_guard = hook_runtime_slot()
        .lock()
        .expect("hook runtime slot poisoned");
    let Some(runtime) = runtime_guard.as_ref() else {
        return;
    };
    let snapshot = runtime.clock.snapshot_for_input();
    let foreground = foreground_status(snapshot.target_hwnd);
    let skill_match = runtime
        .skill_vk
        .is_some_and(|configured| key_matches(hook.vkCode, configured));
    let retreat_match = runtime
        .retreat_vk
        .is_some_and(|configured| key_matches(hook.vkCode, configured));
    let raw_event = RawInputEvent::from_clock(
        RawInputKind::Key {
            action,
            vk: hook.vkCode,
            scan_code: hook.scanCode,
            extended: hook.flags.0 & 0x01 != 0,
            injected: hook.flags.0 & 0x10 != 0,
            foreground_match: foreground.matches,
            skill_match,
            retreat_match,
            target_hwnd: foreground.target_hwnd,
            target_root_hwnd: foreground.target_root_hwnd,
            foreground_hwnd: foreground.foreground_hwnd,
            foreground_root_hwnd: foreground.foreground_root_hwnd,
        },
        snapshot,
    );
    let _ = runtime.tx.send(AppEvent::RawInput(raw_event));

    if !foreground.matches || !matches!(action, KeyAction::Down) {
        return;
    }

    let kind = if skill_match {
        let selected_tile = *runtime
            .selected_tile
            .lock()
            .expect("selected_tile poisoned");
        let detail_capture = dump_detail_capture(runtime, "skill", snapshot);
        Some(OperationKind::SkillHotkey {
            vk: hook.vkCode,
            selected_tile,
            detail_capture,
            detail_name: None,
        })
    } else if retreat_match {
        let selected_tile = *runtime
            .selected_tile
            .lock()
            .expect("selected_tile poisoned");
        let detail_capture = dump_detail_capture(runtime, "retreat", snapshot);
        Some(OperationKind::RetreatHotkey {
            vk: hook.vkCode,
            selected_tile,
            detail_capture,
            detail_name: None,
        })
    } else {
        None
    };

    if let Some(kind) = kind {
        let event = OperationEvent::from_clock(kind, snapshot);
        let _ = runtime.tx.send(AppEvent::Operation(event));
    }
}

fn handle_left_button_down(screen: POINT) {
    let runtime_guard = hook_runtime_slot()
        .lock()
        .expect("hook runtime slot poisoned");
    let Some(runtime) = runtime_guard.as_ref() else {
        return;
    };
    let snapshot = runtime.clock.snapshot_for_input();
    let foreground = foreground_status(snapshot.target_hwnd);
    let point_root_hwnd = root_window_from_point(screen);
    let Some(client) = screen_to_client(snapshot, screen) else {
        let raw_event = RawInputEvent::from_clock(
            RawInputKind::MouseLeft {
                action: MouseAction::Down,
                screen: PointI32 {
                    x: screen.x,
                    y: screen.y,
                },
                client: None,
                point_root_hwnd,
                foreground_match: foreground.matches,
                target_hwnd: foreground.target_hwnd,
                foreground_hwnd: foreground.foreground_hwnd,
                foreground_root_hwnd: foreground.foreground_root_hwnd,
            },
            snapshot,
        );
        let _ = runtime.tx.send(AppEvent::RawInput(raw_event));
        let mut guard = runtime.pending_drag.lock().expect("pending_drag poisoned");
        *guard = None;
        return;
    };
    let raw_event = RawInputEvent::from_clock(
        RawInputKind::MouseLeft {
            action: MouseAction::Down,
            screen: PointI32 {
                x: screen.x,
                y: screen.y,
            },
            client: Some(client),
            point_root_hwnd,
            foreground_match: foreground.matches,
            target_hwnd: foreground.target_hwnd,
            foreground_hwnd: foreground.foreground_hwnd,
            foreground_root_hwnd: foreground.foreground_root_hwnd,
        },
        snapshot,
    );
    let _ = runtime.tx.send(AppEvent::RawInput(raw_event));

    let pending = PendingDrag {
        client,
        frame_at_down: snapshot.elapsed_frames,
        state_at_down: snapshot.battle_state,
    };
    let mut guard = runtime.pending_drag.lock().expect("pending_drag poisoned");
    *guard = Some(pending);
}

fn handle_left_button_up(screen: POINT) {
    let runtime_guard = hook_runtime_slot()
        .lock()
        .expect("hook runtime slot poisoned");
    let Some(runtime) = runtime_guard.as_ref() else {
        return;
    };
    let snapshot = runtime.clock.snapshot_for_input();
    let foreground = foreground_status(snapshot.target_hwnd);
    let point_root_hwnd = root_window_from_point(screen);
    let mut guard = runtime.pending_drag.lock().expect("pending_drag poisoned");
    let Some(pending) = guard.take() else {
        let raw_event = RawInputEvent::from_clock(
            RawInputKind::MouseLeft {
                action: MouseAction::Up,
                screen: PointI32 {
                    x: screen.x,
                    y: screen.y,
                },
                client: screen_to_client(snapshot, screen),
                point_root_hwnd,
                foreground_match: foreground.matches,
                target_hwnd: foreground.target_hwnd,
                foreground_hwnd: foreground.foreground_hwnd,
                foreground_root_hwnd: foreground.foreground_root_hwnd,
            },
            snapshot,
        );
        let _ = runtime.tx.send(AppEvent::RawInput(raw_event));
        return;
    };

    let end_client = screen_to_client(snapshot, screen);
    let raw_event = RawInputEvent::from_clock(
        RawInputKind::MouseLeft {
            action: MouseAction::Up,
            screen: PointI32 {
                x: screen.x,
                y: screen.y,
            },
            client: end_client,
            point_root_hwnd,
            foreground_match: foreground.matches,
            target_hwnd: foreground.target_hwnd,
            foreground_hwnd: foreground.foreground_hwnd,
            foreground_root_hwnd: foreground.foreground_root_hwnd,
        },
        snapshot,
    );
    let _ = runtime.tx.send(AppEvent::RawInput(raw_event));

    let Some(end_client) = end_client else {
        return;
    };

    let dx = end_client.x - pending.client.x;
    let dy = end_client.y - pending.client.y;
    let is_drag = dx.abs().max(dy.abs()) >= DRAG_THRESHOLD_PX;
    let kind = if is_drag {
        let deploy_end_tile = if snapshot.battle_state == BattleStateCode::DeployingOperator {
            snap_deploy_end_tile(end_client, snapshot)
        } else {
            None
        };
        if let Some(tile) = deploy_end_tile {
            let detail_capture = dump_detail_capture(runtime, "deploy", snapshot);
            let pending_deploy = PendingDeploy {
                tile,
                end_client,
                drop_elapsed_frames: snapshot.elapsed_frames,
                drop_logical_frame: snapshot.logical_frame,
                drop_total_frames_in_cycle: snapshot.total_frames_in_cycle,
                drop_battle_state: snapshot.battle_state,
                detail_capture,
                saw_adjusting: false,
            };
            *runtime
                .pending_deploy
                .lock()
                .expect("pending_deploy poisoned") = Some(pending_deploy);
        }
        OperationKind::Drag {
            start_client: pending.client,
            end_client,
            start_frame: pending.frame_at_down,
            start_state: pending.state_at_down,
            deploy_like: pending.state_at_down.is_deploy_related()
                || snapshot.battle_state.is_deploy_related(),
            deploy_end_tile,
        }
    } else {
        let selected_tile = if foreground.matches {
            snap_battlefield_selection_tile(pending.client, snapshot)
        } else {
            None
        };
        if let Some(tile) = selected_tile {
            *runtime
                .selected_tile
                .lock()
                .expect("selected_tile poisoned") = Some(tile);
        }
        if !snapshot.battle_state.is_deploy_related() {
            *runtime
                .pending_deploy
                .lock()
                .expect("pending_deploy poisoned") = None;
        }
        OperationKind::LeftClick {
            client: PointI32 {
                x: pending.client.x,
                y: pending.client.y,
            },
            selected_tile,
        }
    };

    let event = OperationEvent::from_clock(kind, snapshot);
    let _ = runtime.tx.send(AppEvent::Operation(event));

    let should_finalize_deploy = if is_drag && foreground.matches {
        let pending_deploy_guard = runtime
            .pending_deploy
            .lock()
            .expect("pending_deploy poisoned");
        pending.state_at_down == BattleStateCode::AdjustingOperatorFacing
            || pending_deploy_guard.as_ref().is_some_and(|pending_deploy| {
                is_probable_facing_drag(pending, pending_deploy, end_client)
            })
    } else {
        false
    };
    if should_finalize_deploy {
        let pending_deploy = runtime
            .pending_deploy
            .lock()
            .expect("pending_deploy poisoned")
            .take();
        if let Some(pending_deploy) = pending_deploy {
            let tile = pending_deploy.tile;
            let deploy_event = OperationEvent::from_clock(
                OperationKind::DeployResolved {
                    tile,
                    end_client: pending_deploy.end_client,
                    detail_capture: pending_deploy.detail_capture,
                    detail_name: None,
                },
                snapshot,
            );
            let _ = runtime.tx.send(AppEvent::Operation(deploy_event));
            *runtime
                .selected_tile
                .lock()
                .expect("selected_tile poisoned") = Some(tile);
        }
    }
}

fn key_matches(observed_vk: u32, configured_vk: u32) -> bool {
    if observed_vk == configured_vk {
        return true;
    }

    let configured_digit = ascii_digit_value(configured_vk);
    let observed_digit = ascii_digit_value(observed_vk).or_else(|| numpad_digit_value(observed_vk));
    configured_digit.is_some() && configured_digit == observed_digit
}

fn ascii_digit_value(vk: u32) -> Option<u32> {
    if (b'0' as u32..=b'9' as u32).contains(&vk) {
        Some(vk - b'0' as u32)
    } else {
        None
    }
}

fn numpad_digit_value(vk: u32) -> Option<u32> {
    if (0x60..=0x69).contains(&vk) {
        Some(vk - 0x60)
    } else {
        None
    }
}

fn screen_to_client(snapshot: ClockSnapshot, screen: POINT) -> Option<PointI32> {
    if snapshot.target_hwnd == 0 || snapshot.width == 0 || snapshot.height == 0 {
        return None;
    }

    let target_hwnd = HWND(snapshot.target_hwnd as *mut c_void);
    let target_root = unsafe { GetAncestor(target_hwnd, GA_ROOT) };
    let point_root = unsafe { GetAncestor(WindowFromPoint(screen), GA_ROOT) };
    if point_root.0.is_null()
        || target_root.0.is_null()
        || (point_root.0 as isize != snapshot.target_hwnd && point_root.0 != target_root.0)
    {
        return None;
    }

    let client_x = screen.x - snapshot.client_left;
    let client_y = screen.y - snapshot.client_top;
    if client_x < 0
        || client_y < 0
        || client_x >= snapshot.width as i32
        || client_y >= snapshot.height as i32
    {
        return None;
    }

    Some(PointI32 {
        x: client_x,
        y: client_y,
    })
}

#[derive(Clone, Debug)]
struct VisibleWindowInfo {
    hwnd: isize,
    title: String,
    class_name: String,
}

#[derive(Clone, Debug)]
struct MuMuWindowCandidate {
    client_hwnd: isize,
    priority: u8,
    foreground: bool,
}

fn discover_mumu_window_match() -> Option<MuMuWindowMatch> {
    let foreground_root = unsafe {
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() {
            0
        } else {
            GetAncestor(foreground, GA_ROOT).0 as isize
        }
    };

    let mut candidates = Vec::new();
    for root in enumerate_visible_top_windows() {
        let title_lower = root.title.to_ascii_lowercase();
        let class_lower = root.class_name.to_ascii_lowercase();
        let contains_mumu = title_lower.contains("mumu") || class_lower.contains("mumu");
        let exact_mumu12 = root.title == "MuMu模拟器12";
        let looks_like_arknights_mumu = root.title.contains("明日方舟") && contains_mumu;

        let children = enumerate_child_windows(root.hwnd);
        if let Some(child) = children
            .iter()
            .find(|child| child.title == MUMU_CHILD_WINDOW_TITLE)
        {
            candidates.push(MuMuWindowCandidate {
                client_hwnd: child.hwnd,
                priority: if exact_mumu12 { 0 } else { 1 },
                foreground: root.hwnd == foreground_root,
            });
            continue;
        }

        if let Some(child) = children.iter().find(|child| {
            child.title.to_ascii_lowercase().contains("mumuplayer")
                || child.class_name.to_ascii_lowercase().contains("mumuplayer")
        }) {
            candidates.push(MuMuWindowCandidate {
                client_hwnd: child.hwnd,
                priority: if contains_mumu { 2 } else { 3 },
                foreground: root.hwnd == foreground_root,
            });
            continue;
        }

        if looks_like_arknights_mumu || contains_mumu {
            candidates.push(MuMuWindowCandidate {
                client_hwnd: root.hwnd,
                priority: if looks_like_arknights_mumu { 4 } else { 5 },
                foreground: root.hwnd == foreground_root,
            });
        }
    }

    candidates.sort_by_key(|candidate| {
        (
            !candidate.foreground,
            candidate.priority,
            candidate.client_hwnd,
        )
    });
    candidates
        .into_iter()
        .next()
        .map(|candidate| MuMuWindowMatch {
            client_hwnd: candidate.client_hwnd,
        })
}

fn window_info_for_client_hwnd(hwnd_value: isize) -> Option<WindowInfo> {
    let hwnd = HWND(hwnd_value as *mut c_void);
    if hwnd.0.is_null() || !unsafe { IsWindow(hwnd) }.as_bool() {
        return None;
    }

    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return None;
    }

    let mut origin = POINT { x: 0, y: 0 };
    if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
        return None;
    }

    Some(WindowInfo {
        hwnd: hwnd_value,
        client_left: origin.x,
        client_top: origin.y,
        width: width as u32,
        height: height as u32,
    })
}

fn enumerate_visible_top_windows() -> Vec<VisibleWindowInfo> {
    let mut windows = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_visible_window_proc),
            LPARAM((&mut windows as *mut Vec<VisibleWindowInfo>) as isize),
        );
    }
    windows
}

fn enumerate_child_windows(parent_hwnd: isize) -> Vec<VisibleWindowInfo> {
    let mut windows = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            HWND(parent_hwnd as *mut c_void),
            Some(enum_visible_window_proc),
            LPARAM((&mut windows as *mut Vec<VisibleWindowInfo>) as isize),
        );
    }
    windows
}

unsafe extern "system" fn enum_visible_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let title = window_text(hwnd);
    let class_name = window_class(hwnd);
    if title.trim().is_empty() && class_name.trim().is_empty() {
        return BOOL(1);
    }
    let windows = &mut *(lparam.0 as *mut Vec<VisibleWindowInfo>);
    windows.push(VisibleWindowInfo {
        hwnd: hwnd.0 as isize,
        title,
        class_name,
    });
    BOOL(1)
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

fn cursor_state_in_client(window: WindowInfo) -> Option<CursorState> {
    if window.width == 0 || window.height == 0 {
        return None;
    }

    let hwnd = HWND(window.hwnd as *mut c_void);
    let mut point = POINT::default();
    unsafe {
        if !GetCursorPos(&mut point).as_bool() {
            return None;
        }
    }

    let screen_x = point.x;
    let screen_y = point.y;
    unsafe {
        if !ScreenToClient(hwnd, &mut point).as_bool() {
            return None;
        }
    }

    let client_x = point.x;
    let client_y = point.y;
    if client_x < 0
        || client_y < 0
        || client_x >= window.width as i32
        || client_y >= window.height as i32
    {
        return None;
    }

    let top_hwnd = unsafe {
        WindowFromPoint(POINT {
            x: screen_x,
            y: screen_y,
        })
    };
    let uncovered = top_hwnd.0 as isize == window.hwnd;
    let native_visible = cursor_info()
        .map(|info| (info.flags & CURSOR_SHOWING) != 0)
        .unwrap_or(true);

    Some(CursorState {
        client_x,
        client_y,
        native_visible,
        uncovered,
    })
}

fn cursor_info() -> Option<NativeCursorInfo> {
    let mut info = NativeCursorInfo {
        cb_size: std::mem::size_of::<NativeCursorInfo>() as u32,
        flags: 0,
        h_cursor: std::ptr::null_mut(),
        pt_screen_pos: POINT::default(),
    };
    unsafe { GetCursorInfo(&mut info).as_bool().then_some(info) }
}

fn cost_bar_rect(roi: (i32, i32, i32), width: u32, height: u32, ui_scaler: f64) -> CursorGuardRect {
    let scale =
        cursor_layout_scale(width, height) * ruler_compat::analysis::roi::ui_edge_scale(ui_scaler);
    let half_height = (4.0 * scale).round().max(3.0) as i32;
    let max_x = width as i32;
    let max_y = height as i32;
    CursorGuardRect {
        left: roi.0.min(roi.1).clamp(0, max_x),
        right: roi.0.max(roi.1).clamp(0, max_x),
        top: (roi.2 - half_height).clamp(0, max_y),
        bottom: (roi.2 + half_height + 1).clamp(0, max_y),
    }
}

fn cost_area_rect(width: u32, height: u32, ui_scaler: f64) -> CursorGuardRect {
    let scale =
        cursor_layout_scale(width, height) * ruler_compat::analysis::roi::ui_edge_scale(ui_scaler);
    let max_x = width as i32;
    let max_y = height as i32;
    let pad_y = (2.0 * scale).round().max(1.0) as i32;

    CursorGuardRect {
        left: (width as f64 - COST_SIGN_LEFT_OFFSET_FROM_RIGHT_REF * scale).floor() as i32,
        right: (width as f64 - COST_SIGN_RIGHT_OFFSET_FROM_RIGHT_REF * scale).ceil() as i32,
        top: (height as f64 - COST_SIGN_TOP_OFFSET_FROM_BOTTOM_REF * scale).floor() as i32 - pad_y,
        bottom: (height as f64 - COST_SIGN_BOTTOM_OFFSET_FROM_BOTTOM_REF * scale).ceil() as i32
            + pad_y,
    }
    .clamped(max_x, max_y)
}

fn cursor_layout_scale(width: u32, height: u32) -> f64 {
    if width == 0 || height == 0 {
        return 1.0;
    }
    let aspect = width as f64 / height as f64;
    if aspect >= PC_CURSOR_REF_ASPECT_RATIO {
        height as f64 / PC_CURSOR_REF_HEIGHT
    } else {
        width as f64 / PC_CURSOR_REF_WIDTH
    }
}

fn cursor_size_factor(cursor_size: f64) -> f64 {
    PC_CURSOR_MIN_SIZE_FACTOR + (1.0 - PC_CURSOR_MIN_SIZE_FACTOR) * cursor_size.clamp(0.0, 1.0)
}

fn finite_or_default(value: f64, default: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        default
    }
}

fn foreground_status(target_hwnd: isize) -> ForegroundStatus {
    unsafe {
        let target = if target_hwnd == 0 {
            HWND::default()
        } else {
            HWND(target_hwnd as *mut _)
        };
        let target_root = if target.0.is_null() {
            HWND::default()
        } else {
            GetAncestor(target, GA_ROOT)
        };
        let foreground = GetForegroundWindow();
        let foreground_root = if foreground.0.is_null() {
            HWND::default()
        } else {
            GetAncestor(foreground, GA_ROOT)
        };
        let foreground_hwnd = foreground.0 as isize;
        let foreground_root_hwnd = foreground_root.0 as isize;
        let target_root_hwnd = target_root.0 as isize;
        let matches = target_hwnd != 0
            && (foreground_hwnd == target_hwnd
                || foreground_root_hwnd == target_hwnd
                || target_root_hwnd == foreground_hwnd
                || (foreground_root_hwnd != 0 && foreground_root_hwnd == target_root_hwnd));

        ForegroundStatus {
            target_hwnd,
            target_root_hwnd,
            foreground_hwnd,
            foreground_root_hwnd,
            matches,
        }
    }
}

fn root_window_from_point(screen: POINT) -> isize {
    unsafe {
        let window = WindowFromPoint(screen);
        if window.0.is_null() {
            return 0;
        }
        let root = GetAncestor(window, GA_ROOT);
        root.0 as isize
    }
}

fn install_ctrlc(stop: Arc<AtomicBool>, hook_thread_id: Arc<AtomicU32>) -> Result<(), String> {
    ctrlc::set_handler(move || {
        stop.store(true, Ordering::SeqCst);
        let thread_id = hook_thread_id.load(Ordering::SeqCst);
        if thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    })
    .map_err(|e| format!("failed to install Ctrl+C handler: {e}"))
}

fn elapsed_ns_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn calibration_path_from_config(
    config_path: &Path,
    active_profile: Option<&str>,
) -> Option<PathBuf> {
    active_profile.map(|name| {
        config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("calibration")
            .join(name)
    })
}

fn resolve_calibration_path(
    config_path: &Path,
    active_profile: Option<&str>,
) -> Result<PathBuf, String> {
    calibration_path_from_config(config_path, active_profile).ok_or_else(|| {
        "config.json has no active_calibration_profile; first version requires a working CostBarRuler profile".to_string()
    })
}

fn load_supported_ruler_config(config_path: &Path) -> Result<RulerConfig, String> {
    let config = RulerConfig::load_from_path(config_path).map_err(|e| e.to_string())?;
    if config.capture_type != "window" && config.capture_type != "mumu" {
        return Err("当前 GUI 版本暂时支持 PC 窗口捕获（type=\"window\"）和 MuMu 原生捕获（type=\"mumu\"）。".to_string());
    }
    Ok(config)
}

fn capture_window_info(
    backend: &dyn CaptureBackend,
    tracker: Option<&ExternalWindowTracker>,
) -> Option<WindowInfo> {
    backend
        .window_info()
        .or_else(|| tracker.and_then(ExternalWindowTracker::current_window))
}

fn connect_capture_backend(config: &RulerConfig) -> Result<Box<dyn CaptureBackend>, String> {
    let capture_config = config.to_capture_config().map_err(|e| e.to_string())?;
    let mut backend =
        create_backend(capture_config.clone()).map_err(|e| format!("backend init failed: {e}"))?;
    if let Err(first_error) = backend.connect() {
        let mut retry_config = capture_config;
        retry_config.window_handle = None;
        let mut retry_backend =
            create_backend(retry_config).map_err(|e| format!("backend retry init failed: {e}"))?;
        retry_backend.connect().map_err(|retry_error| {
            format!(
                "backend connect failed: {first_error}; retry without cached window_handle failed: {retry_error}"
            )
        })?;
        backend = retry_backend;
    }
    Ok(backend)
}

fn read_pc_ui_scaler() -> Result<Option<f64>, String> {
    read_pc_common_setting_number("uiScaler")
}

fn read_pc_cursor_size() -> Result<Option<f64>, String> {
    read_pc_common_setting_number("cursorSize")
}

fn read_pc_common_setting_number(field: &str) -> Result<Option<f64>, String> {
    const REGISTRY_SUBKEY: &str = "Software\\Hypergryph\\Arknights";
    const REGISTRY_VALUE: &str = "common_setting_h2012961537";

    let subkey = wide(REGISTRY_SUBKEY);
    let value_name = wide(REGISTRY_VALUE);
    let mut value_type = REG_VALUE_TYPE::default();
    let mut byte_len = 0u32;

    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value_name.as_ptr()),
            RRF_RT_ANY,
            Some(&mut value_type),
            None,
            Some(&mut byte_len),
        )
    };
    if !status.is_ok() || byte_len == 0 {
        return Ok(None);
    }

    let mut buffer = vec![0u8; byte_len as usize];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value_name.as_ptr()),
            RRF_RT_ANY,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut byte_len),
        )
    };
    if !status.is_ok() {
        return Ok(None);
    }

    buffer.truncate(byte_len as usize);
    let settings_json = if value_type == REG_SZ {
        let wide = buffer
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        let len = wide.iter().position(|ch| *ch == 0).unwrap_or(wide.len());
        if len == 0 {
            None
        } else {
            Some(String::from_utf16_lossy(&wide[..len]))
        }
    } else if value_type == REG_BINARY {
        let len = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        if len == 0 {
            None
        } else {
            Some(String::from_utf8_lossy(&buffer[..len]).into_owned())
        }
    } else {
        None
    };

    let Some(settings_json) = settings_json else {
        return Ok(None);
    };

    let value: Value = serde_json::from_str(&settings_json)
        .map_err(|error| format!("failed to parse Arknights common settings JSON: {error}"))?;
    Ok(value.get(field).and_then(Value::as_f64))
}

fn resolve_runtime_ui_scaler(config: &RulerConfig) -> f64 {
    if config.capture_type == "window" {
        match read_pc_ui_scaler() {
            Ok(Some(value)) => {
                let ui_scaler = value.clamp(0.0, 1.0);
                log::info!("detected Arknights PC uiScaler={ui_scaler:.3} from registry");
                ui_scaler
            }
            Ok(None) => {
                let ui_scaler = config.effective_ui_scaler();
                log::info!(
                    "Arknights PC uiScaler registry value not found; using config value {ui_scaler:.3}"
                );
                ui_scaler
            }
            Err(error) => {
                let ui_scaler = config.effective_ui_scaler();
                log::warn!(
                    "failed to read Arknights PC uiScaler: {error}; using config value {ui_scaler:.3}"
                );
                ui_scaler
            }
        }
    } else {
        ruler_compat::analysis::roi::DEFAULT_UI_SCALER
    }
}

fn spawn_clock_worker(
    options: RecorderOptions,
    tx: Sender<AppEvent>,
    shared_clock: Arc<SharedClockState>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<Result<(), String>>, String> {
    let (ready_tx, ready_rx) = bounded::<Result<(), String>>(1);
    let handle = thread::spawn(move || {
        let startup = (|| -> Result<(_, _, _, _, _, _), String> {
            let config = load_supported_ruler_config(&options.config_path)?;
            let calibration_path = resolve_calibration_path(
                &options.config_path,
                config.active_calibration_profile.as_deref(),
            )?;

            let backend = connect_capture_backend(&config)?;
            let tracker = ExternalWindowTracker::from_config(&config);

            let (width, height) = backend.dimensions();
            let ui_scaler = resolve_runtime_ui_scaler(&config);
            let cursor_size = read_pc_cursor_size()
                .ok()
                .flatten()
                .unwrap_or(1.0)
                .clamp(0.0, 1.0);
            let roi = find_cost_bar_roi_with_ui_scaler(width as i32, height as i32, ui_scaler);
            let mut analyzer = Analyzer::new();
            analyzer.set_ui_scaler(ui_scaler);
            analyzer.set_roi(width as i32, height as i32);
            analyzer.load_calibration(&calibration_path).map_err(|e| {
                format!(
                    "failed to load calibration '{}': {e}",
                    calibration_path.display()
                )
            })?;

            if let Some(window_info) = capture_window_info(backend.as_ref(), tracker.as_ref()) {
                let _ = tx.send(AppEvent::Info(format!(
                    "connected hwnd=0x{:X} client=({}, {}) size={}x{} ui_scaler={:.3} calibration='{}'",
                    window_info.hwnd,
                    window_info.client_left,
                    window_info.client_top,
                    window_info.width,
                    window_info.height,
                    ui_scaler,
                    calibration_path.display()
                )));
                shared_clock.update_window(window_info);
            } else {
                let _ = tx.send(AppEvent::Warning(format!(
                    "backend connected but window_info is unavailable; size={}x{} ui_scaler={:.3} calibration='{}'",
                    width,
                    height,
                    ui_scaler,
                    calibration_path.display()
                )));
            }

            let capture_interval = config
                .screenshot_delay_ms
                .filter(|value| *value > 0.0)
                .map(|value| Duration::from_secs_f64(value / 1000.0));

            Ok((
                backend,
                analyzer,
                capture_interval,
                roi,
                cursor_size,
                tracker,
            ))
        })();

        let (mut backend, mut analyzer, capture_interval, roi, cursor_size, tracker) = match startup
        {
            Ok(values) => {
                let _ = ready_tx.send(Ok(()));
                values
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error.clone()));
                return Err(error);
            }
        };

        let started = Instant::now();
        let mut last_reported_second = -1;
        let mut last_reported_state = BattleStateCode::Unknown;
        let mut capture_failures = 0u32;
        let mut cursor_guard = Some(SelfDrawnCursorGuard::new(analyzer.ui_scaler(), cursor_size));
        let mut cursor_blocked = false;
        let mut cursor_blocked_frames = 0u32;

        while !stop.load(Ordering::SeqCst) {
            let loop_started = Instant::now();
            let frame = match backend.capture_frame() {
                Ok(frame) => {
                    capture_failures = 0;
                    shared_clock.update_capture_state(CaptureStateCode::Active);
                    frame
                }
                Err(error) => {
                    capture_failures = capture_failures.saturating_add(1);
                    shared_clock.update_latest_frame(None);
                    shared_clock.update_detail_name_crop(None);
                    let window_missing = error.contains("no frame arrived within timeout");
                    shared_clock.update_capture_state(if window_missing {
                        CaptureStateCode::WindowMissing
                    } else {
                        CaptureStateCode::Error
                    });
                    if !window_missing && (capture_failures <= 3 || capture_failures % 30 == 0) {
                        let _ = tx.send(AppEvent::Warning(format!("capture failed: {error}")));
                    }
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };

            let battle_state_raw = scanner::detect_battle_state_with_ui_scaler(
                &frame.data,
                frame.width,
                frame.height,
                frame.format,
                analyzer.ui_scaler(),
            );
            let window_info = capture_window_info(backend.as_ref(), tracker.as_ref());
            if cursor_guard.as_mut().is_some_and(|guard| {
                guard.should_pause_for_frame(window_info, roi, battle_state_raw)
            }) {
                cursor_blocked_frames = cursor_blocked_frames.saturating_add(1);
                if !cursor_blocked {
                    cursor_blocked = true;
                    let _ = tx.send(AppEvent::Info(
                        "cursor overlaps cost bar; skipping occluded frames".to_string(),
                    ));
                } else if cursor_blocked_frames % 60 == 0 {
                    let _ = tx.send(AppEvent::Info(format!(
                        "cursor still overlaps cost bar; skipped {} frames",
                        cursor_blocked_frames
                    )));
                }
                if let Some(interval) = capture_interval {
                    let elapsed = loop_started.elapsed();
                    if interval > elapsed {
                        thread::sleep(interval - elapsed);
                    }
                }
                continue;
            }
            if cursor_blocked {
                cursor_blocked = false;
                cursor_blocked_frames = 0;
                let _ = tx.send(AppEvent::Info(
                    "cursor left cost bar; analysis resumed".to_string(),
                ));
            }

            let timestamp_ns = elapsed_ns_since(started);
            let result = match analyzer.analyze_captured_frame_at(&frame, Some(timestamp_ns)) {
                Ok(result) => result,
                Err(error) => {
                    shared_clock.update_capture_state(CaptureStateCode::Error);
                    let _ = tx.send(AppEvent::Warning(format!("analysis failed: {error}")));
                    continue;
                }
            };

            shared_clock.update_latest_frame(Some(LatestCapturedFrame {
                elapsed_frames: result.elapsed_frames,
                width: frame.width,
                height: frame.height,
                format: frame.format,
                data: frame.data.clone(),
            }));

            shared_clock.update_detail_name_crop(extract_detail_name_crop(
                &frame,
                result.elapsed_frames,
                shared_clock.detail_name_roi,
            ));

            if let Some(window_info) = window_info {
                shared_clock.update_window(window_info);
            }

            let battle_state = BattleStateCode::from_ruler(result.battle_state);
            shared_clock.update_timing(
                result.elapsed_frames,
                result.logical_frame,
                result.total_frames_in_cycle,
                battle_state,
            );

            let current_second = result.elapsed_frames / FRAMES_PER_SECOND;
            reconcile_pending_deploy_for_state_change(&tx, battle_state);
            if battle_state != last_reported_state || current_second != last_reported_second {
                let _ = tx.send(AppEvent::Clock(ClockEvent {
                    elapsed_frames: result.elapsed_frames,
                    battle_state,
                }));
                last_reported_state = battle_state;
                last_reported_second = current_second;
            }

            if let Some(interval) = capture_interval {
                let elapsed = loop_started.elapsed();
                if interval > elapsed {
                    thread::sleep(interval - elapsed);
                }
            }
        }

        backend.disconnect();
        Ok(())
    });
    ready_rx
        .recv()
        .map_err(|_| "clock startup channel closed".to_string())??;
    Ok(handle)
}

pub fn run_calibration<F, C>(
    options: CalibrationOptions,
    mut on_update: F,
    should_cancel: C,
) -> Result<CalibrationOutcome, String>
where
    F: FnMut(CalibrationUpdate),
    C: Fn() -> bool,
{
    const CALIBRATION_CYCLES: usize = 2;

    let log_paths = RuntimeLogPaths::create(&options.logs_root)?;
    let mut event_logger = EventLogger::create(log_paths.event_log_path.clone())?;
    let session_dir = log_paths.session_dir.clone();
    event_logger.write_line(&format!(
        "[calibration] started_at_unix={} config='{}' logs_dir='{}'",
        unix_timestamp_seconds(),
        options.config_path.display(),
        session_dir.display()
    ));

    let emit = |update: &CalibrationUpdate, event_logger: &mut EventLogger| {
        event_logger.write_line(&format!(
            "[calibration] stage={:?} progress={:.1}% {}",
            update.stage, update.progress_percent, update.message
        ));
    };

    let mut publish = |stage: CalibrationStage, progress_percent: f32, message: String| {
        let update = CalibrationUpdate {
            stage,
            progress_percent,
            message,
            session_dir: Some(session_dir.clone()),
        };
        emit(&update, &mut event_logger);
        on_update(update);
    };

    publish(
        CalibrationStage::Starting,
        0.0,
        "正在连接捕获目标".to_string(),
    );

    let mut config = load_supported_ruler_config(&options.config_path)?;
    let mut backend = connect_capture_backend(&config)?;
    let tracker = ExternalWindowTracker::from_config(&config);
    let calibration_result = (|| -> Result<CalibrationOutcome, String> {
        let (width, height) = backend.dimensions();
        let ui_scaler = resolve_runtime_ui_scaler(&config);
        let roi = find_cost_bar_roi_with_ui_scaler(width as i32, height as i32, ui_scaler);
        let total_bar_width = roi.1 - roi.0;
        if total_bar_width <= 0 {
            return Err("capture ROI has invalid width".to_string());
        }

        if let Some(window_info) = capture_window_info(backend.as_ref(), tracker.as_ref()) {
            publish(
                CalibrationStage::Starting,
                0.0,
                format!(
                    "已连接窗口，分辨率 {}x{}，ui_scaler={ui_scaler:.3}，roi=({}, {}, {})",
                    window_info.width, window_info.height, roi.0, roi.1, roi.2
                ),
            );
        } else {
            publish(
                CalibrationStage::Starting,
                0.0,
                format!(
                    "已连接捕获后端，分辨率 {}x{}，ui_scaler={ui_scaler:.3}，roi=({}, {}, {})",
                    width, height, roi.0, roi.1, roi.2
                ),
            );
        }

        let capture_interval = config
            .screenshot_delay_ms
            .filter(|value| *value > 0.0)
            .map(|value| Duration::from_secs_f64(value / 1000.0));
        let cursor_size = read_pc_cursor_size()
            .ok()
            .flatten()
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        let mut cursor_guard = Some(SelfDrawnCursorGuard::new(ui_scaler, cursor_size));
        let mut cursor_blocked = false;
        let mut cursor_blocked_frames = 0u32;

        let mut cycle_samples: Vec<Vec<i32>> = Vec::new();
        let mut current_cycle_data: Vec<i32> = Vec::new();
        let mut previous_cost_state_raw: Option<i32> = None;
        let mut is_collecting_cycle = false;
        let mut progress = CalibrationProgress::new(total_bar_width);
        let mut frames_seen: u64 = 0;
        let mut none_streak: u64 = 0;
        let mut last_progress_bucket = -1i32;
        let mut screen_width = width;
        let mut screen_height = height;

        while cycle_samples.len() < CALIBRATION_CYCLES {
            if should_cancel() {
                publish(
                    CalibrationStage::Starting,
                    progress.last_percent,
                    "校准已取消".to_string(),
                );
                return Err(CALIBRATION_CANCELLED.to_string());
            }

            let loop_started = Instant::now();
            let frame = backend
                .capture_frame()
                .map_err(|e| format!("capture failed: {e}"))?;
            screen_width = frame.width;
            screen_height = frame.height;
            frames_seen = frames_seen.saturating_add(1);

            let battle_state = scanner::detect_battle_state_with_ui_scaler(
                &frame.data,
                frame.width,
                frame.height,
                frame.format,
                ui_scaler,
            );
            let window_info = capture_window_info(backend.as_ref(), tracker.as_ref());
            if cursor_guard
                .as_mut()
                .is_some_and(|guard| guard.should_pause_for_frame(window_info, roi, battle_state))
            {
                cursor_blocked_frames = cursor_blocked_frames.saturating_add(1);
                if !cursor_blocked || cursor_blocked_frames % 60 == 0 {
                    publish(
                        CalibrationStage::Sampling,
                        progress.last_percent,
                        "鼠标遮挡了费用条，校准已暂停，请将光标移开".to_string(),
                    );
                }
                cursor_blocked = true;
                if let Some(interval) = capture_interval {
                    let elapsed = loop_started.elapsed();
                    if interval > elapsed {
                        thread::sleep(interval - elapsed);
                    }
                }
                continue;
            }
            if cursor_blocked {
                cursor_blocked = false;
                cursor_blocked_frames = 0;
                publish(
                    CalibrationStage::Sampling,
                    progress.last_percent,
                    "费用条遮挡已解除，继续采样".to_string(),
                );
            }

            let current_cost_state_raw = scanner::get_raw_filled_pixel_width(
                &frame.data,
                frame.width,
                frame.height,
                frame.format,
                roi,
            );

            if let Some(current) = current_cost_state_raw {
                none_streak = 0;

                if let Some(previous) = previous_cost_state_raw {
                    if (previous as f64) > total_bar_width as f64 * 0.9
                        && (current as f64) < total_bar_width as f64 * 0.1
                    {
                        is_collecting_cycle = true;
                        if !current_cycle_data.is_empty() {
                            cycle_samples.push(std::mem::take(&mut current_cycle_data));
                        }
                    }
                }

                if is_collecting_cycle && cycle_samples.len() < CALIBRATION_CYCLES {
                    current_cycle_data.push(current);
                }

                let progress_percent =
                    progress.update(current, cycle_samples.len(), is_collecting_cycle);
                let bucket = progress_percent.floor() as i32;
                if frames_seen <= 3 || bucket != last_progress_bucket || bucket % 10 == 0 {
                    let message = if cycle_samples.is_empty() && !is_collecting_cycle {
                        format!(
                            "已检测到费用条，等待首个满到空切换后开始采样 ({progress_percent:.1}%)"
                        )
                    } else {
                        format!(
                            "正在采样费用条，第 {}/{} 圈 ({progress_percent:.1}%)",
                            cycle_samples.len().min(CALIBRATION_CYCLES - 1) + 1,
                            CALIBRATION_CYCLES
                        )
                    };
                    publish(CalibrationStage::Sampling, progress_percent, message);
                    last_progress_bucket = bucket;
                }

                previous_cost_state_raw = Some(current);
            } else {
                none_streak = none_streak.saturating_add(1);
                previous_cost_state_raw = None;
                if none_streak == 1 || none_streak == 60 || none_streak % 120 == 0 {
                    publish(
                        CalibrationStage::Starting,
                        progress.last_percent,
                        "未检测到费用条。请进入关卡并让费用条正常运行。".to_string(),
                    );
                }
            }

            if let Some(interval) = capture_interval {
                let elapsed = loop_started.elapsed();
                if interval > elapsed {
                    thread::sleep(interval - elapsed);
                }
            }
        }

        if should_cancel() {
            publish(CalibrationStage::Sampling, 100.0, "校准已取消".to_string());
            return Err(CALIBRATION_CANCELLED.to_string());
        }

        publish(
            CalibrationStage::Inferring,
            100.0,
            "采样完成，正在推断 calibration profile".to_string(),
        );

        let calibration_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("system clock error: {e}"))?
            .as_secs_f64();
        let bar_width_frac = cost_bar_width_frac_with_ui_scaler(
            screen_width as i32,
            screen_height as i32,
            ui_scaler,
        );
        let calibration_data = infer_calibration_from_samples_with_ui_scaler_and_total_bar_width(
            &cycle_samples,
            screen_width,
            screen_height,
            ui_scaler,
            total_bar_width,
            Some(bar_width_frac),
            calibration_time,
        )?;

        if should_cancel() {
            publish(CalibrationStage::Saving, 100.0, "校准已取消".to_string());
            return Err(CALIBRATION_CANCELLED.to_string());
        }

        publish(
            CalibrationStage::Saving,
            100.0,
            "正在保存 calibration profile".to_string(),
        );

        let basename = format!("profile_{}", calibration_time.trunc() as u64);
        let (profile_filename, profile_path) =
            write_calibration_profile(&options.config_path, &calibration_data, &basename)?;
        config.active_calibration_profile = Some(profile_filename.clone());
        config.save_to_path(&options.config_path).map_err(|e| {
            format!(
                "failed to update config '{}': {e}",
                options.config_path.display()
            )
        })?;

        publish(CalibrationStage::Saving, 100.0, "校准完成".to_string());

        Ok(CalibrationOutcome {
            session_dir: session_dir.clone(),
            profile_path,
            profile_filename,
        })
    })();
    backend.disconnect();
    calibration_result
}

pub fn format_time_from_frames(total_frames: i32) -> String {
    if total_frames < 0 {
        return "00:00:00".to_string();
    }
    let frames = total_frames % FRAMES_PER_SECOND;
    let total_seconds = total_frames / FRAMES_PER_SECOND;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02}:{frames:02}")
}

fn format_time_slug_from_frames(total_frames: i32) -> String {
    format_time_from_frames(total_frames).replace(':', "-")
}

fn format_clock_event(event: &ClockEvent) -> String {
    format!(
        "[clock] {} state={}",
        format_time_from_frames(event.elapsed_frames),
        event.battle_state.as_str()
    )
}

fn format_tile_debug(tile: TileMatch) -> String {
    format!(
        "tile=({}, {}) tile_key={} buildable_type={} tile_screen=({}, {}) dist_px={:.1}",
        tile.x,
        tile.y,
        tile.tile_key,
        tile.buildable_type,
        tile.screen.x,
        tile.screen.y,
        tile.distance_px
    )
}

fn format_tile_brief(tile: TileMatch) -> String {
    format!("tile=({}, {})", tile.x, tile.y)
}

fn format_detail_capture_suffix(
    event: &OperationEvent,
    detail_capture: &Option<DetailCapture>,
) -> String {
    detail_capture
        .as_ref()
        .map(|capture| {
            format!(
                " detail_roi='{}' detail_size={}x{} detail_age_frames={}",
                capture.relative_path,
                capture.width,
                capture.height,
                event.elapsed_frames - capture.captured_elapsed_frames
            )
        })
        .unwrap_or_default()
}

fn format_detail_name_suffix(detail_name: &Option<String>) -> String {
    detail_name
        .as_ref()
        .map(|name| format!(" detail_name='{}'", name.replace('\'', "\\'")))
        .unwrap_or_default()
}

fn format_operation_event(event: &OperationEvent) -> String {
    let time = format_time_from_frames(event.elapsed_frames);
    let logical_frame = event
        .logical_frame
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    match &event.kind {
        OperationKind::LeftClick {
            client,
            selected_tile,
        } => {
            let tile_suffix = selected_tile
                .map(|tile| format!(" {}", format_tile_debug(tile)))
                .unwrap_or_default();
            format!(
                "[op] {} elapsed={} logical={}/{} state={} click client=({}, {}){}",
                time,
                event.elapsed_frames,
                logical_frame,
                event.total_frames_in_cycle,
                event.battle_state.as_str(),
                client.x,
                client.y,
                tile_suffix
            )
        }
        OperationKind::Drag {
            start_client,
            end_client,
            start_frame,
            start_state,
            deploy_like,
            deploy_end_tile,
        } => {
            let tile_suffix = deploy_end_tile
                .map(|tile| format!(" {}", format_tile_debug(tile)))
                .unwrap_or_default();
            format!(
                "[op] {} elapsed={} logical={}/{} state={} drag start=({}, {}) end=({}, {}) start_frame={} start_state={} deploy_like={}{}",
                time,
                event.elapsed_frames,
                logical_frame,
                event.total_frames_in_cycle,
                event.battle_state.as_str(),
                start_client.x,
                start_client.y,
                end_client.x,
                end_client.y,
                start_frame,
                start_state.as_str(),
                if *deploy_like { "yes" } else { "no" },
                tile_suffix
            )
        }
        OperationKind::DeployResolved {
            tile,
            end_client,
            detail_capture,
            detail_name,
        } => {
            let detail_suffix = format_detail_capture_suffix(event, detail_capture);
            let name_suffix = format_detail_name_suffix(detail_name);
            format!(
                "[op] {} elapsed={} logical={}/{} state={} deploy_resolved {} end=({}, {}){}{}",
                time,
                event.elapsed_frames,
                logical_frame,
                event.total_frames_in_cycle,
                event.battle_state.as_str(),
                format_tile_debug(*tile),
                end_client.x,
                end_client.y,
                name_suffix,
                detail_suffix
            )
        }
        OperationKind::SkillHotkey {
            vk,
            selected_tile,
            detail_capture,
            detail_name,
        } => {
            let tile_suffix = selected_tile
                .map(|tile| format!(" {}", format_tile_debug(tile)))
                .unwrap_or_default();
            let detail_suffix = format_detail_capture_suffix(event, detail_capture);
            let name_suffix = format_detail_name_suffix(detail_name);
            format!(
                "[op] {} elapsed={} logical={}/{} state={} skill_hotkey vk=0x{:02X}{}{}{}",
                time,
                event.elapsed_frames,
                logical_frame,
                event.total_frames_in_cycle,
                event.battle_state.as_str(),
                vk,
                tile_suffix,
                name_suffix,
                detail_suffix
            )
        }
        OperationKind::RetreatHotkey {
            vk,
            selected_tile,
            detail_capture,
            detail_name,
        } => {
            let tile_suffix = selected_tile
                .map(|tile| format!(" {}", format_tile_debug(tile)))
                .unwrap_or_default();
            let detail_suffix = format_detail_capture_suffix(event, detail_capture);
            let name_suffix = format_detail_name_suffix(detail_name);
            format!(
                "[op] {} elapsed={} logical={}/{} state={} retreat_hotkey vk=0x{:02X}{}{}{}",
                time,
                event.elapsed_frames,
                logical_frame,
                event.total_frames_in_cycle,
                event.battle_state.as_str(),
                vk,
                tile_suffix,
                name_suffix,
                detail_suffix
            )
        }
    }
}

fn format_user_operation(event: &OperationEvent) -> Option<String> {
    let time = format_time_from_frames(event.elapsed_frames);
    match &event.kind {
        OperationKind::DeployResolved {
            tile,
            end_client,
            detail_name,
            ..
        } => Some(format!(
            "[deploy] {}{} tile=({}, {}) end=({}, {})",
            time,
            detail_name
                .as_ref()
                .map(|name| format!(" name={name}"))
                .unwrap_or_default(),
            tile.x,
            tile.y,
            end_client.x,
            end_client.y
        )),
        OperationKind::SkillHotkey {
            vk,
            selected_tile,
            detail_name,
            ..
        } => Some(format!(
            "[skill] {}{} key={}{}",
            time,
            detail_name
                .as_ref()
                .map(|name| format!(" name={name}"))
                .unwrap_or_default(),
            format_virtual_key(*vk),
            selected_tile
                .map(|tile| format!(" {}", format_tile_brief(tile)))
                .unwrap_or_default()
        )),
        OperationKind::RetreatHotkey {
            vk,
            selected_tile,
            detail_name,
            ..
        } => Some(format!(
            "[retreat] {}{} key={}{}",
            time,
            detail_name
                .as_ref()
                .map(|name| format!(" name={name}"))
                .unwrap_or_default(),
            format_virtual_key(*vk),
            selected_tile
                .map(|tile| format!(" {}", format_tile_brief(tile)))
                .unwrap_or_default()
        )),
        _ => None,
    }
}

fn format_raw_input_event(event: &RawInputEvent) -> String {
    let time = format_time_from_frames(event.elapsed_frames);
    let logical_frame = event
        .logical_frame
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());

    match event.kind {
        RawInputKind::Key {
            action,
            vk,
            scan_code,
            extended,
            injected,
            foreground_match,
            skill_match,
            retreat_match,
            target_hwnd,
            target_root_hwnd,
            foreground_hwnd,
            foreground_root_hwnd,
        } => format!(
            "[raw-key] {} elapsed={} logical={}/{} state={} action={} vk=0x{:02X} scan=0x{:02X} extended={} injected={} foreground_match={} skill_match={} retreat_match={} target_hwnd=0x{:X} target_root=0x{:X} foreground_hwnd=0x{:X} foreground_root=0x{:X}",
            time,
            event.elapsed_frames,
            logical_frame,
            event.total_frames_in_cycle,
            event.battle_state.as_str(),
            action.as_str(),
            vk,
            scan_code,
            yes_no(extended),
            yes_no(injected),
            yes_no(foreground_match),
            yes_no(skill_match),
            yes_no(retreat_match),
            target_hwnd,
            target_root_hwnd,
            foreground_hwnd,
            foreground_root_hwnd
        ),
        RawInputKind::MouseLeft {
            action,
            screen,
            client,
            point_root_hwnd,
            foreground_match,
            target_hwnd,
            foreground_hwnd,
            foreground_root_hwnd,
        } => {
            let client_text = client
                .map(|point| format!("({}, {})", point.x, point.y))
                .unwrap_or_else(|| "none".to_string());
            format!(
                "[raw-mouse] {} elapsed={} logical={}/{} state={} button=left action={} screen=({}, {}) client={} point_root=0x{:X} foreground_match={} target_hwnd=0x{:X} foreground_hwnd=0x{:X} foreground_root=0x{:X}",
                time,
                event.elapsed_frames,
                logical_frame,
                event.total_frames_in_cycle,
                event.battle_state.as_str(),
                action.as_str(),
                screen.x,
                screen.y,
                client_text,
                point_root_hwnd,
                yes_no(foreground_match),
                target_hwnd,
                foreground_hwnd,
                foreground_root_hwnd
            )
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn format_virtual_key(vk: u32) -> String {
    match vk {
        0x20 => "SPACE".to_string(),
        0x09 => "TAB".to_string(),
        0x1B => "ESC".to_string(),
        0x0D => "ENTER".to_string(),
        0x10 => "SHIFT".to_string(),
        0x11 => "CTRL".to_string(),
        0x12 => "ALT".to_string(),
        _ if ascii_digit_value(vk).is_some() || ((b'A' as u32)..=(b'Z' as u32)).contains(&vk) => {
            char::from_u32(vk).unwrap_or('?').to_string()
        }
        0x60..=0x69 => format!("NUMPAD{}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        _ => format!("0x{vk:02X}"),
    }
}

fn write_event_log(logger: &mut EventLogger, event: &AppEvent) {
    let line = match event {
        AppEvent::Clock(event) => Some(format_clock_event(event)),
        AppEvent::Operation(event) => Some(format_operation_event(event)),
        AppEvent::Info(message) => Some(format!("[info] {message}")),
        AppEvent::RawInput(event) => Some(format_raw_input_event(event)),
        AppEvent::Warning(message) => Some(format!("[warn] {message}")),
    };

    if let Some(line) = line {
        logger.write_line(&line);
    }
}

fn print_visible_event(event: &AppEvent) {
    match event {
        AppEvent::Operation(event) => {
            if let Some(line) = format_user_operation(&event) {
                println!("{line}");
            }
        }
        AppEvent::Clock(_) | AppEvent::Info(_) | AppEvent::RawInput(_) | AppEvent::Warning(_) => {}
    }
}

fn emit_pumped_event(
    logger: &mut EventLogger,
    tx: &Sender<AppEvent>,
    visible_console: bool,
    event: AppEvent,
) {
    write_event_log(logger, &event);
    if visible_console {
        print_visible_event(&event);
    }
    let _ = tx.send(event);
}

fn enqueue_or_emit_event(
    logger: &mut EventLogger,
    tx: &Sender<AppEvent>,
    ocr_tx: &Sender<OperationEvent>,
    pending_ocr: &Arc<AtomicUsize>,
    visible_console: bool,
    event: AppEvent,
) {
    match event {
        AppEvent::Operation(operation) if operation.needs_operator_name_resolution() => {
            pending_ocr.fetch_add(1, Ordering::SeqCst);
            if let Err(error) = ocr_tx.send(operation) {
                pending_ocr.fetch_sub(1, Ordering::SeqCst);
                emit_pumped_event(logger, tx, visible_console, AppEvent::Operation(error.0));
            }
        }
        other => emit_pumped_event(logger, tx, visible_console, other),
    }
}

fn run_operation_ocr_worker(rx: Receiver<OperationEvent>, tx: Sender<OperationEvent>) {
    let recognizer = OperatorNameRecognizer::new();
    while let Ok(mut operation) = rx.recv() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recognizer.enrich_operation(&mut operation);
        }));
        if result.is_err() {
            let payload = result.err().expect("panic payload must exist");
            log::warn!(
                "operator OCR worker panicked for one event; forwarding unresolved operation: {}",
                describe_panic_payload(&payload)
            );
        }
        if tx.send(operation).is_err() {
            break;
        }
    }
}

fn pump_events(
    rx: Receiver<AppEvent>,
    tx: Sender<AppEvent>,
    stop: Arc<AtomicBool>,
    pending_ocr: Arc<AtomicUsize>,
    mut logger: EventLogger,
    visible_console: bool,
) {
    let (ocr_in_tx, ocr_in_rx) = unbounded::<OperationEvent>();
    let (ocr_out_tx, ocr_out_rx) = unbounded::<OperationEvent>();
    let ocr_worker = thread::spawn(move || {
        run_operation_ocr_worker(ocr_in_rx, ocr_out_tx);
    });

    let mut raw_disconnected = false;
    while !stop.load(Ordering::SeqCst) && !raw_disconnected {
        crossbeam_channel::select! {
            recv(rx) -> message => match message {
                Ok(event) => enqueue_or_emit_event(
                    &mut logger,
                    &tx,
                    &ocr_in_tx,
                    &pending_ocr,
                    visible_console,
                    event,
                ),
                Err(_) => raw_disconnected = true,
            },
            recv(ocr_out_rx) -> message => match message {
                Ok(operation) => {
                    emit_pumped_event(
                        &mut logger,
                        &tx,
                        visible_console,
                        AppEvent::Operation(operation),
                    );
                    pending_ocr.fetch_sub(1, Ordering::SeqCst);
                }
                Err(_) => break,
            },
            default(Duration::from_millis(100)) => {}
        }
    }

    while let Ok(event) = rx.try_recv() {
        enqueue_or_emit_event(
            &mut logger,
            &tx,
            &ocr_in_tx,
            &pending_ocr,
            visible_console,
            event,
        );
    }

    drop(ocr_in_tx);

    while let Ok(operation) = ocr_out_rx.recv_timeout(Duration::from_millis(100)) {
        emit_pumped_event(
            &mut logger,
            &tx,
            visible_console,
            AppEvent::Operation(operation),
        );
        pending_ocr.fetch_sub(1, Ordering::SeqCst);
    }

    let _ = ocr_worker.join();
}

pub fn enable_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn IsUserAnAdmin() -> BOOL;
    fn ShellExecuteW(
        hwnd: HWND,
        lpoperation: *const u16,
        lpfile: *const u16,
        lpparameters: *const u16,
        lpdirectory: *const u16,
        nshowcmd: i32,
    ) -> HINSTANCE;
    fn GetCursorInfo(cursor_info: *mut NativeCursorInfo) -> BOOL;
    fn GetCursorPos(point: *mut POINT) -> BOOL;
    fn ClientToScreen(hwnd: HWND, point: *mut POINT) -> BOOL;
    fn ScreenToClient(hwnd: HWND, point: *mut POINT) -> BOOL;
}

pub fn relaunch_as_admin_if_needed() -> Result<bool, String> {
    unsafe {
        if IsUserAnAdmin().as_bool() {
            return Ok(false);
        }
    }

    let exe = std::env::current_exe()
        .map_err(|e| format!("failed to resolve current exe for elevation: {e}"))?;
    let exe_wide = wide_os(exe.as_os_str());
    let args = std::env::args_os()
        .skip(1)
        .map(|arg| quote_arg(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let args_wide = wide(&args);
    let runas = wide("runas");

    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            runas.as_ptr(),
            exe_wide.as_ptr(),
            if args.is_empty() {
                std::ptr::null()
            } else {
                args_wide.as_ptr()
            },
            std::ptr::null(),
            1,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        Ok(true)
    } else {
        Err(format!(
            "failed to relaunch as administrator: ShellExecuteW returned {code}"
        ))
    }
}

fn quote_arg(value: &str) -> String {
    if value.is_empty() || value.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(value: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    value.encode_wide().chain(std::iter::once(0)).collect()
}

static SYSTEM_LOGGER_READY: OnceLock<()> = OnceLock::new();

fn init_system_logger(path: &Path) -> Result<(), String> {
    if SYSTEM_LOGGER_READY.get().is_some() {
        return Ok(());
    }
    let file = File::create(path)
        .map_err(|e| format!("failed to create system log '{}': {e}", path.display()))?;
    let writer = LineWriter::new(file);
    let mut builder = env_logger::Builder::from_default_env();
    if std::env::var_os("RUST_LOG").is_none() {
        builder.filter_level(log::LevelFilter::Info);
    }
    builder.format_timestamp_secs();
    builder.write_style(env_logger::WriteStyle::Never);
    builder.target(env_logger::Target::Pipe(Box::new(writer)));
    builder
        .try_init()
        .map_err(|e| format!("failed to initialize system logger: {e}"))?;
    let _ = SYSTEM_LOGGER_READY.set(());
    Ok(())
}

fn session_header_line(options: &RecorderOptions, log_paths: &RuntimeLogPaths) -> String {
    format!(
        "[session] started_at_unix={} config='{}' skill_key={} retreat_key={} logs_dir='{}' system_log='{}' event_log='{}' capture_dir='{}'",
        unix_timestamp_seconds(),
        options.config_path.display(),
        options
            .skill_vk
            .map(|vk| format!("0x{vk:02X}"))
            .unwrap_or_else(|| "unset".to_string()),
        options
            .retreat_vk
            .map(|vk| format!("0x{vk:02X}"))
            .unwrap_or_else(|| "unset".to_string())
        ,
        log_paths.session_dir.display(),
        log_paths.system_log_path.display(),
        log_paths.event_log_path.display(),
        log_paths.capture_dir.display()
    )
}

pub struct RecorderLaunch {
    pub receiver: Receiver<AppEvent>,
    pub handle: RecorderHandle,
    pub log_paths: RuntimeLogPaths,
}

#[derive(Clone, Copy, Debug)]
pub struct RecorderClockSnapshot {
    pub elapsed_frames: i32,
    pub logical_frame: Option<i32>,
    pub battle_state: BattleStateCode,
    pub capture_state: CaptureStateCode,
}

#[derive(Clone, Debug)]
pub struct RecorderDetailPreview {
    pub elapsed_frames: i32,
    pub width: u32,
    pub height: u32,
    pub data_bgr: Vec<u8>,
}

pub struct RecorderHandle {
    stop: Arc<AtomicBool>,
    hook_thread_id: Arc<AtomicU32>,
    shared_clock: Arc<SharedClockState>,
    pending_ocr: Arc<AtomicUsize>,
    hook_thread: Option<HookThread>,
    clock_handle: Option<JoinHandle<Result<(), String>>>,
    pump_handle: Option<JoinHandle<()>>,
}

impl RecorderHandle {
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    pub fn hook_thread_id(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.hook_thread_id)
    }

    pub fn clock_snapshot(&self) -> RecorderClockSnapshot {
        let snapshot = self.shared_clock.snapshot();
        RecorderClockSnapshot {
            elapsed_frames: snapshot.elapsed_frames,
            logical_frame: snapshot.logical_frame,
            battle_state: snapshot.battle_state,
            capture_state: snapshot.capture_state,
        }
    }

    pub fn pending_ocr_count(&self) -> usize {
        self.pending_ocr.load(Ordering::SeqCst)
    }

    pub fn detail_name_preview(&self, roi: RelativeRectF32) -> Option<RecorderDetailPreview> {
        let crop = self.shared_clock.preview_crop(roi)?;
        Some(RecorderDetailPreview {
            elapsed_frames: crop.elapsed_frames,
            width: crop.width,
            height: crop.height,
            data_bgr: crop.data_bgr,
        })
    }

    pub fn shutdown(mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(hook_thread) = self.hook_thread.take() {
            hook_thread.shutdown();
            hook_thread.join();
        }
        if let Some(clock_handle) = self.clock_handle.take() {
            match clock_handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err("clock thread panicked".to_string()),
            }
        }
        if let Some(pump_handle) = self.pump_handle.take() {
            let _ = pump_handle.join();
        }
        Ok(())
    }
}

pub fn start_recorder(options: RecorderOptions) -> Result<RecorderLaunch, String> {
    let log_paths = RuntimeLogPaths::create(&options.logs_root)?;
    init_system_logger(&log_paths.system_log_path)?;

    let mut event_logger = EventLogger::create(log_paths.event_log_path.clone())?;
    event_logger.write_line(&session_header_line(&options, &log_paths));

    let stop = Arc::new(AtomicBool::new(false));
    let hook_thread_id = Arc::new(AtomicU32::new(0));
    let shared_clock = Arc::new(SharedClockState::new(options.detail_name_roi));
    let pending_ocr = Arc::new(AtomicUsize::new(0));
    let (raw_tx, raw_rx) = unbounded::<AppEvent>();
    let (user_tx, user_rx) = unbounded::<AppEvent>();

    let clock_handle = spawn_clock_worker(
        options.clone(),
        raw_tx.clone(),
        Arc::clone(&shared_clock),
        Arc::clone(&stop),
    )?;

    let hook_thread = match HookThread::spawn(
        raw_tx,
        Arc::clone(&shared_clock),
        options.skill_vk,
        options.retreat_vk,
        log_paths.capture_dir.clone(),
    ) {
        Ok(thread) => thread,
        Err(error) => {
            stop.store(true, Ordering::SeqCst);
            let _ = clock_handle.join();
            return Err(error);
        }
    };
    hook_thread_id.store(hook_thread.thread_id, Ordering::SeqCst);

    let pump_stop = Arc::clone(&stop);
    let pump_pending_ocr = Arc::clone(&pending_ocr);
    let visible_console = options.visible_console;
    let pump_handle = thread::spawn(move || {
        pump_events(
            raw_rx,
            user_tx,
            pump_stop,
            pump_pending_ocr,
            event_logger,
            visible_console,
        );
    });

    Ok(RecorderLaunch {
        receiver: user_rx,
        handle: RecorderHandle {
            stop,
            hook_thread_id,
            shared_clock: Arc::clone(&shared_clock),
            pending_ocr,
            hook_thread: Some(hook_thread),
            clock_handle: Some(clock_handle),
            pump_handle: Some(pump_handle),
        },
        log_paths,
    })
}

pub fn run_cli() -> Result<(), String> {
    enable_dpi_awareness();
    if relaunch_as_admin_if_needed()? {
        return Ok(());
    }

    let options = RecorderOptions::parse()?;
    let launch = start_recorder(options)?;
    install_ctrlc(launch.handle.stop_flag(), launch.handle.hook_thread_id())?;
    let RecorderLaunch {
        receiver, handle, ..
    } = launch;

    while receiver.recv().is_ok() {}

    handle.shutdown()
}

fn unix_timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
