//! Controls, actions, tuning values and the persisted settings format.
//!
//! Everything the plugin stores lives in one host setting ([`STATE_KEY`]),
//! written atomically by the host. The format is versioned and lenient: every
//! field has a default, unknown keys are ignored and each top-level field is
//! read independently, so a newer or partly damaged file never resets the
//! whole configuration (GPD-8).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The host setting holding [`PersistedState`].
pub const STATE_KEY: &str = "state";

/// Version written into [`PersistedState`] and [`ProfileFile`]. Files from
/// 0.x carry no version and read as 0.
pub const FORMAT_VERSION: u32 = 1;

/// At most this many saved profiles (keeps the settings file small).
pub const MAX_PROFILES: usize = 100;

/// Longest accepted profile name, in characters.
pub const MAX_PROFILE_NAME: usize = 64;

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// A physical control of a gamepad, using the standard (Xbox-style) layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Control {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftTrigger,
    RightTrigger,
    DPadLeft,
    DPadRight,
    DPadUp,
    DPadDown,
    South,
    East,
    North,
    West,
    LeftShoulder,
    RightShoulder,
    LeftStickClick,
    RightStickClick,
    Start,
    Select,
}

impl Control {
    pub const COUNT: usize = 20;

    /// Every control, in display order. Analog controls come first and their
    /// index (0–5) doubles as the index into a device's axis values.
    pub const ALL: [Control; Self::COUNT] = [
        Control::LeftStickX,
        Control::LeftStickY,
        Control::RightStickX,
        Control::RightStickY,
        Control::LeftTrigger,
        Control::RightTrigger,
        Control::DPadLeft,
        Control::DPadRight,
        Control::DPadUp,
        Control::DPadDown,
        Control::South,
        Control::East,
        Control::North,
        Control::West,
        Control::LeftShoulder,
        Control::RightShoulder,
        Control::LeftStickClick,
        Control::RightStickClick,
        Control::Start,
        Control::Select,
    ];

    /// Number of analog controls (sticks and triggers).
    pub const AXES: usize = 6;

    pub fn index(self) -> usize {
        self as usize
    }

    /// Sticks and triggers (as opposed to buttons).
    pub fn is_axis(self) -> bool {
        self.index() < Self::AXES
    }

    /// Triggers report 0 (released) to 1 (fully pressed).
    pub fn is_trigger(self) -> bool {
        matches!(self, Control::LeftTrigger | Control::RightTrigger)
    }

    /// The persisted key (unchanged from 0.x where the control existed).
    pub fn key(self) -> &'static str {
        match self {
            Control::LeftStickX => "left_stick_x",
            Control::LeftStickY => "left_stick_y",
            Control::RightStickX => "right_stick_x",
            Control::RightStickY => "right_stick_y",
            Control::LeftTrigger => "left_trigger",
            Control::RightTrigger => "right_trigger",
            Control::DPadLeft => "dpad_left",
            Control::DPadRight => "dpad_right",
            Control::DPadUp => "dpad_up",
            Control::DPadDown => "dpad_down",
            Control::South => "south",
            Control::East => "east",
            Control::North => "north",
            Control::West => "west",
            Control::LeftShoulder => "left_trigger_button",
            Control::RightShoulder => "right_trigger_button",
            Control::LeftStickClick => "left_stick_button",
            Control::RightStickClick => "right_stick_button",
            Control::Start => "start",
            Control::Select => "select",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Control::LeftStickX => "Left Stick X",
            Control::LeftStickY => "Left Stick Y",
            Control::RightStickX => "Right Stick X",
            Control::RightStickY => "Right Stick Y",
            Control::LeftTrigger => "Left Trigger",
            Control::RightTrigger => "Right Trigger",
            Control::DPadLeft => "D-Pad Left",
            Control::DPadRight => "D-Pad Right",
            Control::DPadUp => "D-Pad Up",
            Control::DPadDown => "D-Pad Down",
            Control::South => "South Button",
            Control::East => "East Button",
            Control::North => "North Button",
            Control::West => "West Button",
            Control::LeftShoulder => "Left Shoulder",
            Control::RightShoulder => "Right Shoulder",
            Control::LeftStickClick => "Left Stick Click",
            Control::RightStickClick => "Right Stick Click",
            Control::Start => "Start",
            Control::Select => "Select",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|control| control.key() == key)
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// What a control does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    None,
    /// Pans while deflected; right/up pans towards the right.
    PanHorizontal,
    /// Pans while deflected; up/right pans towards the top.
    PanVertical,
    /// Zooms while deflected; up/right zooms in.
    Zoom,
    /// Zooms in while held or deflected (either direction).
    ZoomIn,
    /// Zooms out while held or deflected (either direction).
    ZoomOut,
    /// Moves the system pointer (opt-in, see `pointer`).
    PointerHorizontal,
    PointerVertical,
    PreviousPane,
    NextPane,
    PreviousTab,
    NextTab,
    ToggleSeriesBar,
    PreviousSeriesItem,
    NextSeriesItem,
    OpenSeriesItem,
    FitView,
    ToggleWindow,
    TogglePause,
}

