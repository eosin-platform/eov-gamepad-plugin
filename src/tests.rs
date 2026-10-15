//! Plugin-level tests: the settings window contract and the plugin driven
//! through `TestHarness` with scripted controller input.

use crate::backend::scripted::{FailingBackend, ScriptedBackend, script};
use crate::config::{Control, PersistedState, Prefs, STATE_KEY};
use crate::engine::{DeviceInfo, InputEvent, Power};
use crate::plugin::{BUTTON_ID, GamepadPlugin};
use crate::shared::{BackendState, Status, TOKEN_TOGGLE_WINDOW};
use crate::ui::{self, Focus};
use crate::worker::PAUSED_STATUS;
use eov_plugin_api::prelude::*;
use eov_plugin_api::testing::{MockHost, TestHarness, test_init};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// The window contract
// ---------------------------------------------------------------------------

mod contract {
    use super::*;
    use i_slint_compiler::langtype::Type;
    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;

    fn spin_on<T>(future: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Waker};
        let mut cx = Context::from_waker(Waker::noop());
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    /// Compiles the window the way the host does (same Slint version, the
    /// plugin root as include path).
    fn compile() -> slint_interpreter::ComponentDefinition {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut compiler = slint_interpreter::Compiler::default();
        compiler.set_include_paths(vec![root.to_path_buf()]);
        let result = spin_on(compiler.build_from_path(root.join(ui::UI_PATH)));
        let problems: Vec<String> = result.diagnostics().map(|d| d.to_string()).collect();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
        result
            .component(ui::COMPONENT)
            .expect("the window component is exported")
    }

    fn sample_status() -> Status {
        Status {
            backend: BackendState::Running,
            devices: vec![
                DeviceInfo {
                    id: 1,
                    key: "a".into(),
                    name: "Xbox Wireless Controller".into(),
                    occurrence: 1,
                    power: Power::Charging(40),
                },
                DeviceInfo {
                    id: 2,
                    key: "a#2".into(),
                    name: "Xbox Wireless Controller".into(),
                    occurrence: 2,
                    power: Power::Wired,
                },
            ],
            ..Status::default()
        }
    }

    #[test]
    fn the_window_compiles_and_matches_the_plugin() {
        let definition = compile();
        let mut properties = HashMap::new();
        let mut callbacks = BTreeSet::new();
        for (name, (ty, _)) in definition.properties_and_callbacks() {
            match ty {
                Type::Callback(_) => {
                    callbacks.insert(name.to_string());
                }
                ty if ty.is_property_type() => {
                    properties.insert(name.to_string(), ty);
                }
                _ => {}
            }
        }
        let mut prefs = Prefs::default();
        prefs.profiles.insert("Desk".into(), prefs.config.clone());
        prefs.active_profile = Some("Desk".into());
        let model = ui::model(&prefs, &sample_status(), Focus::default());
        // Every pushed value exists and converts like the host converts it.
        let mut pushed: Vec<(String, Value)> = model.properties.into_iter().collect();
        pushed.push(("profile-name".into(), json!("cleared")));
        for (name, value) in &pushed {
            let ty = properties
                .get(name)
                .unwrap_or_else(|| panic!("the window has no property '{name}'"));
            if let Err(error) = slint_interpreter::json::value_from_json(ty, value) {
                panic!("property '{name}' rejects {value}: {error}");
            }
        }
        // The plugin handles exactly the declared callbacks.
        let handled: BTreeSet<String> = ui::CALLBACKS.iter().map(|s| s.to_string()).collect();
        assert_eq!(callbacks, handled);
    }
}

// ---------------------------------------------------------------------------
// The plugin through the harness
// ---------------------------------------------------------------------------

type Plugin = GamepadPlugin<ScriptedBackend>;

const PAD: u64 = 1;

struct Rig {
    harness: TestHarness<Plugin>,
    input: Sender<InputEvent>,
}

fn mock_host(focused: bool) -> MockHost {
    let host = MockHost::new();
    {
        let mut state = host.state();
        state.snapshot.window_focused = focused;
        state.snapshot.focused_pane = Some(3);
        state.snapshot.panes.push(PaneInfo {
            pane: 3,
            index: 0,
            file_id: Some(1),
            view: Some(ViewInfo {
                pane: 3,
                file_id: 1,
                zoom: 1.0,
                width: 1200.0,
                height: 800.0,
                ..Default::default()
            }),
        });
    }
    host
}

