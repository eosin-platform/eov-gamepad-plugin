//! The plugin in another language, with a catalog read from its
//! `translations` folder as eov's SDK reads it.
//!
//! The language of `tr!` belongs to the whole library, so this runs in a
//! process of its own (an integration test), and as one test: the unit tests
//! stay in English.

use eov_plugin_api::prelude::*;
use eov_plugin_api::testing::{MockHost, TestHarness, test_init};
use gamepad::GamepadPlugin;
use gamepad::backend::{BackendContext, BackendError, InputBackend};
use gamepad::config::STATE_KEY;
use gamepad::engine::InputEvent;
use gamepad::plugin::BUTTON_ID;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A backend without controllers.
struct NoInput;

impl InputBackend for NoInput {
    fn open(_context: &BackendContext) -> Result<Self, BackendError> {
        Ok(Self)
    }

    fn wait(&mut self, timeout: Duration, _events: &mut Vec<InputEvent>) {
        std::thread::sleep(timeout);
    }

    fn refresh(&mut self, _events: &mut Vec<InputEvent>) {}
}

/// A few German translations; every other text stays in English.
const GERMAN: &str = r#"msgid ""
msgstr ""
"Project-Id-Version: gamepad\n"
"Language: de\n"
"MIME-Version: 1.0\n"
"Content-Type: text/plain; charset=UTF-8\n"
"Content-Transfer-Encoding: 8bit\n"
"Plural-Forms: nplurals=2; plural=(n != 1);\n"

msgid "Gamepad"
msgstr "Gamecontroller"

msgid "Gamepad paused"
msgstr "Gamecontroller pausiert"

msgid "No profile"
msgstr "Kein Profil"

msgid "Pan Horizontally"
msgstr "Horizontal verschieben"

msgid "Left Dead Zone"
msgstr "Totzone links"
"#;

/// A plugin folder with the real manifest and the German catalog.
fn plugin_root(root: &Path) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin.toml");
    std::fs::copy(manifest, root.join("plugin.toml")).unwrap();
    let folder = root.join("translations").join("de").join("LC_MESSAGES");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("gamepad.po"), GERMAN).unwrap();
}

struct Rig {
    harness: TestHarness<GamepadPlugin<NoInput>>,
    root: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Rig {
    fn tooltip(&self) -> String {
        self.harness.state().toolbar_buttons[BUTTON_ID]
            .tooltip
            .clone()
    }

    fn status(&self) -> Option<String> {
        self.harness.state().status.clone()
    }

    fn property(&self, name: &str) -> Value {
        let state = self.harness.state();
        let surface = state
            .surfaces
            .values()
            .find(|s| s.open)
            .expect("the window");
        surface.properties[name].clone()
    }

    fn switch_to(&self, language: &str) {
        self.harness
            .send(HostEvent::LanguageChanged {
                language: language.into(),
            })
            .unwrap();
    }
}

#[test]
fn texts_follow_the_language_of_the_interface() {
    let mut init = test_init("gamepad");
    let root = PathBuf::from(&init.plugin_root);
    plugin_root(&root);
    init.language = Some("de".into());
    // Paused, so that the plugin shows a status-bar text.
    init.settings
        .insert(STATE_KEY.into(), json!({ "version": 1, "enabled": false }));
    let harness = TestHarness::with_host(MockHost::new(), init).unwrap();
    let rig = Rig { harness, root };

    // German from the start.
    assert_eq!(rig.tooltip(), "Gamecontroller");
    assert_eq!(rig.status().as_deref(), Some("Gamecontroller pausiert"));
    rig.harness
        .send(HostEvent::ToolbarButtonClicked {
            button_id: BUTTON_ID.into(),
        })
        .unwrap();
    assert_eq!(rig.property("profile-options")[0], "Kein Profil");
    assert_eq!(rig.property("tuning-rows")[0]["label"], "Totzone links");
    // German writes decimals with a comma; the value itself is a number.
    assert_eq!(rig.property("tuning-rows")[0]["text"], "0,16");
    assert_eq!(rig.property("tuning-rows")[0]["value"], json!(0.16));
    let actions = rig.property("axis-action-options");
    assert_eq!(actions[1], "Horizontal verschieben");
    // Texts the catalog lacks are shown in English.
    assert_eq!(actions[0], "Unassigned");
    assert_eq!(
        rig.property("axis-mappings")[0]["current-label"],
        actions[1]
    );

    // The user switches to English: every text is handed over again.
    rig.switch_to("en");
    assert_eq!(rig.tooltip(), "Gamepad");
    assert_eq!(rig.status().as_deref(), Some("Gamepad paused"));
    assert_eq!(rig.property("profile-options")[0], "No profile");
    assert_eq!(rig.property("selected-profile-label"), "No profile");
    assert_eq!(rig.property("tuning-rows")[0]["text"], "0.16");
    assert_eq!(
        rig.property("axis-mappings")[0]["current-label"],
        "Pan Horizontally"
    );

    // And back.
    rig.switch_to("de");
    assert_eq!(rig.tooltip(), "Gamecontroller");
    assert_eq!(rig.property("selected-profile-label"), "Kein Profil");
    assert_eq!(
        rig.property("axis-action-options")[1],
        "Horizontal verschieben"
    );
}