impl Action {
    /// Actions offered for sticks and triggers, in menu order.
    pub const AXIS: &'static [Action] = &[
        Action::None,
        Action::PanHorizontal,
        Action::PanVertical,
        Action::Zoom,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::PointerHorizontal,
        Action::PointerVertical,
    ];

    /// Actions offered for buttons, in menu order.
    pub const BUTTON: &'static [Action] = &[
        Action::None,
        Action::PreviousPane,
        Action::NextPane,
        Action::PreviousTab,
        Action::NextTab,
        Action::ToggleSeriesBar,
        Action::PreviousSeriesItem,
        Action::NextSeriesItem,
        Action::OpenSeriesItem,
        Action::FitView,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::ToggleWindow,
        Action::TogglePause,
    ];

    /// The menu entries for a control kind.
    pub fn options(axis: bool) -> &'static [Action] {
        if axis { Self::AXIS } else { Self::BUTTON }
    }

    /// Whether the action can be assigned to `control`.
    pub fn fits(self, control: Control) -> bool {
        Self::options(control.is_axis()).contains(&self)
    }

    /// The persisted key (the 0.x keys are kept so old profiles import).
    pub fn key(self) -> &'static str {
        match self {
            Action::None => "none",
            Action::PanHorizontal => "pan_horizontal",
            Action::PanVertical => "pan_vertical",
            Action::Zoom => "zoom",
            Action::ZoomIn => "zoom_in",
            Action::ZoomOut => "zoom_out",
            Action::PointerHorizontal => "mouse_horizontal",
            Action::PointerVertical => "mouse_vertical",
            Action::PreviousPane => "previous_pane",
            Action::NextPane => "next_pane",
            Action::PreviousTab => "previous_tab",
            Action::NextTab => "next_tab",
            Action::ToggleSeriesBar => "toggle_series_bar",
            Action::PreviousSeriesItem => "previous_series_item",
            Action::NextSeriesItem => "next_series_item",
            Action::OpenSeriesItem => "activate_series_item",
            Action::FitView => "fit_view",
            Action::ToggleWindow => "toggle_panel",
            Action::TogglePause => "toggle_controller",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::AXIS
            .iter()
            .chain(Self::BUTTON)
            .copied()
            .find(|action| action.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            Action::None => "Unassigned",
            Action::PanHorizontal => "Pan Horizontally",
            Action::PanVertical => "Pan Vertically",
            Action::Zoom => "Zoom (Up / Right Zooms In)",
            Action::ZoomIn => "Zoom In",
            Action::ZoomOut => "Zoom Out",
            Action::PointerHorizontal => "Move Pointer Horizontally",
            Action::PointerVertical => "Move Pointer Vertically",
            Action::PreviousPane => "Previous Pane",
            Action::NextPane => "Next Pane",
            Action::PreviousTab => "Previous Tab",
            Action::NextTab => "Next Tab",
            Action::ToggleSeriesBar => "Toggle Series Bar",
            Action::PreviousSeriesItem => "Previous Series Item",
            Action::NextSeriesItem => "Next Series Item",
            Action::OpenSeriesItem => "Open Selected Series Item",
            Action::FitView => "Fit View",
            Action::ToggleWindow => "Show / Hide This Window",
            Action::TogglePause => "Enable / Pause",
        }
    }

    pub fn moves_pointer(self) -> bool {
        matches!(self, Action::PointerHorizontal | Action::PointerVertical)
    }
}

/// The action of every control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mappings([Action; Control::COUNT]);

impl Mappings {
    /// Every control unassigned.
    pub fn empty() -> Self {
        Self([Action::None; Control::COUNT])
    }

    pub fn get(&self, control: Control) -> Action {
        self.0[control.index()]
    }

    /// Assigns `action` to `control`; refuses actions of the wrong kind.
    pub fn set(&mut self, control: Control, action: Action) -> bool {
        if !action.fits(control) {
            return false;
        }
        self.0[control.index()] = action;
        true
    }

    /// Whether any control moves the system pointer.
    pub fn uses_pointer(&self) -> bool {
        self.0.iter().any(|action| action.moves_pointer())
    }
}

