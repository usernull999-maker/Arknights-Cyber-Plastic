//! Application-owned compatibility boundary for ArknightsCostBarRuler.
//!
//! New code must use the stable types in this module. `legacy` exists solely
//! while the recorder is being migrated from the former direct integration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureTarget {
    pub fingerprint: String,
    pub display_name: String,
    pub kind: String,
}

/// Target data collected by the application platform layer. It intentionally
/// contains no CostBarRuler configuration types.
#[derive(Clone, Debug)]
pub enum TargetConfiguration {
    Window {
        fingerprint: String,
        window_handle: Option<isize>,
        window_title: Option<String>,
        window_class: Option<String>,
    },
    Mumu {
        fingerprint: String,
        install_path: PathBuf,
        instance_index: u32,
    },
}

#[derive(Clone, Debug)]
pub struct ProfileInspection {
    pub capture_type: String,
    pub target_fingerprint: Option<String>,
    pub calibration_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RulerProfile {
    pub schema_version: u32,
    pub id: String,
    pub display_name: String,
    pub config_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub client_left: i32,
    pub client_top: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalibrationStatus {
    pub profile_path: Option<PathBuf>,
    pub compatible: bool,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct FrameImage {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PixelFormat {
    Bgr,
    Bgra,
    Rgba,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BattleState {
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

#[derive(Clone, Debug)]
pub struct FrameObservation {
    pub image: FrameImage,
    pub window: Option<WindowGeometry>,
    pub elapsed_frames: i32,
    pub logical_frame: Option<i32>,
    pub total_frames_in_cycle: i32,
    pub battle_state: BattleState,
}

#[derive(Clone, Debug)]
pub struct RulerError(pub String);

impl std::fmt::Display for RulerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RulerError {}

pub fn inspect_profile(config_path: &std::path::Path) -> Result<ProfileInspection, RulerError> {
    let config = legacy::RulerConfig::load_from_path(config_path)
        .map_err(|error| RulerError(error.to_string()))?;
    let calibration_path = config.active_calibration_profile.as_deref().map(|profile| {
        config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("calibration")
            .join(profile)
    });
    Ok(ProfileInspection {
        capture_type: config.capture_type,
        target_fingerprint: config.target_fingerprint,
        calibration_path,
    })
}

pub fn save_target_configuration(
    config_path: &std::path::Path,
    target: &TargetConfiguration,
) -> Result<(), RulerError> {
    let previous = legacy::RulerConfig::load_from_path(config_path).ok();
    let fingerprint = match target {
        TargetConfiguration::Window { fingerprint, .. }
        | TargetConfiguration::Mumu { fingerprint, .. } => fingerprint,
    };
    let keep_profile = previous
        .as_ref()
        .and_then(|config| config.target_fingerprint.as_deref())
        .is_some_and(|existing| existing == fingerprint);
    let config = match target {
        TargetConfiguration::Window {
            fingerprint,
            window_handle,
            window_title,
            window_class,
        } => legacy::RulerConfig {
            uuid: previous.as_ref().and_then(|config| config.uuid.clone()),
            telemetry_enabled: previous
                .as_ref()
                .and_then(|config| config.telemetry_enabled),
            screenshot_delay_ms: previous
                .as_ref()
                .and_then(|config| config.screenshot_delay_ms),
            capture_type: "window".to_string(),
            install_path: None,
            instance_index: None,
            device_id: None,
            window_handle: *window_handle,
            window_title: window_title.clone(),
            window_class: window_class.clone(),
            active_calibration_profile: keep_profile
                .then(|| {
                    previous
                        .as_ref()
                        .and_then(|config| config.active_calibration_profile.clone())
                })
                .flatten(),
            frame_display_mode: previous
                .as_ref()
                .and_then(|config| config.frame_display_mode.clone())
                .or_else(|| Some("0_to_n-1".to_string())),
            language: previous
                .as_ref()
                .and_then(|config| config.language.clone())
                .or_else(|| Some("zh_CN".to_string())),
            auto_select_target: false,
            target_fingerprint: Some(fingerprint.clone()),
            overlay_pos_x: previous.as_ref().and_then(|config| config.overlay_pos_x),
            overlay_pos_y: previous.as_ref().and_then(|config| config.overlay_pos_y),
            overlay_scale: previous.as_ref().and_then(|config| config.overlay_scale),
            ui_scaler: previous.as_ref().and_then(|config| config.ui_scaler),
            debug_recording_enabled: previous
                .as_ref()
                .map(|config| config.debug_recording_enabled)
                .unwrap_or(false),
            debug_recording_video: previous
                .as_ref()
                .map(|config| config.debug_recording_video)
                .unwrap_or(false),
            debug_recording_csv: previous
                .as_ref()
                .map(|config| config.debug_recording_csv)
                .unwrap_or(false),
            trace_logging_enabled: previous
                .as_ref()
                .map(|config| config.trace_logging_enabled)
                .unwrap_or(false),
            log_output_dir: previous
                .as_ref()
                .and_then(|config| config.log_output_dir.clone()),
            replay_video_path: previous
                .as_ref()
                .and_then(|config| config.replay_video_path.clone()),
            replay_fps: previous.as_ref().and_then(|config| config.replay_fps),
        },
        TargetConfiguration::Mumu {
            fingerprint,
            install_path,
            instance_index,
        } => legacy::RulerConfig {
            uuid: previous.as_ref().and_then(|config| config.uuid.clone()),
            telemetry_enabled: previous
                .as_ref()
                .and_then(|config| config.telemetry_enabled),
            screenshot_delay_ms: previous
                .as_ref()
                .and_then(|config| config.screenshot_delay_ms),
            capture_type: "mumu".to_string(),
            install_path: Some(install_path.to_string_lossy().to_string()),
            instance_index: Some(*instance_index),
            device_id: keep_profile
                .then(|| {
                    previous
                        .as_ref()
                        .and_then(|config| config.device_id.clone())
                })
                .flatten(),
            window_handle: None,
            window_title: None,
            window_class: None,
            active_calibration_profile: keep_profile
                .then(|| {
                    previous
                        .as_ref()
                        .and_then(|config| config.active_calibration_profile.clone())
                })
                .flatten(),
            frame_display_mode: previous
                .as_ref()
                .and_then(|config| config.frame_display_mode.clone())
                .or_else(|| Some("0_to_n-1".to_string())),
            language: previous
                .as_ref()
                .and_then(|config| config.language.clone())
                .or_else(|| Some("zh_CN".to_string())),
            auto_select_target: false,
            target_fingerprint: Some(fingerprint.clone()),
            overlay_pos_x: previous.as_ref().and_then(|config| config.overlay_pos_x),
            overlay_pos_y: previous.as_ref().and_then(|config| config.overlay_pos_y),
            overlay_scale: previous.as_ref().and_then(|config| config.overlay_scale),
            ui_scaler: previous.as_ref().and_then(|config| config.ui_scaler),
            debug_recording_enabled: previous
                .as_ref()
                .map(|config| config.debug_recording_enabled)
                .unwrap_or(false),
            debug_recording_video: previous
                .as_ref()
                .map(|config| config.debug_recording_video)
                .unwrap_or(false),
            debug_recording_csv: previous
                .as_ref()
                .map(|config| config.debug_recording_csv)
                .unwrap_or(false),
            trace_logging_enabled: previous
                .as_ref()
                .map(|config| config.trace_logging_enabled)
                .unwrap_or(false),
            log_output_dir: previous
                .as_ref()
                .and_then(|config| config.log_output_dir.clone()),
            replay_video_path: previous
                .as_ref()
                .and_then(|config| config.replay_video_path.clone()),
            replay_fps: previous.as_ref().and_then(|config| config.replay_fps),
        },
    };
    config
        .save_to_path(config_path)
        .map_err(|error| RulerError(error.to_string()))
}

/// Temporary adapter used by the legacy recorder. Do not expose it through a
/// UI or application-service API; replacement work belongs in this crate.
pub mod legacy {
    pub use ruler_core::analysis;
    pub use ruler_core::capture;
    pub use ruler_core::{Analyzer, BattleState, PixelFormat, RulerConfig};
}