fn rig_with(init: PluginInit, focused: bool) -> Rig {
    let input = script(&init.data_dir);
    let harness = TestHarness::<Plugin>::with_host(mock_host(focused), init).unwrap();
    input
        .send(InputEvent::Connected(DeviceInfo {
            id: PAD,
            key: "pad".into(),
            name: "Test Pad".into(),
            occurrence: 1,
            power: Power::Unknown,
        }))
        .unwrap();
    Rig { harness, input }
}

fn rig() -> Rig {
    rig_with(test_init("gamepad"), true)
}

/// Waits until `condition` holds (the input thread runs concurrently).
fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Gives the input thread time to (not) react.
fn settle() {
    std::thread::sleep(Duration::from_millis(300));
}

impl Rig {
    fn calls(&self, method: &str) -> Vec<Value> {
        self.harness.calls_to(method)
    }

    fn press(&self, control: Control) {
        for pressed in [true, false] {
            self.input
                .send(InputEvent::Button {
                    device: PAD,
                    control,
                    pressed,
                })
                .unwrap();
        }
        // Keep presses apart from the debounce window.
        std::thread::sleep(Duration::from_millis(60));
    }

    fn axis(&self, control: Control, value: f32) {
        self.input
            .send(InputEvent::Axis {
                device: PAD,
                control,
                value,
            })
            .unwrap();
    }

    fn open_window(&self) -> String {
        self.harness
            .send(HostEvent::ToolbarButtonClicked {
                button_id: BUTTON_ID.into(),
            })
            .unwrap();
        let surfaces = &self.harness.state().surfaces;
        let (id, surface) = surfaces
            .iter()
            .find(|(_, s)| s.open)
            .expect("the window opened");
        assert_eq!(surface.spec.kind, SurfaceKind::Window);
        id.clone()
    }

