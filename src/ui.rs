//! What the settings window shows: the properties pushed to the surface.
//!
//! The window (`ui/gamepad-window.slint`) is a plugin surface of kind
//! `Window` opened through the host. The plugin computes every property from
//! its state and pushes them with `Surface::set_many`; the host only applies
//! values that changed and patches array rows in place. Ranges, labels and
//! defaults come from [`crate::config`], so the slider ranges always match the
//! clamps (GPD-8).

use crate::config::{Action, Control, Prefs, TuningKey};
use crate::shared::{BackendState, PointerState, Status};
use serde_json::{Map, Value, json};

pub const UI_PATH: &str = "ui/gamepad-window.slint";
pub const COMPONENT: &str = "GamepadWindow";
pub const TITLE: &str = "Gamepad";

/// Preferred size; the host keeps windows within the screen.
pub const WIDTH: f32 = 960.0;
pub const HEIGHT: f32 = 760.0;

/// The callbacks the window declares; the plugin handles exactly these.
#[cfg(test)]
pub const CALLBACKS: &[&str] = &[
    "enabled-toggled",
    "device-selected",
    "profile-selected",
    "save-profile-as",
    "new-profile",
    "import-profile",
    "export-profile",
    "delete-profile",
    "tuning-changed",
    "tuning-step",
    "tuning-reset",
    "pointer-control-toggled",
    "mapping-selected",
    "window-active-changed",
];

/// The tuning rows shown in the Tuning card (pointer speed sits in the
/// pointer card).
const TUNING_ROWS: [TuningKey; 6] = [
    TuningKey::LeftDeadZone,
    TuningKey::RightDeadZone,
    TuningKey::TriggerDeadZone,
    TuningKey::PanSensitivity,
    TuningKey::ZoomSensitivity,
    TuningKey::ResponseCurve,
];

/// Status colour in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Tone {
    Neutral = 0,
    Good = 1,
    Warning = 2,
    Error = 3,
}

/// Where keyboard focus is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Focus {
    pub main_window: bool,
    pub settings_window: bool,
}

/// The computed properties plus what the device list indices mean.
pub struct Model {
    pub properties: Map<String, Value>,
    /// `device-options[i]` selects `device_keys[i]` (`None`: every
    /// controller).
    pub device_keys: Vec<Option<String>>,
}

/// Rounds for display and stable JSON (`0.16`, not `0.1599999964`).
fn number(value: f32) -> f64 {
    (f64::from(value) * 100.0).round() / 100.0
}

fn tuning_row(prefs: &Prefs, key: TuningKey) -> Value {
    let range = key.range();
    let value = prefs.config.tuning.get(key);
    json!({
        "key": key.key(),
        "label": key.label(),
        "value": number(value),
        "text": format!("{value:.2}"),
        "min": number(range.min),
        "max": number(range.max),
        "default-value": number(range.default),
    })
}

fn mapping_rows(prefs: &Prefs, axis: bool) -> Value {
    Value::Array(
        Control::ALL
            .into_iter()
            .filter(|control| control.is_axis() == axis)
            .map(|control| {
                let action = prefs.config.mappings.get(control);
                let index = Action::options(axis)
                    .iter()
                    .position(|candidate| *candidate == action)
                    .unwrap_or(0);
                json!({
                    "control-key": control.key(),
                    "control-label": control.label(),
                    "current-index": index,
                    "axis-control": axis,
                })
            })
            .collect(),
    )
}

fn labels(actions: &[Action]) -> Value {
    Value::Array(actions.iter().map(|a| json!(a.label())).collect())
}

/// The controller input comes from: the preferred one while it is connected,
/// otherwise every controller (`None`) — the rule the engine applies.
fn effective_device<'a>(prefs: &'a Prefs, status: &Status) -> Option<&'a str> {
    prefs
        .device
        .as_deref()
        .filter(|key| status.devices.iter().any(|d| d.key == *key))
}

fn status_line(prefs: &Prefs, status: &Status, focus: Focus) -> (String, Tone) {
    match &status.backend {
        BackendState::Starting => ("Starting…".to_string(), Tone::Neutral),
        BackendState::Unavailable(message) => (
            format!("Controllers are unavailable: {message}"),
            Tone::Error,
        ),
        BackendState::Running if !prefs.enabled => (
            "Paused. Turn it on here or press the Enable / Pause button.".to_string(),
            Tone::Warning,
        ),
        BackendState::Running if status.devices.is_empty() => {
            ("Waiting for a controller".to_string(), Tone::Neutral)
        }
        BackendState::Running if !(focus.main_window || focus.settings_window) => (
            "Ready. Input is ignored while eov is in the background.".to_string(),
            Tone::Neutral,
        ),
        BackendState::Running => {
            let source = match effective_device(prefs, status) {
                Some(key) => status
                    .devices
                    .iter()
                    .find(|d| d.key == key)
                    .map(|d| d.label())
                    .unwrap_or_default(),
                None if status.devices.len() == 1 => status.devices[0].label(),
                None => format!("{} controllers", status.devices.len()),
            };
            (format!("Ready: {source}"), Tone::Good)
        }
    }
}