impl Default for Mappings {
    /// Slide navigation without system-wide side effects: nothing moves the
    /// system pointer by default (GPD-2).
    fn default() -> Self {
        let mut mappings = Self::empty();
        for (control, action) in [
            (Control::LeftStickX, Action::PanHorizontal),
            (Control::LeftStickY, Action::PanVertical),
            (Control::RightStickY, Action::Zoom),
            (Control::LeftTrigger, Action::ZoomOut),
            (Control::RightTrigger, Action::ZoomIn),
            (Control::DPadLeft, Action::PreviousPane),
            (Control::DPadRight, Action::NextPane),
            (Control::DPadUp, Action::PreviousTab),
            (Control::DPadDown, Action::NextTab),
            (Control::South, Action::OpenSeriesItem),
            (Control::East, Action::FitView),
            (Control::North, Action::ToggleSeriesBar),
            (Control::LeftShoulder, Action::PreviousSeriesItem),
            (Control::RightShoulder, Action::NextSeriesItem),
            (Control::Start, Action::TogglePause),
            (Control::Select, Action::ToggleWindow),
        ] {
            mappings.set(control, action);
        }
        mappings
    }
}

// ---------------------------------------------------------------------------
// Tuning
// ---------------------------------------------------------------------------

/// One adjustable value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TuningKey {
    LeftDeadZone,
    RightDeadZone,
    TriggerDeadZone,
    PanSensitivity,
    ZoomSensitivity,
    ResponseCurve,
    PointerSpeed,
}

/// The single source of truth for a value's range: the UI slider, the step
/// buttons and every clamp use it (GPD-8).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// Increment of the step buttons.
    pub step: f32,
}

impl Range {
    /// Clamps into the range and rounds to 0.01; non-finite input gives the
    /// default.
    pub fn clamp(&self, value: f32) -> f32 {
        if !value.is_finite() {
            return self.default;
        }
        ((value.clamp(self.min, self.max)) * 100.0).round() / 100.0
    }
}

impl TuningKey {
    pub const ALL: [TuningKey; 7] = [
        TuningKey::LeftDeadZone,
        TuningKey::RightDeadZone,
        TuningKey::TriggerDeadZone,
        TuningKey::PanSensitivity,
        TuningKey::ZoomSensitivity,
        TuningKey::ResponseCurve,
        TuningKey::PointerSpeed,
    ];

    /// The key used by the UI.
    pub fn key(self) -> &'static str {
        match self {
            TuningKey::LeftDeadZone => "left-dead-zone",
            TuningKey::RightDeadZone => "right-dead-zone",
            TuningKey::TriggerDeadZone => "trigger-dead-zone",
            TuningKey::PanSensitivity => "pan-sensitivity",
            TuningKey::ZoomSensitivity => "zoom-sensitivity",
            TuningKey::ResponseCurve => "response-curve",
            TuningKey::PointerSpeed => "pointer-speed",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            TuningKey::LeftDeadZone => "Left Dead Zone",
            TuningKey::RightDeadZone => "Right Dead Zone",
            TuningKey::TriggerDeadZone => "Trigger Dead Zone",
            TuningKey::PanSensitivity => "Pan Speed",
            TuningKey::ZoomSensitivity => "Zoom Speed",
            TuningKey::ResponseCurve => "Response Curve",
            TuningKey::PointerSpeed => "Pointer Speed",
        }
    }

    pub fn range(self) -> Range {
        let dead_zone = |default| Range {
            min: 0.0,
            max: 0.9,
            default,
            step: 0.01,
        };
        let speed = |default| Range {
            min: 0.1,
            max: 3.0,
            default,
            step: 0.05,
        };
        match self {
            TuningKey::LeftDeadZone => dead_zone(0.16),
            TuningKey::RightDeadZone => dead_zone(0.12),
            TuningKey::TriggerDeadZone => dead_zone(0.08),
            TuningKey::PanSensitivity => speed(0.95),
            TuningKey::ZoomSensitivity => speed(0.78),
            TuningKey::ResponseCurve => Range {
                min: 1.0,
                max: 3.0,
                default: 1.6,
                step: 0.1,
            },
            TuningKey::PointerSpeed => speed(1.0),
        }
    }
}

/// The analog tuning of a profile. Values are always within their
/// [`TuningKey::range`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    pub left_dead_zone: f32,
    pub right_dead_zone: f32,
    pub trigger_dead_zone: f32,
    pub pan_sensitivity: f32,
    pub zoom_sensitivity: f32,
    /// Exponent applied after the dead zone (1 = linear; higher gives finer
    /// control near the centre).
    pub response_curve: f32,
    pub pointer_speed: f32,
}

impl Default for Tuning {
    fn default() -> Self {
        let mut tuning = Tuning {
            left_dead_zone: 0.0,
            right_dead_zone: 0.0,
            trigger_dead_zone: 0.0,
            pan_sensitivity: 0.0,
            zoom_sensitivity: 0.0,
            response_curve: 0.0,
            pointer_speed: 0.0,
        };
        for key in TuningKey::ALL {
            tuning.set(key, key.range().default);
        }
        tuning
    }
}