    fn property(&self, surface: &str, name: &str) -> Value {
        self.harness.state().surfaces[surface]
            .properties
            .get(name)
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn callback(&self, surface: &str, name: &str, args: Vec<Value>) {
        self.harness.ui_callback(surface, name, args).unwrap();
    }

    /// The id the mock host gave the latest dialog request (it numbers
    /// surfaces and requests from one counter).
    fn last_request(&self) -> String {
        let issued: usize = [
            "surface.open",
            "dialog.confirm",
            "dialog.open_file",
            "dialog.save_file",
            "job.start",
        ]
        .iter()
        .map(|method| self.calls(method).len())
        .sum();
        format!("request-{issued}")
    }

    fn stored_state(&self) -> Option<PersistedState> {
        let value = self.harness.state().settings.get(STATE_KEY).cloned()?;
        Some(PersistedState::from_value(Some(&value)).0)
    }
}

#[test]
fn creation_registers_the_button_and_focus_events_only() {
    let rig = rig();
    let state = rig.harness.state();
    let button = &state.toolbar_buttons[BUTTON_ID];
    assert!(!button.active);
    assert!(button.icon_svg.contains("<svg"));
    assert_eq!(
        state.calls_to("events.subscribe"),
        vec![json!({ "events": ["window_focus_changed"] })]
    );
    // Nothing touches the view, and no window opens by itself.
    assert!(state.surfaces.is_empty());
    assert!(state.calls_to("view.set").is_empty());
}

#[test]
fn the_toolbar_button_toggles_the_window() {
    let rig = rig();
    let id = rig.open_window();
    let spec = rig.harness.state().surfaces[&id].spec.clone();
    assert_eq!(spec.ui_path, "ui/gamepad-window.slint");
    assert_eq!(spec.component, "GamepadWindow");
    assert_eq!(spec.title.as_deref(), Some("Gamepad"));
    assert!(spec.properties.contains_key("axis-mappings"));
    assert!(rig.harness.state().toolbar_buttons[BUTTON_ID].active);
    // Second click closes it.
    rig.harness
        .send(HostEvent::ToolbarButtonClicked {
            button_id: BUTTON_ID.into(),
        })
        .unwrap();
    assert!(!rig.harness.state().surfaces[&id].open);
    assert!(!rig.harness.state().toolbar_buttons[BUTTON_ID].active);
    // Closed by the user: the button follows.
    let id = rig.open_window();
    rig.harness.state().surfaces.get_mut(&id).unwrap().open = false;
    rig.harness
        .send(HostEvent::SurfaceClosed {
            surface_id: id.clone(),
        })
        .unwrap();
    assert!(!rig.harness.state().toolbar_buttons[BUTTON_ID].active);
    // Callbacks from a closed window are ignored.
    rig.callback(&id, "enabled-toggled", vec![json!(false)]);
    assert!(rig.calls("status").is_empty());
}

#[test]
fn buttons_drive_navigation_through_the_host() {
    let rig = rig();
    rig.press(Control::DPadDown);
    rig.press(Control::DPadRight);
    rig.press(Control::North);
    rig.press(Control::East);
    eventually("the actions", || !rig.calls("view.fit").is_empty());
    assert_eq!(rig.calls("app.cycle_tab"), vec![json!({ "offset": 1 })]);
    assert_eq!(rig.calls("app.cycle_pane"), vec![json!({ "offset": 1 })]);
    assert_eq!(rig.calls("app.series"), vec![json!({ "action": "toggle" })]);
    assert_eq!(rig.calls("view.fit"), vec![json!({ "pane": null })]);
}

#[test]
fn sticks_pan_relatively_and_stop_on_disconnect() {
    let rig = rig();
    rig.axis(Control::LeftStickX, 1.0);
    eventually("panning", || rig.calls("view.pan").len() >= 3);
    for call in rig.calls("view.pan") {
        // Relative, for whichever pane is focused when applied (GPD-4).
        assert_eq!(call["pane"], Value::Null);
        assert!(call["dx"].as_f64().unwrap() > 0.0);
    }
    assert!(rig.calls("view.set").is_empty(), "no read-modify-write");
    // The pad goes away with the stick still deflected.
    rig.input.send(InputEvent::Disconnected(PAD)).unwrap();
    settle();
    let count = rig.calls("view.pan").len();
    settle();
    assert_eq!(rig.calls("view.pan").len(), count, "panning continued");
}

#[test]
fn triggers_zoom_the_focused_pane() {
    let rig = rig();
    rig.axis(Control::RightTrigger, 1.0);
    eventually("zooming", || !rig.calls("view.zoom").is_empty());
    let call = &rig.calls("view.zoom")[0];
    assert_eq!(call["pane"], Value::Null);
    assert!(call["factor"].as_f64().unwrap() > 1.0);
    rig.axis(Control::RightTrigger, 0.0);
}

#[test]
fn input_is_ignored_while_eov_is_in_the_background() {
    let rig = rig();
    rig.harness
        .send(HostEvent::WindowFocusChanged { focused: false })
        .unwrap();
    rig.press(Control::DPadDown);
    rig.axis(Control::LeftStickX, 1.0);
    settle();
    assert!(rig.calls("app.cycle_tab").is_empty());
    assert!(rig.calls("view.pan").is_empty());
    // Input asked the host to republish its focus.
    assert!(!rig.calls("render.request").is_empty());
    // Back in front: the held stick pans again.
    rig.harness
        .send(HostEvent::WindowFocusChanged { focused: true })
        .unwrap();
    eventually("panning after refocus", || {
        !rig.calls("view.pan").is_empty()
    });
    rig.axis(Control::LeftStickX, 0.0);
}

#[test]
fn the_settings_window_counts_as_focus() {
    let rig = rig();
    let id = rig.open_window();
    rig.harness
        .send(HostEvent::WindowFocusChanged { focused: false })
        .unwrap();
    rig.callback(&id, "window-active-changed", vec![json!(true)]);
    rig.press(Control::DPadUp);
    eventually("the tab change", || !rig.calls("app.cycle_tab").is_empty());
    // Leaving it (to another application) stops input again.
    rig.callback(&id, "window-active-changed", vec![json!(false)]);
    rig.press(Control::DPadUp);
    settle();
    assert_eq!(rig.calls("app.cycle_tab").len(), 1);
}

#[test]
fn starting_unfocused_gates_input_until_focus_arrives() {
    let rig = rig_with(test_init("gamepad"), false);
    rig.press(Control::DPadDown);
    settle();
    assert!(rig.calls("app.cycle_tab").is_empty());
}

#[test]
fn pausing_blocks_every_action_but_resume() {
    let rig = rig();
    let id = rig.open_window();
    rig.callback(&id, "enabled-toggled", vec![json!(false)]);
    assert_eq!(rig.harness.state().status.as_deref(), Some(PAUSED_STATUS));
    assert_eq!(rig.property(&id, "controller-enabled"), json!(false));
    rig.press(Control::DPadDown);
    rig.axis(Control::LeftStickX, 1.0);
    settle();
    assert!(rig.calls("app.cycle_tab").is_empty());
    assert!(rig.calls("view.pan").is_empty());
    rig.axis(Control::LeftStickX, 0.0);
    // Start (Enable / Pause) still resumes.
    rig.press(Control::Start);
    eventually("resuming", || rig.harness.state().status.is_none());
    let notices = rig.harness.state().notifications.clone();
    assert!(notices.iter().any(|n| n.message == "Gamepad input resumed"));
    // The pause state survives a restart.
    rig.press(Control::Start);
    eventually("pausing", || rig.harness.state().status.is_some());
    eventually("the write", || {
        rig.stored_state().is_some_and(|state| !state.enabled)
    });
}

#[test]
fn the_window_action_posts_to_the_ui_thread() {
    let rig = rig();
    rig.press(Control::Select);
    eventually("the post", || {
        rig.harness
            .state()
            .posted
            .contains(&TOKEN_TOGGLE_WINDOW.to_string())
    });
    assert!(rig.harness.state().surfaces.is_empty());
    rig.harness
        .send(HostEvent::Posted {
            token: TOKEN_TOGGLE_WINDOW.into(),
        })
        .unwrap();
    assert_eq!(rig.harness.state().surfaces.len(), 1);
}

#[test]
fn slider_drags_update_the_window_now_and_disk_later() {
    let rig = rig();
    let id = rig.open_window();
    for step in 0..20 {
        let value = 0.5 + step as f64 * 0.05;
        rig.callback(
            &id,
            "tuning-changed",
            vec![json!("pan-sensitivity"), json!(value)],
        );
    }
    let rows = rig.property(&id, "tuning-rows");
    let pan = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key"] == "pan-sensitivity")
        .unwrap()
        .clone();
    assert_eq!(pan["value"], json!(1.45));
    assert_eq!(pan["text"], "1.45");
    // Nothing written on the UI thread; one debounced write later.
    assert!(rig.calls("settings.set").is_empty());
    eventually("the write", || !rig.calls("settings.set").is_empty());
    settle();
    let writes = rig.calls("settings.set");
    assert!(writes.len() <= 2, "{} writes", writes.len());
    assert_eq!(rig.stored_state().unwrap().config.pan_sensitivity, 1.45);
    // Out-of-range input is clamped to the slider range.
    rig.callback(
        &id,
        "tuning-changed",
        vec![json!("left-dead-zone"), json!(5.0)],
    );
    rig.callback(
        &id,
        "tuning-step",
        vec![json!("zoom-sensitivity"), json!(1)],
    );
    rig.callback(&id, "tuning-reset", vec![json!("pan-sensitivity")]);
    let rows = rig.property(&id, "tuning-rows");
    let value = |key: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["key"] == key)
            .unwrap()["value"]
            .clone()
    };
    assert_eq!(value("left-dead-zone"), json!(0.9));
    assert_eq!(value("zoom-sensitivity"), json!(0.83));
    assert_eq!(value("pan-sensitivity"), json!(0.95));
}