fn device_note(prefs: &Prefs, status: &Status) -> String {
    if let BackendState::Unavailable(message) = &status.backend {
        return status
            .access_hint
            .clone()
            .unwrap_or_else(|| format!("The controller subsystem could not start: {message}"));
    }
    if status.devices.is_empty() {
        return status.access_hint.clone().unwrap_or_else(|| {
            "No controllers detected. Connect one by USB or Bluetooth.".to_string()
        });
    }
    match (&prefs.device, effective_device(prefs, status)) {
        (Some(_), None) => {
            "The chosen controller is not connected, so every controller is used.".to_string()
        }
        (_, None) if status.devices.len() > 1 => {
            "Every connected controller drives eov.".to_string()
        }
        _ => String::new(),
    }
}

fn pointer_note(prefs: &Prefs, status: &Status) -> String {
    let mapped = prefs.config.mappings.uses_pointer();
    if !prefs.pointer_control {
        return if mapped {
            "Your mappings include Move Pointer. Turn this on to let them move the system \
             pointer while eov has focus."
                .to_string()
        } else {
            "Off. When on, Move Pointer mappings move the system pointer while eov has \
             focus. Wayland asks for Remote Desktop permission and macOS for \
             Accessibility access."
                .to_string()
        };
    }
    let state = match &status.pointer {
        PointerState::Off => "On. Starts when a mapped stick moves.".to_string(),
        PointerState::Connecting => "Connecting. The system may ask for permission.".to_string(),
        PointerState::Active(how) => format!("Active ({how})."),
        PointerState::Failed(why) => format!("Not available: {why}"),
    };
    if mapped {
        state
    } else {
        format!("{state} Map a stick to Move Pointer to use it.")
    }
}

