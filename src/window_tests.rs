//! The settings window on Slint's testing backend, fed the plugin's own
//! properties the way eov applies them to plugin surfaces: only values that
//! changed, arrays patched row by row. Covers what the harness cannot see:
//! what the combo boxes show, and when the window reports itself active
//! (input then acts while it has focus, GPD-2).

use crate::config::{Action, Control, Prefs};
use crate::engine::{DeviceInfo, Power};
use crate::shared::{BackendState, Status};
use crate::ui::{self, Focus};
use i_slint_backend_testing::ElementHandle;
use i_slint_compiler::langtype::Type;
use serde_json::{Map, Value as Json};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use slint_interpreter::{ComponentInstance, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

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

type Calls = Rc<RefCell<Vec<(String, Vec<Value>)>>>;

/// A window fed like eov feeds plugin surfaces (`host_ui/surfaces.rs`).
struct Surface {
    instance: ComponentInstance,
    types: HashMap<String, Type>,
    applied: HashMap<String, Json>,
    models: HashMap<String, Rc<VecModel<Value>>>,
    calls: Calls,
}

impl Surface {
    fn new(properties: &Map<String, Json>) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut compiler = slint_interpreter::Compiler::default();
        compiler.set_include_paths(vec![root.to_path_buf()]);
        let result = spin_on(compiler.build_from_path(root.join(ui::UI_PATH)));
        let definition = result
            .component(ui::COMPONENT)
            .expect("the window compiles");
        let mut types = HashMap::new();
        for (name, (ty, _)) in definition.properties_and_callbacks() {
            if ty.is_property_type() {
                types.insert(name.to_string(), ty);
            }
        }
        let instance = definition.create().unwrap();
        let calls: Calls = Rc::default();
        for name in ui::CALLBACKS {
            let calls = Rc::clone(&calls);
            instance
                .set_callback(name, move |args| {
                    calls.borrow_mut().push((name.to_string(), args.to_vec()));
                    Value::Void
                })
                .unwrap();
        }
        let mut surface = Self {
            instance,
            types,
            applied: HashMap::new(),
            models: HashMap::new(),
            calls,
        };
        surface.set_many(properties);
        // Tall enough that every row is on screen: the element search skips
        // what a window would clip.
        surface
            .instance
            .window()
            .set_size(slint::LogicalSize::new(860.0, 3000.0));
        surface.instance.show().unwrap();
        tick();
        surface
    }

    fn set_many(&mut self, properties: &Map<String, Json>) {
        for (name, json) in properties {
            if self.applied.get(name) == Some(json) {
                continue;
            }
            let ty = self.types[name].clone();
            if let (Type::Array(inner), Json::Array(rows)) = (&ty, json) {
                let values: Vec<Value> = rows
                    .iter()
                    .map(|row| slint_interpreter::json::value_from_json(inner, row).unwrap())
                    .collect();
                match self.models.get(name) {
                    Some(model) => {
                        let common = model.row_count().min(values.len());
                        for (i, value) in values.iter().enumerate().take(common) {
                            if model.row_data(i).as_ref() != Some(value) {
                                model.set_row_data(i, value.clone());
                            }
                        }
                        while model.row_count() > values.len() {
                            model.remove(model.row_count() - 1);
                        }
                        for value in values.into_iter().skip(common) {
                            model.push(value);
                        }
                    }
                    None => {
                        let model = Rc::new(VecModel::from(values));
                        self.instance
                            .set_property(name, Value::Model(ModelRc::from(Rc::clone(&model))))
                            .unwrap();
                        self.models.insert(name.clone(), model);
                    }
                }
            } else {
                let value = slint_interpreter::json::value_from_json(&ty, json).unwrap();
                self.instance.set_property(name, value).unwrap();
            }
            self.applied.insert(name.clone(), json.clone());
        }
        tick();
    }

    fn combos(&self) -> Vec<ElementHandle> {
        ElementHandle::find_by_element_type_name(&self.instance, "ComboBox").collect()
    }

    fn shown(&self, combo: usize) -> String {
        self.combos()[combo]
            .accessible_value()
            .unwrap_or_default()
            .to_string()
    }

    fn calls(&self, name: &str) -> Vec<Vec<Value>> {
        self.calls
            .borrow()
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, args)| args.clone())
            .collect()
    }

    /// The last activity the window reported (inactive until it says so).
    fn active(&self) -> bool {
        self.calls("window-active-changed")
            .last()
            .is_some_and(|args| args[0] == Value::Bool(true))
    }

    fn activate(&self, active: bool) {
        self.instance
            .window()
            .dispatch_event(WindowEvent::WindowActiveChanged(active));
        tick();
    }

    fn key(&self, key: Key) {
        let window = self.instance.window();
        window.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        tick();
    }

    fn name_field(&self) -> ElementHandle {
        ElementHandle::find_by_accessible_label(&self.instance, "Profile name")
            .next()
            .expect("the name field")
    }
}

fn tick() {
    slint::platform::update_timers_and_animations();
}

/// Lets mock time pass (timers fire).
fn advance(ms: u64) {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(ms));
    tick();
}