#[test]
fn mappings_change_from_the_window() {
    let rig = rig();
    let id = rig.open_window();
    // West: index 9 of the button actions is Fit View.
    let options = rig.property(&id, "button-action-options");
    let fit = options
        .as_array()
        .unwrap()
        .iter()
        .position(|o| o == "Fit View")
        .unwrap();
    rig.callback(&id, "mapping-selected", vec![json!("west"), json!(fit)]);
    rig.press(Control::West);
    eventually("the fit", || !rig.calls("view.fit").is_empty());
    // Unknown controls and out-of-range indices are ignored.
    rig.callback(&id, "mapping-selected", vec![json!("nope"), json!(1)]);
    rig.callback(&id, "mapping-selected", vec![json!("west"), json!(99)]);
    let rows = rig.property(&id, "button-mappings");
    let west = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["control-key"] == "west")
        .unwrap()
        .clone();
    assert_eq!(west["current-index"], json!(fit));
}

#[test]
fn profiles_never_overwrite_without_asking() {
    let rig = rig();
    let id = rig.open_window();
    rig.callback(&id, "save-profile-as", vec![json!("  Xbox Elite  ")]);
    assert_eq!(rig.property(&id, "profile-options"), json!(["Xbox Elite"]));
    assert_eq!(rig.property(&id, "selected-profile-index"), json!(0));
    // The name field is cleared after saving.
    assert_eq!(rig.property(&id, "profile-name"), json!(""));
    // Edits go to the active profile only.
    rig.callback(
        &id,
        "tuning-changed",
        vec![json!("zoom-sensitivity"), json!(2.0)],
    );
    // Saving under an existing name asks first.
    rig.callback(&id, "new-profile", vec![json!("Xbox Elite")]);
    let confirms = rig.calls("dialog.confirm");
    assert_eq!(confirms.len(), 1);
    assert_eq!(confirms[0]["destructive"], json!(true));
    // Declined: nothing changed.
    rig.harness
        .send(HostEvent::ConfirmResult {
            request_id: rig.last_request(),
            confirmed: false,
        })
        .unwrap();
    let rows = rig.property(&id, "tuning-rows");
    let zoom = rows.as_array().unwrap()[4]["value"].clone();
    assert_eq!(zoom, json!(2.0));
    // Path-like names are just names.
    rig.callback(&id, "save-profile-as", vec![json!("../../etc/passwd")]);
    assert_eq!(
        rig.property(&id, "profile-options"),
        json!(["../../etc/passwd", "Xbox Elite"])
    );
    rig.callback(&id, "profile-selected", vec![json!("Xbox Elite")]);
    assert_eq!(rig.property(&id, "selected-profile-index"), json!(1));
    // Deleting asks too.
    rig.callback(&id, "delete-profile", vec![]);
    assert_eq!(rig.calls("dialog.confirm").len(), 2);
    rig.harness
        .send(HostEvent::ConfirmResult {
            request_id: rig.last_request(),
            confirmed: true,
        })
        .unwrap();
    assert_eq!(
        rig.property(&id, "profile-options"),
        json!(["../../etc/passwd"])
    );
    assert_eq!(rig.property(&id, "selected-profile-index"), json!(-1));
    // Bad names are refused with a message.
    rig.callback(&id, "save-profile-as", vec![json!("a\u{7}b")]);
    let notices = rig.harness.state().notifications.clone();
    assert!(
        notices
            .iter()
            .any(|n| n.message.contains("control characters"))
    );
}