impl Tuning {
    pub fn get(&self, key: TuningKey) -> f32 {
        match key {
            TuningKey::LeftDeadZone => self.left_dead_zone,
            TuningKey::RightDeadZone => self.right_dead_zone,
            TuningKey::TriggerDeadZone => self.trigger_dead_zone,
            TuningKey::PanSensitivity => self.pan_sensitivity,
            TuningKey::ZoomSensitivity => self.zoom_sensitivity,
            TuningKey::ResponseCurve => self.response_curve,
            TuningKey::PointerSpeed => self.pointer_speed,
        }
    }

    /// Sets a value (clamped to its range); returns the stored value.
    pub fn set(&mut self, key: TuningKey, value: f32) -> f32 {
        let value = key.range().clamp(value);
        let slot = match key {
            TuningKey::LeftDeadZone => &mut self.left_dead_zone,
            TuningKey::RightDeadZone => &mut self.right_dead_zone,
            TuningKey::TriggerDeadZone => &mut self.trigger_dead_zone,
            TuningKey::PanSensitivity => &mut self.pan_sensitivity,
            TuningKey::ZoomSensitivity => &mut self.zoom_sensitivity,
            TuningKey::ResponseCurve => &mut self.response_curve,
            TuningKey::PointerSpeed => &mut self.pointer_speed,
        };
        *slot = value;
        value
    }
}

/// A complete controller configuration (what a profile stores).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub tuning: Tuning,
    pub mappings: Mappings,
}

// ---------------------------------------------------------------------------
// Persisted formats
// ---------------------------------------------------------------------------

/// A profile as stored and exported. Reads profiles written by 0.x, which
/// had no `version`, `response_curve` or `pointer_speed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileFile {
    /// 0 for files written by 0.x.
    #[serde(default)]
    pub version: u32,
    #[serde(serialize_with = "hundredths")]
    pub left_dead_zone: f32,
    #[serde(serialize_with = "hundredths")]
    pub right_dead_zone: f32,
    #[serde(serialize_with = "hundredths")]
    pub trigger_dead_zone: f32,
    #[serde(serialize_with = "hundredths")]
    pub pan_sensitivity: f32,
    #[serde(serialize_with = "hundredths")]
    pub zoom_sensitivity: f32,
    #[serde(serialize_with = "hundredths")]
    pub response_curve: f32,
    #[serde(serialize_with = "hundredths")]
    pub pointer_speed: f32,
    /// Control key → action key.
    pub mappings: BTreeMap<String, String>,
}

/// Writes `0.95` rather than `0.949999988079071` (values are kept to 0.01).
fn hundredths<S: serde::Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64((f64::from(*value) * 100.0).round() / 100.0)
}

impl Default for ProfileFile {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl From<&Config> for ProfileFile {
    fn from(config: &Config) -> Self {
        let t = &config.tuning;
        Self {
            version: FORMAT_VERSION,
            left_dead_zone: t.left_dead_zone,
            right_dead_zone: t.right_dead_zone,
            trigger_dead_zone: t.trigger_dead_zone,
            pan_sensitivity: t.pan_sensitivity,
            zoom_sensitivity: t.zoom_sensitivity,
            response_curve: t.response_curve,
            pointer_speed: t.pointer_speed,
            mappings: Control::ALL
                .iter()
                .map(|control| {
                    (
                        control.key().to_string(),
                        config.mappings.get(*control).key().to_string(),
                    )
                })
                .collect(),
        }
    }
}

impl ProfileFile {
    /// The configuration this file describes. Out-of-range values are
    /// clamped; unknown controls and actions, and actions of the wrong kind,
    /// are ignored; controls missing from `mappings` are unassigned.
    pub fn to_config(&self) -> Config {
        let mut tuning = Tuning::default();
        for (key, value) in [
            (TuningKey::LeftDeadZone, self.left_dead_zone),
            (TuningKey::RightDeadZone, self.right_dead_zone),
            (TuningKey::TriggerDeadZone, self.trigger_dead_zone),
            (TuningKey::PanSensitivity, self.pan_sensitivity),
            (TuningKey::ZoomSensitivity, self.zoom_sensitivity),
            (TuningKey::ResponseCurve, self.response_curve),
            (TuningKey::PointerSpeed, self.pointer_speed),
        ] {
            tuning.set(key, value);
        }
        let mut mappings = Mappings::empty();
        for (control_key, action_key) in &self.mappings {
            if let (Some(control), Some(action)) =
                (Control::from_key(control_key), Action::from_key(action_key))
            {
                mappings.set(control, action);
            }
        }
        Config { tuning, mappings }
    }

