//! Stable, serializable boundary between the desktop backend and the WebView.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppBootstrap<T> {
    pub protocol_version: u32,
    pub revision: u64,
    pub session_id: Option<String>,
    pub state: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineUpdate {
    pub session_id: String,
    pub sequence: u64,
    pub observed_at_ns: u64,
    pub elapsed_frames: i32,
    pub logical_frame: Option<i32>,
    pub battle_state: String,
    pub capture_state: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusChanged<T> {
    pub revision: u64,
    pub session_id: Option<String>,
    pub state: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandReply<T> {
    pub revision: u64,
    pub value: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum UiEvent<T, O> {
    Timeline(TimelineUpdate),
    Status(StatusChanged<T>),
    Operation(O),
}