#[test]
fn profiles_export_and_import_files() {
    let rig = rig();
    let id = rig.open_window();
    rig.callback(
        &id,
        "tuning-changed",
        vec![json!("pan-sensitivity"), json!(2.5)],
    );
    let dir = std::env::temp_dir().join(format!(
        "eov-gamepad-files-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // Export.
    rig.callback(&id, "export-profile", vec![]);
    let dialog = &rig.calls("dialog.save_file")[0];
    assert_eq!(dialog["default_name"], "Gamepad profile.json");
    let target = dir.join("exported");
    rig.harness
        .send(HostEvent::DialogResult {
            request_id: rig.last_request(),
            paths: vec![target.to_string_lossy().into_owned()],
        })
        .unwrap();
    let written = dir.join("exported.json");
    eventually("the export", || written.exists());
    let exported: Value =
        serde_json::from_str(&std::fs::read_to_string(&written).unwrap()).unwrap();
    assert_eq!(exported["pan_sensitivity"], json!(2.5));
    assert_eq!(exported["version"], json!(1));
    // Import a profile written by 0.2.x.
    let legacy = dir.join("Desk Pad.json");
    std::fs::write(
        &legacy,
        r#"{"left_dead_zone":0.2,"right_dead_zone":0.1,"trigger_dead_zone":0.1,
            "pan_sensitivity":1.0,"zoom_sensitivity":1.0,
            "mappings":{"left_stick_x":"pan_horizontal"}}"#,
    )
    .unwrap();
    rig.callback(&id, "import-profile", vec![]);
    assert_eq!(rig.calls("dialog.open_file").len(), 1);
    rig.harness
        .send(HostEvent::DialogResult {
            request_id: rig.last_request(),
            paths: vec![legacy.to_string_lossy().into_owned()],
        })
        .unwrap();
    eventually("the import", || {
        rig.stored_state()
            .is_some_and(|state| state.profiles.contains_key("Desk Pad"))
    });
    let state = rig.stored_state().unwrap();
    assert_eq!(state.active_profile.as_deref(), Some("Desk Pad"));
    assert_eq!(state.config.left_dead_zone, 0.2);
    // Cancelled dialogs do nothing.
    rig.callback(&id, "import-profile", vec![]);
    rig.harness
        .send(HostEvent::DialogResult {
            request_id: rig.last_request(),
            paths: vec![],
        })
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn device_choice_is_persisted() {
    let rig = rig();
    rig.input
        .send(InputEvent::Connected(DeviceInfo {
            id: 2,
            key: "pad#2".into(),
            name: "Test Pad".into(),
            occurrence: 2,
            power: Power::Discharging(55),
        }))
        .unwrap();
    let id = rig.open_window();
    eventually("both pads listed", || {
        rig.harness
            .send(HostEvent::Posted {
                token: crate::shared::TOKEN_REFRESH.into(),
            })
            .unwrap();
        rig.property(&id, "device-options")
            .as_array()
            .is_some_and(|o| o.len() == 3)
    });
    assert_eq!(
        rig.property(&id, "device-options")[2],
        json!("Test Pad (2) · battery 55%")
    );
    rig.callback(&id, "device-selected", vec![json!(2)]);
    eventually("the write", || {
        rig.stored_state()
            .is_some_and(|state| state.device.as_deref() == Some("pad#2"))
    });
    // Only the chosen pad acts now.
    rig.press(Control::DPadDown);
    settle();
    assert!(rig.calls("app.cycle_tab").is_empty());
    rig.input
        .send(InputEvent::Button {
            device: 2,
            control: Control::DPadDown,
            pressed: true,
        })
        .unwrap();
    eventually("the second pad's press", || {
        !rig.calls("app.cycle_tab").is_empty()
    });
}

#[test]
fn stored_settings_are_restored() {
    let mut init = test_init("gamepad");
    init.settings.insert(
        STATE_KEY.into(),
        json!({
            "version": 1,
            "enabled": false,
            "config": { "pan_sensitivity": 2.0 },
            "profiles": { "Lab": {} },
            "active_profile": "Lab",
            "pointer_control": 3
        }),
    );
    let rig = rig_with(init, true);
    // Paused at start-up, shown in the status bar.
    assert_eq!(rig.harness.state().status.as_deref(), Some(PAUSED_STATUS));
    // The unreadable field was reported once.
    let notices = rig.harness.state().notifications.clone();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].message.contains("could not be read"));
    let id = rig.open_window();
    assert_eq!(rig.property(&id, "profile-options"), json!(["Lab"]));
    assert_eq!(rig.property(&id, "pointer-control"), json!(false));
}