    /// The keys a profile may have.
    const FIELDS: [&'static str; 9] = [
        "version",
        "left_dead_zone",
        "right_dead_zone",
        "trigger_dead_zone",
        "pan_sensitivity",
        "zoom_sensitivity",
        "response_curve",
        "pointer_speed",
        "mappings",
    ];

    /// Reads a profile field by field: a value of the wrong type keeps its
    /// default (and is reported) instead of discarding the whole profile.
    pub fn from_value(value: &Value) -> Result<(Self, Vec<String>), String> {
        let map = value
            .as_object()
            .ok_or_else(|| "not a profile object".to_string())?;
        let mut file = Self {
            version: 0,
            ..Self::default()
        };
        let mut warnings = Vec::new();
        if let Some(version) = map.get("version") {
            match version.as_u64().and_then(|v| u32::try_from(v).ok()) {
                Some(version) => file.version = version,
                None => warnings.push(format!("ignoring the version {version}")),
            }
        }
        for (key, slot) in [
            ("left_dead_zone", &mut file.left_dead_zone),
            ("right_dead_zone", &mut file.right_dead_zone),
            ("trigger_dead_zone", &mut file.trigger_dead_zone),
            ("pan_sensitivity", &mut file.pan_sensitivity),
            ("zoom_sensitivity", &mut file.zoom_sensitivity),
            ("response_curve", &mut file.response_curve),
            ("pointer_speed", &mut file.pointer_speed),
        ] {
            if let Some(value) = map.get(key) {
                match value.as_f64() {
                    Some(number) => *slot = number as f32,
                    None => warnings.push(format!("ignoring '{key}': {value} is not a number")),
                }
            }
        }
        if let Some(mappings) = map.get("mappings") {
            match mappings.as_object() {
                Some(mappings) => {
                    file.mappings = mappings
                        .iter()
                        .filter_map(|(control, action)| {
                            Some((control.clone(), action.as_str()?.to_string()))
                        })
                        .collect();
                }
                None => warnings.push("ignoring 'mappings': not an object".to_string()),
            }
        }
        Ok((file, warnings))
    }

    /// Parses an exported or 0.x profile file. Refuses JSON that is not a
    /// profile (no profile field at all), so an unrelated file is not
    /// imported as the defaults.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let is_profile = value
            .as_object()
            .is_some_and(|map| Self::FIELDS.iter().any(|key| map.contains_key(*key)));
        if !is_profile {
            return Err("the file is not a gamepad profile".to_string());
        }
        Self::from_value(&value).map(|(file, _)| file)
    }
}

/// Everything the plugin persists, under [`STATE_KEY`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u32,
    pub enabled: bool,
    /// The preferred controller's key; `None` uses every controller.
    pub device: Option<String>,
    /// Opt-in system pointer control (GPD-2).
    pub pointer_control: bool,
    pub active_profile: Option<String>,
    /// The live configuration.
    pub config: ProfileFile,
    pub profiles: BTreeMap<String, ProfileFile>,
    /// XDG RemoteDesktop portal restore token (pointer control on Wayland).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal_restore_token: Option<String>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            enabled: true,
            device: None,
            pointer_control: false,
            active_profile: None,
            config: ProfileFile::default(),
            profiles: BTreeMap::new(),
            portal_restore_token: None,
        }
    }
}

impl PersistedState {
    /// Reads a stored state field by field; fields that cannot be read keep
    /// their defaults and are reported in the returned warnings.
    pub fn from_value(value: Option<&Value>) -> (Self, Vec<String>) {
        let mut state = Self::default();
        let mut warnings = Vec::new();
        let Some(value) = value else {
            return (state, warnings);
        };
        let Some(map) = value.as_object() else {
            warnings.push("the stored gamepad settings are not an object".to_string());
            return (state, warnings);
        };
        fn field<T: serde::de::DeserializeOwned>(
            map: &serde_json::Map<String, Value>,
            key: &str,
            warnings: &mut Vec<String>,
        ) -> Option<T> {
            let value = map.get(key)?;
            match serde_json::from_value(value.clone()) {
                Ok(parsed) => Some(parsed),
                Err(error) => {
                    warnings.push(format!("ignoring the stored '{key}': {error}"));
                    None
                }
            }
        }
        if let Some(version) = field(map, "version", &mut warnings) {
            state.version = version;
        }
        if let Some(enabled) = field(map, "enabled", &mut warnings) {
            state.enabled = enabled;
        }
        if let Some(device) = field(map, "device", &mut warnings) {
            state.device = device;
        }
        if let Some(pointer) = field(map, "pointer_control", &mut warnings) {
            state.pointer_control = pointer;
        }
        if let Some(active) = field(map, "active_profile", &mut warnings) {
            state.active_profile = active;
        }
        if let Some(config) = map.get("config") {
            match ProfileFile::from_value(config) {
                Ok((config, problems)) => {
                    state.config = config;
                    warnings.extend(problems.into_iter().map(|p| format!("settings: {p}")));
                }
                Err(error) => warnings.push(format!("ignoring the stored settings: {error}")),
            }
        }
        if let Some(token) = field(map, "portal_restore_token", &mut warnings) {
            state.portal_restore_token = token;
        }
        if let Some(profiles) = map.get("profiles").and_then(Value::as_object) {
            for (name, profile) in profiles {
                if normalize_profile_name(name).as_deref() != Ok(name) {
                    warnings.push(format!("ignoring the profile with the name {name:?}"));
                    continue;
                }
                if state.profiles.len() >= MAX_PROFILES {
                    warnings.push(format!(
                        "ignoring the profile {name:?}: at most {MAX_PROFILES} profiles are kept"
                    ));
                    continue;
                }
                match ProfileFile::from_value(profile) {
                    Ok((profile, problems)) => {
                        state.profiles.insert(name.clone(), profile);
                        warnings.extend(problems.into_iter().map(|p| format!("{name}: {p}")));
                    }
                    Err(error) => warnings.push(format!("ignoring the profile {name:?}: {error}")),
                }
            }
        }
        (state, warnings)
    }
}

