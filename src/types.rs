use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartArgs {
    /// Recreate the capture/input session while retaining saved permission. Use after a stream error or display layout change.
    #[serde(default)]
    pub restart: bool,
    /// Forget the saved permission token and let KDE ask again.
    #[serde(default)]
    pub fresh_permission: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObserveArgs {
    /// Index from computer_start/status. Defaults to the first selected display.
    #[serde(default)]
    pub display: usize,
    /// Optional region in the original capture's physical pixels, before resizing.
    pub crop: Option<Crop>,
    /// Longest returned image edge. Default 1600; 0 means original size.
    pub max_dimension: Option<u32>,
    /// Wait for a sequence newer than this one on the selected current-session stream.
    pub after_sequence: Option<u64>,
    /// One capture wait budget, 0–5000 ms. Default 700 with after_sequence, otherwise 5000.
    /// Zero checks immediately. On timeout, returns cached pixels with freshness_met=false.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
    Back,
    Forward,
}
impl Button {
    pub fn code(self) -> i32 {
        match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
            Self::Back => 0x116,
            Self::Forward => 0x115,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputAction {
    Move {
        x: f64,
        y: f64,
    },
    /// Relative motion for captured-pointer games. Not an absolute-coordinate substitute.
    MoveRelative {
        dx: f64,
        dy: f64,
    },
    /// Hold inputs for a bounded gesture, optionally moving the captured pointer meanwhile.
    /// All held inputs are released when finished, cancelled, or stopped.
    Hold {
        #[serde(default)]
        keys: Vec<String>,
        /// Linux evdev codes (e.g. 17=W), independent of keyboard layout.
        #[serde(default)]
        keycodes: Vec<u16>,
        button: Option<Button>,
        /// 1–30000 milliseconds.
        duration_ms: u64,
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
    },
    Click {
        x: f64,
        y: f64,
        #[serde(default)]
        button: Button,
        /// 1 (default) or 2 for a double click.
        #[serde(default = "one")]
        count: u8,
    },
    Scroll {
        x: f64,
        y: f64,
        /// Positive moves right, in logical desktop units.
        #[serde(default)]
        dx: f64,
        /// Positive moves down, in logical desktop units.
        dy: f64,
    },
    Keypress {
        /// A chord, for example ["CTRL", "L"] or ["ENTER"].
        #[serde(default)]
        keys: Vec<String>,
        /// Physical Linux evdev chord, e.g. [29, 38] for CTRL+L on US positions.
        /// Supply either keys or keycodes, not both.
        #[serde(default)]
        keycodes: Vec<u16>,
    },
    Type {
        /// Optional field to click immediately before pasting, in the referenced image.
        at: Option<Point>,
        /// UTF-8 text pasted into the focused app. Replaces the system clipboard.
        text: String,
        /// Paste chord. Defaults to CTRL+V; terminals often need CTRL+SHIFT+V.
        paste_keys: Option<Vec<String>>,
        /// Physical paste chord, e.g. [29, 47]. Mutually exclusive with paste_keys.
        paste_keycodes: Option<Vec<u16>>,
    },
    Drag {
        /// At least two points in the referenced screenshot's coordinate space.
        points: Vec<Point>,
        #[serde(default)]
        button: Button,
        /// Total gesture duration, 50–10000 ms. Default 500.
        duration_ms: Option<u64>,
        /// Optional held modifiers, for example SHIFT for constrained dragging.
        #[serde(default)]
        keys: Vec<String>,
    },
}
fn one() -> u8 {
    1
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActArgs {
    /// The latest observation's frame_id, including for keyboard input. Never guess coordinates.
    pub frame_id: String,
    pub action: InputAction,
    /// Return another observation after the action. Default true.
    #[serde(default = "yes")]
    pub observe: bool,
    /// Delay before observing, 0–2000 ms. Default 150. Not proof the UI finished.
    pub settle_ms: Option<u64>,
    /// Fresh capture wait after settle, 0–5000 ms; default 700. Zero checks without waiting.
    pub feedback_timeout_ms: Option<u64>,
    /// Optional pre-gesture focus check against a node from the latest inspection.
    /// Mismatch/unknown sends nothing. Does not reserve focus; omit for visual-only targeting.
    pub expected_focus_node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub index: usize,
    pub node_id: u32,
    pub logical_size: Option<(u32, u32)>,
    pub position: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationInfo {
    pub frame_id: String,
    pub display: usize,
    pub captured_at_ms: u64,
    pub age_ms: u64,
    pub capture_sequence: u64,
    pub image_width: u32,
    pub image_height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub crop: Crop,
    pub logical_size: Option<(u32, u32)>,
    /// A capture newer than the one immediately after input submission was received.
    /// False can mean the desktop is idle; it does not establish UI success/failure.
    pub new_frame_after_input: Option<bool>,
    /// Whether the requested sequence barrier was met; null when none was requested.
    pub freshness_met: Option<bool>,
    pub requested_after_sequence: Option<u64>,
    pub wait_timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub info: ObservationInfo,
    #[serde(default, skip_serializing)]
    pub png: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputStatus {
    NotSent,
    Sent,
    PossiblyPartial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReport {
    pub input_status: InputStatus,
    pub outcome_verified: bool,
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup_errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_error: Option<String>,
    pub diagnostics: ActionDiagnostics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionDiagnostics {
    pub observation_age_ms: Option<u64>,
    pub referenced_capture_age_ms: Option<u64>,
    /// A sample advanced, not evidence that focus changed.
    pub capture_advanced_before_input: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus_check: Option<FocusCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guidance: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusState {
    Confirmed,
    Mismatch,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusCheck {
    pub state: FocusState,
    pub reason: String,
}
impl FocusCheck {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            state: FocusState::Unknown,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub active: bool,
    pub displays: Vec<DisplayInfo>,
    pub clipboard_available: bool,
    pub permission_saved: bool,
    /// Capture metadata in display-index order; empty before session start.
    pub captures: Vec<crate::capture::CaptureDiagnostics>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectArgs {
    /// Case-insensitive application-name filter. Without a filter, only app/window metadata is read.
    pub application: Option<String>,
    /// Maximum returned nodes, default 100, capped at 500. Incomplete results are labeled truncated.
    pub max_nodes: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FocusArgs {
    /// Exact node ID from the latest computer_inspect result.
    pub node_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusReport {
    /// The accessibility service accepted the request; another agent can still steal focus.
    pub request_accepted: bool,
    pub outcome_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopReport {
    pub controller_halted: bool,
    pub desktop_close_confirmed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", content = "args", rename_all = "snake_case")]
pub enum Command {
    Start(StartArgs),
    Observe(ObserveArgs),
    Act(ActArgs),
    Inspect(InspectArgs),
    Focus(FocusArgs),
    Status,
    /// Close portal access but retain the actor for retryable teardown.
    CloseControl,
    /// Invalidate action references after an application launch.
    Invalidate,
    Stop,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Observation(Observation),
    Action(ActionReport),
    Inspection(crate::accessibility::Inspection),
    Focused(FocusReport),
    Stopped(StopReport),
}
impl Reply {
    pub fn observation(&self) -> Option<&Observation> {
        match self {
            Self::Observation(o) => Some(o),
            Self::Action(a) => a.observation.as_ref(),
            _ => None,
        }
    }
}