#[test]
fn shutdown_flushes_pending_changes() {
    let rig = rig();
    let id = rig.open_window();
    rig.callback(
        &id,
        "tuning-changed",
        vec![json!("pan-sensitivity"), json!(1.5)],
    );
    rig.harness.send(HostEvent::Shutdown).unwrap();
    assert_eq!(rig.stored_state().unwrap().config.pan_sensitivity, 1.5);
}

#[test]
fn an_unavailable_input_subsystem_is_explained() {
    let init = test_init("gamepad");
    let harness =
        TestHarness::<GamepadPlugin<FailingBackend>>::with_host(mock_host(true), init).unwrap();
    eventually("the notification", || {
        !harness.state().notifications.is_empty()
    });
    let notice = harness.state().notifications[0].clone();
    assert!(
        notice.message.contains("not available"),
        "{}",
        notice.message
    );
    assert!(notice.message.contains("a hint for the test"));
    harness
        .send(HostEvent::ToolbarButtonClicked {
            button_id: BUTTON_ID.into(),
        })
        .unwrap();
    let state = harness.state();
    let surface = state.surfaces.values().next().unwrap();
    assert!(
        surface.properties["status-text"]
            .as_str()
            .unwrap()
            .starts_with("Controllers are unavailable")
    );
    assert_eq!(surface.properties["device-note"], "a hint for the test");
}