fn pad(power: Power) -> DeviceInfo {
    DeviceInfo {
        id: 1,
        key: "pad".into(),
        name: "Test Pad".into(),
        occurrence: 1,
        power,
    }
}

fn status(power: Power) -> Status {
    Status {
        backend: BackendState::Running,
        devices: vec![pad(power)],
        ..Status::default()
    }
}

fn model(prefs: &Prefs, status: &Status) -> Map<String, Json> {
    ui::model(prefs, status, Focus::default()).properties
}

/// Device combo, profile combo, then one per mapping row.
const DEVICE: usize = 0;
const PROFILE: usize = 1;
const FIRST_MAPPING: usize = 2;

fn combo_boxes_show_what_the_plugin_pushes() {
    let mut prefs = Prefs::default();
    prefs.profiles.insert("Lab".into(), prefs.config.clone());
    prefs.active_profile = Some("Lab".into());
    prefs.device = Some("pad".into());
    let mut status = status(Power::Discharging(90));
    let mut surface = Surface::new(&model(&prefs, &status));
    assert_eq!(surface.combos().len(), FIRST_MAPPING + Control::COUNT);
    assert_eq!(surface.shown(PROFILE), "Lab");
    assert_eq!(surface.shown(DEVICE), "Test Pad · battery 90%");
    assert_eq!(surface.shown(FIRST_MAPPING), Action::PanHorizontal.label());

    // Save As "Desk": it sorts before "Lab" and is selected at the same
    // index, so only the rows and the label change.
    prefs.profiles.insert("Desk".into(), prefs.config.clone());
    prefs.active_profile = Some("Desk".into());
    surface.set_many(&model(&prefs, &status));
    assert_eq!(surface.shown(PROFILE), "Desk");

    // A battery update patches the selected row in place.
    status.devices[0].power = Power::Discharging(80);
    surface.set_many(&model(&prefs, &status));
    assert_eq!(surface.shown(DEVICE), "Test Pad · battery 80%");

    // A mapping changed elsewhere (a loaded profile) shows in its row.
    prefs.config.mappings.set(Control::LeftStickX, Action::Zoom);
    surface.set_many(&model(&prefs, &status));
    assert_eq!(surface.shown(FIRST_MAPPING), Action::Zoom.label());
}

fn the_window_reports_activity_conservatively() {
    let mut prefs = Prefs::default();
    prefs.profiles.insert("Lab".into(), prefs.config.clone());
    let surface = Surface::new(&model(&prefs, &status(Power::Unknown)));
    // Shown but never activated: not active.
    assert!(!surface.active());
    surface.activate(true);
    assert!(surface.active());
    surface.activate(false);
    assert!(!surface.active());
    surface.activate(true);
    assert!(surface.active());

    // Typing in the name field of the active window keeps it active ...
    surface
        .name_field()
        .mock_single_click(PointerEventButton::Left);
    assert!(surface.active());
    // ... until the window is deactivated; activation reaches it again
    // after the focus returned to the window.
    surface.activate(false);
    assert!(!surface.active());
    advance(200);
    surface.activate(true);
    assert!(surface.active());
    // Moving between editors keeps it active, also as time passes.
    surface.combos()[PROFILE].mock_single_click(PointerEventButton::Left);
    surface.key(Key::Escape);
    surface
        .name_field()
        .mock_single_click(PointerEventButton::Left);
    advance(500);
    assert!(surface.active());

    // Input reaching an inactive window (a background click, assistive
    // technology) never makes it active.
    surface.activate(false);
    advance(200);
    let reported = surface.calls("window-active-changed").len();
    surface
        .name_field()
        .mock_single_click(PointerEventButton::Left);
    surface.combos()[PROFILE].mock_single_click(PointerEventButton::Left);
    advance(200);
    surface.combos()[FIRST_MAPPING].invoke_accessible_expand_action();
    advance(200);
    let later = surface.calls("window-active-changed");
    assert!(
        later[reported..]
            .iter()
            .all(|args| args[0] == Value::Bool(false)),
        "{later:?}"
    );
    assert!(!surface.active());
}

fn combo_boxes_keep_the_keyboard_focus() {
    let surface = Surface::new(&model(&Prefs::default(), &status(Power::Unknown)));
    surface.activate(true);
    // Open a mapping's menu, close it again: the combo box keeps the focus
    // and arrow keys step through the actions.
    surface.combos()[FIRST_MAPPING].mock_single_click(PointerEventButton::Left);
    assert!(surface.active());
    surface.key(Key::Escape);
    surface.key(Key::DownArrow);
    surface.key(Key::DownArrow);
    let picks = surface.calls("mapping-selected");
    assert_eq!(picks.len(), 2, "{picks:?}");
    assert_eq!(picks[0][0], Value::String("left_stick_x".into()));
    assert_ne!(picks[0][1], picks[1][1]);
    // Choosing from the menu still counts as using the active window.
    assert!(surface.active());
}

/// One test: the testing platform can be installed once per process.
#[test]
fn the_window_behaves() {
    i_slint_backend_testing::init_no_event_loop();
    combo_boxes_show_what_the_plugin_pushes();
    the_window_reports_activity_conservatively();
    combo_boxes_keep_the_keyboard_focus();
}