// ---------------------------------------------------------------------------
// In-memory preferences
// ---------------------------------------------------------------------------

/// The plugin's persistent preferences, in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    pub enabled: bool,
    pub device: Option<String>,
    pub pointer_control: bool,
    /// The profile that edits are saved into, if any.
    pub active_profile: Option<String>,
    pub config: Config,
    pub profiles: BTreeMap<String, Config>,
    pub portal_restore_token: Option<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self::from_persisted(PersistedState::default())
    }
}

impl Prefs {
    pub fn from_persisted(state: PersistedState) -> Self {
        let profiles: BTreeMap<String, Config> = state
            .profiles
            .iter()
            .take(MAX_PROFILES)
            .map(|(name, profile)| (name.clone(), profile.to_config()))
            .collect();
        let active_profile = state
            .active_profile
            .filter(|name| profiles.contains_key(name));
        Self {
            enabled: state.enabled,
            device: state.device.filter(|key| !key.is_empty()),
            pointer_control: state.pointer_control,
            active_profile,
            config: state.config.to_config(),
            profiles,
            portal_restore_token: state.portal_restore_token,
        }
    }

    pub fn to_persisted(&self) -> PersistedState {
        PersistedState {
            version: FORMAT_VERSION,
            enabled: self.enabled,
            device: self.device.clone(),
            pointer_control: self.pointer_control,
            active_profile: self.active_profile.clone(),
            config: ProfileFile::from(&self.config),
            profiles: self
                .profiles
                .iter()
                .map(|(name, config)| (name.clone(), ProfileFile::from(config)))
                .collect(),
            portal_restore_token: self.portal_restore_token.clone(),
        }
    }

    /// Profile names in display order (case-insensitive).
    pub fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b)));
        names
    }

    /// Applies an edit to the live configuration and saves it into the
    /// active profile (never into another one, GPD-8).
    pub fn edit_config(&mut self, edit: impl FnOnce(&mut Config)) {
        edit(&mut self.config);
        if let Some(profile) = self
            .active_profile
            .as_ref()
            .and_then(|name| self.profiles.get_mut(name))
        {
            *profile = self.config.clone();
        }
    }
}

// ---------------------------------------------------------------------------
// Profile names
// ---------------------------------------------------------------------------

/// Validates a profile name typed by the user: surrounding whitespace is
/// removed (only here, never per keystroke), the name must not be empty, too
/// long or contain control characters. Names are map keys, never paths.
pub fn normalize_profile_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("Enter a name for the profile.".to_string());
    }
    if name.chars().count() > MAX_PROFILE_NAME {
        return Err(format!(
            "Profile names are limited to {MAX_PROFILE_NAME} characters."
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("Profile names cannot contain control characters.".to_string());
    }
    Ok(name.to_string())
}

/// `base`, or `base 2`, `base 3`, … — the first name `taken` rejects not.
pub fn unique_profile_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let base: String = base.chars().take(MAX_PROFILE_NAME - 4).collect();
    let base = base.trim();
    let base = if base.is_empty() { "Profile" } else { base };
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| !taken(candidate))
        .expect("an unbounded range always yields a free name")
}