/// Every property of the window.
pub fn model(prefs: &Prefs, status: &Status, focus: Focus) -> Model {
    let (status_text, tone) = status_line(prefs, status, focus);
    let mut device_keys = vec![None];
    let mut device_options = vec![json!("All controllers")];
    for device in &status.devices {
        device_keys.push(Some(device.key.clone()));
        device_options.push(json!(device.label()));
    }
    let selected_device = effective_device(prefs, status)
        .and_then(|key| device_keys.iter().position(|k| k.as_deref() == Some(key)))
        .unwrap_or(0);
    let names = prefs.profile_names();
    let selected_profile = prefs
        .active_profile
        .as_ref()
        .and_then(|active| names.iter().position(|name| name == active))
        .map_or(-1, |index| index as i64);
    let profile_note = match &prefs.active_profile {
        Some(name) => format!("Changes are saved to “{name}”."),
        None if names.is_empty() => {
            "Changes are saved automatically. Save them as a profile to keep several \
             layouts."
                .to_string()
        }
        None => "Changes are saved automatically but not to a profile.".to_string(),
    };

    let mut p = Map::new();
    p.insert("controller-enabled".into(), json!(prefs.enabled));
    p.insert("status-text".into(), json!(status_text));
    p.insert("status-tone".into(), json!(tone as i32));
    p.insert("device-options".into(), Value::Array(device_options));
    p.insert("selected-device-index".into(), json!(selected_device));
    p.insert("device-note".into(), json!(device_note(prefs, status)));
    p.insert("profile-options".into(), json!(names));
    p.insert("selected-profile-index".into(), json!(selected_profile));
    p.insert("profile-note".into(), json!(profile_note));
    p.insert(
        "tuning-rows".into(),
        Value::Array(TUNING_ROWS.iter().map(|k| tuning_row(prefs, *k)).collect()),
    );
    p.insert(
        "pointer-row".into(),
        tuning_row(prefs, TuningKey::PointerSpeed),
    );
    p.insert("pointer-control".into(), json!(prefs.pointer_control));
    p.insert("pointer-note".into(), json!(pointer_note(prefs, status)));
    p.insert("axis-action-options".into(), labels(Action::AXIS));
    p.insert("button-action-options".into(), labels(Action::BUTTON));
    p.insert("axis-mappings".into(), mapping_rows(prefs, true));
    p.insert("button-mappings".into(), mapping_rows(prefs, false));
    Model {
        properties: p,
        device_keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{DeviceInfo, Power};

    fn pad(key: &str, occurrence: u32) -> DeviceInfo {
        DeviceInfo {
            id: u64::from(occurrence),
            key: key.to_string(),
            name: "DualSense Wireless Controller".to_string(),
            occurrence,
            power: Power::Discharging(80),
        }
    }

    fn running(devices: Vec<DeviceInfo>) -> Status {
        Status {
            backend: BackendState::Running,
            devices,
            ..Status::default()
        }
    }

    const FOCUSED: Focus = Focus {
        main_window: true,
        settings_window: false,
    };

    #[test]
    fn status_reflects_pause_focus_and_devices() {
        let mut prefs = Prefs::default();
        let one = running(vec![pad("k", 1)]);
        let (text, tone) = status_line(&prefs, &one, FOCUSED);
        assert_eq!(text, "Ready: DualSense Wireless Controller · battery 80%");
        assert_eq!(tone, Tone::Good);
        let (text, _) = status_line(&prefs, &one, Focus::default());
        assert!(text.contains("background"), "{text}");
        let (text, _) = status_line(&prefs, &running(vec![]), FOCUSED);
        assert_eq!(text, "Waiting for a controller");
        prefs.enabled = false;
        let (text, tone) = status_line(&prefs, &one, FOCUSED);
        assert!(text.starts_with("Paused"), "{text}");
        assert_eq!(tone, Tone::Warning);
        let unavailable = Status {
            backend: BackendState::Unavailable("no udev".into()),
            access_hint: Some("Use flatpak override".into()),
            ..Status::default()
        };
        assert_eq!(status_line(&prefs, &unavailable, FOCUSED).1, Tone::Error);
        assert_eq!(device_note(&prefs, &unavailable), "Use flatpak override");
    }

    #[test]
    fn identical_controllers_get_distinct_entries() {
        let status = running(vec![pad("k", 1), pad("k#2", 2)]);
        let prefs = Prefs {
            device: Some("k#2".into()),
            ..Prefs::default()
        };
        let model = model(&prefs, &status, FOCUSED);
        let options = model.properties["device-options"].as_array().unwrap();
        assert_eq!(options.len(), 3);
        assert_eq!(options[0], "All controllers");
        assert_eq!(
            options[2],
            "DualSense Wireless Controller (2) · battery 80%"
        );
        assert_eq!(model.properties["selected-device-index"], 2);
        assert_eq!(model.device_keys[2].as_deref(), Some("k#2"));
    }

    #[test]
    fn tuning_rows_carry_their_ranges() {
        let model = model(&Prefs::default(), &Status::default(), FOCUSED);
        let rows = model.properties["tuning-rows"].as_array().unwrap();
        assert_eq!(rows.len(), TUNING_ROWS.len());
        let left = &rows[0];
        assert_eq!(left["key"], "left-dead-zone");
        assert_eq!(left["value"], 0.16);
        assert_eq!(left["text"], "0.16");
        assert_eq!(left["max"], 0.9);
        assert_eq!(model.properties["pointer-row"]["key"], "pointer-speed");
    }

    #[test]
    fn mapping_rows_index_into_the_option_lists() {
        let model = model(&Prefs::default(), &Status::default(), FOCUSED);
        let axes = model.properties["axis-mappings"].as_array().unwrap();
        let buttons = model.properties["button-mappings"].as_array().unwrap();
        assert_eq!(axes.len() + buttons.len(), Control::COUNT);
        let options = model.properties["axis-action-options"].as_array().unwrap();
        let left_x = &axes[0];
        assert_eq!(left_x["control-key"], "left_stick_x");
        let index = left_x["current-index"].as_u64().unwrap() as usize;
        assert_eq!(options[index], Action::PanHorizontal.label());
    }

    #[test]
    fn the_pointer_note_explains_the_opt_in() {
        let mut prefs = Prefs::default();
        let status = Status::default();
        assert!(pointer_note(&prefs, &status).starts_with("Off."));
        prefs
            .config
            .mappings
            .set(Control::RightStickX, Action::PointerHorizontal);
        assert!(pointer_note(&prefs, &status).contains("Turn this on"));
        prefs.pointer_control = true;
        let failed = Status {
            pointer: PointerState::Failed("no portal".into()),
            ..Status::default()
        };
        assert_eq!(pointer_note(&prefs, &failed), "Not available: no portal");
    }

    #[test]
    fn the_active_profile_is_selected() {
        let mut prefs = Prefs::default();
        for name in ["b", "A", "c"] {
            prefs.profiles.insert(name.into(), prefs.config.clone());
        }
        prefs.active_profile = Some("b".into());
        let model = model(&prefs, &Status::default(), FOCUSED);
        assert_eq!(model.properties["profile-options"], json!(["A", "b", "c"]));
        assert_eq!(model.properties["selected-profile-index"], 1);
        prefs.active_profile = None;
        let model = super::model(&prefs, &Status::default(), FOCUSED);
        assert_eq!(model.properties["selected-profile-index"], -1);
    }
}