/// A file name for exporting a profile: path separators and other characters
/// that are awkward in file names are replaced.
pub fn export_file_name(profile: &str) -> String {
    let stem: String = profile
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if stem.chars().any(char::is_alphanumeric) {
        format!("{stem}.json")
    } else {
        "gamepad-profile.json".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_round_trip_and_are_unique() {
        for control in Control::ALL {
            assert_eq!(Control::from_key(control.key()), Some(control));
        }
        for action in Action::AXIS.iter().chain(Action::BUTTON) {
            assert_eq!(Action::from_key(action.key()), Some(*action));
        }
        let mut keys: Vec<_> = Control::ALL.iter().map(|c| c.key()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), Control::COUNT);
        assert_eq!(
            Control::ALL.iter().filter(|c| c.is_axis()).count(),
            Control::AXES
        );
    }

    #[test]
    fn defaults_never_move_the_system_pointer() {
        let mappings = Mappings::default();
        assert!(!mappings.uses_pointer());
        assert_eq!(mappings.get(Control::LeftStickX), Action::PanHorizontal);
        assert_eq!(mappings.get(Control::RightTrigger), Action::ZoomIn);
        for control in Control::ALL {
            assert!(mappings.get(control).fits(control), "{control:?}");
        }
    }

    #[test]
    fn mappings_refuse_actions_of_the_wrong_kind() {
        let mut mappings = Mappings::empty();
        assert!(!mappings.set(Control::LeftStickX, Action::FitView));
        assert!(!mappings.set(Control::South, Action::PanVertical));
        assert!(mappings.set(Control::South, Action::ZoomIn));
        assert!(mappings.set(Control::LeftTrigger, Action::ZoomIn));
    }

    #[test]
    fn ranges_clamp_round_and_reject_non_finite_values() {
        let range = TuningKey::LeftDeadZone.range();
        assert_eq!(range.clamp(1.0), 0.9);
        assert_eq!(range.clamp(-1.0), 0.0);
        assert_eq!(range.clamp(0.123_456), 0.12);
        assert_eq!(range.clamp(f32::NAN), range.default);
        assert_eq!(range.clamp(f32::INFINITY), range.default);
        for key in TuningKey::ALL {
            let r = key.range();
            assert!(
                r.min < r.max && (r.min..=r.max).contains(&r.default),
                "{key:?}"
            );
            assert_eq!(TuningKey::from_key(key.key()), Some(key));
        }
    }

    #[test]
    fn legacy_profiles_import() {
        // A profile written by 0.2.x: no version, no new fields, mouse
        // mappings on the right stick.
        let legacy = r#"{
            "left_dead_zone": 0.2,
            "right_dead_zone": 0.12,
            "trigger_dead_zone": 1.5,
            "pan_sensitivity": 4.0,
            "zoom_sensitivity": 0.78,
            "mappings": {
                "left_stick_x": "pan_horizontal",
                "right_stick_x": "mouse_horizontal",
                "south": "activate_series_item",
                "east": "pan_vertical",
                "made_up_control": "zoom",
                "north": "made_up_action"
            }
        }"#;
        let file = ProfileFile::parse(legacy).unwrap();
        assert_eq!(file.version, 0);
        let config = file.to_config();
        assert_eq!(config.tuning.left_dead_zone, 0.2);
        // Clamped to the slider ranges.
        assert_eq!(config.tuning.trigger_dead_zone, 0.9);
        assert_eq!(config.tuning.pan_sensitivity, 3.0);
        // Missing new fields get their defaults.
        assert_eq!(config.tuning.response_curve, 1.6);
        let m = &config.mappings;
        assert_eq!(m.get(Control::LeftStickX), Action::PanHorizontal);
        assert_eq!(m.get(Control::RightStickX), Action::PointerHorizontal);
        assert_eq!(m.get(Control::South), Action::OpenSeriesItem);
        // Wrong kinds and unknown actions are dropped; missing controls are
        // unassigned.
        assert_eq!(m.get(Control::East), Action::None);
        assert_eq!(m.get(Control::North), Action::None);
        assert_eq!(m.get(Control::Start), Action::None);
        assert!(ProfileFile::parse("[1, 2]").is_err());
        assert!(ProfileFile::parse("not json").is_err());
        // Unrelated JSON is not a profile.
        assert!(ProfileFile::parse("{}").is_err());
        assert!(ProfileFile::parse(r#"{"theme": "dark"}"#).is_err());
    }

    #[test]
    fn one_bad_value_does_not_discard_a_profile() {
        let (file, warnings) = ProfileFile::from_value(&json!({
            "pan_sensitivity": "fast",
            "zoom_sensitivity": 1.5,
            "mappings": { "south": "fit_view", "east": 7 }
        }))
        .unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let config = file.to_config();
        assert_eq!(config.tuning.pan_sensitivity, 0.95);
        assert_eq!(config.tuning.zoom_sensitivity, 1.5);
        assert_eq!(config.mappings.get(Control::South), Action::FitView);
        assert_eq!(config.mappings.get(Control::East), Action::None);
        assert!(ProfileFile::from_value(&json!(3)).is_err());
    }

    #[test]
    fn profiles_round_trip() {
        let mut config = Config::default();
        config.tuning.set(TuningKey::PanSensitivity, 1.25);
        config.mappings.set(Control::West, Action::FitView);
        let file = ProfileFile::from(&config);
        let text = serde_json::to_string(&file).unwrap();
        // Readable numbers in exported files.
        assert!(text.contains(r#""zoom_sensitivity":0.78"#), "{text}");
        let back = ProfileFile::parse(&text).unwrap().to_config();
        assert_eq!(back, config);
    }

    #[test]
    fn state_is_read_field_by_field() {
        let stored = json!({
            "version": 1,
            "enabled": false,
            "device": "045e:02ea:x#2",
            "pointer_control": "yes please",
            "active_profile": "Desk",
            "config": { "pan_sensitivity": 2.0, "zoom_sensitivity": "slow" },
            "profiles": {
                "Desk": { "zoom_sensitivity": 1.5 },
                "Broken": 7,
                "  padded  ": {}
            },
            "from_the_future": [1, 2, 3]
        });
        let (state, warnings) = PersistedState::from_value(Some(&stored));
        assert!(!state.enabled);
        assert_eq!(state.device.as_deref(), Some("045e:02ea:x#2"));
        // The bad field keeps its default, the others survive.
        assert!(!state.pointer_control);
        assert_eq!(state.config.pan_sensitivity, 2.0);
        assert_eq!(state.config.zoom_sensitivity, 0.78);
        assert_eq!(state.profiles.len(), 1);
        assert_eq!(warnings.len(), 4, "{warnings:?}");

        let prefs = Prefs::from_persisted(state);
        assert_eq!(prefs.active_profile.as_deref(), Some("Desk"));
        assert_eq!(prefs.profiles["Desk"].tuning.zoom_sensitivity, 1.5);
        let again = Prefs::from_persisted(prefs.to_persisted());
        assert_eq!(again, prefs);

        let (defaults, warnings) = PersistedState::from_value(None);
        assert!(defaults.enabled && warnings.is_empty());
        let (_, warnings) = PersistedState::from_value(Some(&json!("garbage")));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn profiles_beyond_the_limit_are_reported() {
        let profiles: serde_json::Map<String, Value> = (0..MAX_PROFILES + 3)
            .map(|n| (format!("p{n:03}"), json!({})))
            .collect();
        let (state, warnings) = PersistedState::from_value(Some(&json!({ "profiles": profiles })));
        assert_eq!(state.profiles.len(), MAX_PROFILES);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
    }

    #[test]
    fn missing_active_profiles_are_dropped() {
        let state = PersistedState {
            active_profile: Some("Gone".into()),
            ..Default::default()
        };
        assert_eq!(Prefs::from_persisted(state).active_profile, None);
    }

    #[test]
    fn edits_go_to_the_active_profile_only() {
        let mut prefs = Prefs::default();
        prefs.profiles.insert("A".into(), Config::default());
        prefs.profiles.insert("B".into(), Config::default());
        prefs.active_profile = Some("A".into());
        prefs.edit_config(|c| {
            c.tuning.set(TuningKey::ZoomSensitivity, 2.0);
        });
        assert_eq!(prefs.profiles["A"].tuning.zoom_sensitivity, 2.0);
        assert_eq!(prefs.profiles["B"].tuning.zoom_sensitivity, 0.78);
        prefs.active_profile = None;
        prefs.edit_config(|c| {
            c.tuning.set(TuningKey::ZoomSensitivity, 0.5);
        });
        assert_eq!(prefs.profiles["A"].tuning.zoom_sensitivity, 2.0);
    }

    #[test]
    fn profile_names_are_validated_not_trimmed_per_keystroke() {
        assert_eq!(
            normalize_profile_name("  Xbox Elite  ").unwrap(),
            "Xbox Elite"
        );
        assert!(normalize_profile_name("   ").is_err());
        assert!(normalize_profile_name("a\nb").is_err());
        assert!(normalize_profile_name(&"x".repeat(65)).is_err());
        // Path-like names are just names.
        assert_eq!(normalize_profile_name("a/b").unwrap(), "a/b");
        assert_eq!(
            normalize_profile_name("/etc/passwd").unwrap(),
            "/etc/passwd"
        );
    }

    #[test]
    fn unique_names_and_file_names() {
        let taken = |n: &str| ["Desk", "Desk 2"].contains(&n);
        assert_eq!(unique_profile_name("Desk", taken), "Desk 3");
        assert_eq!(unique_profile_name("Lab", taken), "Lab");
        assert_eq!(unique_profile_name("  ", taken), "Profile");
        assert_eq!(export_file_name("a/b: c"), "a_b_ c.json");
        assert_eq!(export_file_name("../.."), "gamepad-profile.json");
        assert_eq!(export_file_name("Xbox Elite"), "Xbox Elite.json");
    }
}
