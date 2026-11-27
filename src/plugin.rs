//! The [`Plugin`] implementation: host events, the settings window and its
//! callbacks.
//!
//! Event handlers and UI callbacks run on the host's UI thread and only touch
//! memory; file reads and writes run on short-lived threads, settings are
//! written by the input thread (debounced), and no lock is held while calling
//! the host (GPD-1, GPD-5).
//!
//! Every text the user reads is made with `tr!`, in the language of the
//! interface; after a language change the plugin hands eov its texts again.

use crate::backend::{BackendContext, GilrsBackend, InputBackend};
use crate::config::{
    Action, Config, Control, MAX_PROFILES, PersistedState, Prefs, ProfileFile, STATE_KEY,
    TuningKey, export_file_name, normalize_profile_name, unique_profile_name,
};
use crate::legacy;
use crate::shared::{Persist, Shared, TOKEN_REFRESH, TOKEN_TOGGLE_WINDOW, lock};
use crate::ui::{self, Focus};
use crate::worker::{self, Worker, paused_status};
use eov_plugin_api::i18n::isolate;
use eov_plugin_api::prelude::*;
use eov_plugin_api::protocol::{FileDialogArgs, FileFilter, event_names};
use std::collections::HashMap;
use std::io::Read;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The toolbar button that shows and hides the settings window.
pub const BUTTON_ID: &str = "toggle_gamepad";
const ICON: &str = include_str!("../ui/icons/gamepad.svg");
/// Profile files larger than this are not imported.
const IMPORT_LIMIT: u64 = 1024 * 1024;

/// A request waiting for the user's answer.
enum Pending {
    DeleteProfile(String),
    OverwriteProfile { name: String, config: Config },
    Import,
    Export { name: String, config: Config },
}

#[derive(Default)]
struct UiState {
    window: Option<Surface>,
    /// What each entry of the device and profile lists selects.
    device_keys: Vec<Option<String>>,
    profile_keys: Vec<Option<String>>,
    /// Confirmations and file dialogs by request id.
    pending: HashMap<String, Pending>,
}

/// The gamepad plugin. `B` is the input backend (real controllers by
/// default; tests script input).
pub struct GamepadPlugin<B: InputBackend = GilrsBackend> {
    host: Host,
    shared: Arc<Shared>,
    ui: Mutex<UiState>,
    worker: Mutex<Option<Worker>>,
    persister: Mutex<Option<JoinHandle<()>>>,
    _backend: PhantomData<fn() -> B>,
}

/// The settings writer gets this long to write what is pending at shutdown.
const PERSIST_STOP: Duration = Duration::from_secs(2);

fn arg_bool(args: &[Value], index: usize) -> Option<bool> {
    args.get(index)?.as_bool()
}

fn arg_str(args: &[Value], index: usize) -> Option<&str> {
    args.get(index)?.as_str()
}

fn arg_f64(args: &[Value], index: usize) -> Option<f64> {
    args.get(index)?.as_f64().filter(|v| v.is_finite())
}

/// Slint ints arrive as JSON numbers, possibly with a fraction part.
fn arg_index(args: &[Value], index: usize) -> Option<usize> {
    let value = arg_f64(args, index)?;
    (value >= 0.0).then_some(value as usize)
}

fn json_filter() -> Vec<FileFilter> {
    vec![FileFilter {
        // Translators: the type of file in the file dialog
        name: tr!("Gamepad profile"),
        extensions: vec!["json".to_string()],
    }]
}

/// The toolbar button's tooltip.
fn button_tooltip() -> String {
    // Translators: the tooltip of the toolbar button, and the title of
    // the gamepad settings window
    tr!("Gamepad")
}

/// Writes through a temporary file in the same directory, so an interrupted
/// write never leaves a truncated file.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other(tr!("the path has no file name")))?;
    let temp = path.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

fn read_profile(path: &Path) -> Result<ProfileFile, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut text = String::new();
    file.take(IMPORT_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > IMPORT_LIMIT {
        return Err(tr!("the file is too large to be a gamepad profile"));
    }
    ProfileFile::parse(&text)
}

impl<B: InputBackend> GamepadPlugin<B> {
    fn focus(&self) -> Focus {
        Focus {
            main_window: self.shared.main_focused(),
            settings_window: self.shared.settings_active(),
        }
    }

    fn model(&self) -> ui::Model {
        let status = self.shared.status();
        let focus = self.focus();
        self.shared
            .read_prefs(|prefs| ui::model(prefs, &status, focus))
    }

    fn window(&self) -> Option<Surface> {
        lock(&self.ui).window.clone()
    }

    fn is_window(&self, surface_id: &str) -> bool {
        lock(&self.ui)
            .window
            .as_ref()
            .is_some_and(|w| w.id() == surface_id)
    }

    /// Pushes every property; the host applies only the changed ones.
    fn push_ui(&self) {
        self.shared.refresh_started();
        let Some(window) = self.window() else {
            return;
        };
        let model = self.model();
        {
            let mut ui = lock(&self.ui);
            ui.device_keys = model.device_keys;
            ui.profile_keys = model.profile_keys;
        }
        let _ = window.set_many(model.properties);
    }

    /// Sets a property to what the user just entered and then to `value`.
    /// The host skips values equal to the last one *it* applied, which may
    /// not be what the field shows after the user edited it.
    fn reset_field(&self, name: &str, entered: Value, value: Value) {
        if let Some(window) = self.window() {
            let _ = window.set(name, entered);
            let _ = window.set(name, value);
        }
    }

    fn toggle_window(&self) {
        if let Some(window) = self.window()
            && window.is_open()
        {
            let _ = window.close();
            self.window_closed();
            return;
        }
        self.open_window();
    }

    fn open_window(&self) {
        self.shared.set_window_open(true);
        let model = self.model();
        let mut spec = SurfaceSpec::new(SurfaceKind::Window, ui::UI_PATH, ui::COMPONENT)
            .with_title(ui::title())
            .with_size(ui::WIDTH, Some(ui::HEIGHT));
        spec.min_width = Some(380.0);
        spec.min_height = Some(360.0);
        spec.properties = model.properties;
        match self.host.open_surface(spec) {
            Ok(surface) => {
                {
                    let mut ui = lock(&self.ui);
                    ui.window = Some(surface);
                    ui.device_keys = model.device_keys;
                    ui.profile_keys = model.profile_keys;
                }
                let _ = self.host.set_toolbar_button_active(BUTTON_ID, true);
            }
            Err(error) => {
                self.shared.set_window_open(false);
                self.host
                    .notify_error(tr!("Cannot open the gamepad window"), error.message);
            }
        }
    }

    fn window_closed(&self) {
        lock(&self.ui).window = None;
        self.shared.set_window_open(false);
        let _ = self.host.set_toolbar_button_active(BUTTON_ID, false);
    }

    /// The user chose another language and the SDK switched `tr!` to it:
    /// hands eov the texts it keeps showing again. The window's own texts
    /// (`@tr`) follow by themselves; its properties carry texts made here.
    fn language_changed(&self) {
        let _ = self.host.update_toolbar_button(ButtonUpdate {
            button_id: BUTTON_ID.to_string(),
            tooltip: Some(button_tooltip()),
            ..Default::default()
        });
        if !self.shared.gate().enabled {
            self.host.set_status(Some(&paused_status()));
            // The controller may have resumed (and cleared the text) since.
            if self.shared.gate().enabled {
                self.host.set_status(None);
            }
        }
        // The access hint the input thread keeps is made again, and pushed
        // when it is ready.
        self.shared.language_changed();
        self.push_ui();
    }

    fn warn(&self, message: impl Into<String>) {
        self.host.notify(NotifyLevel::Warning, message);
    }

    // -- profiles -------------------------------------------------------------------

    /// Entry `index` of the profile list: a profile, or "no profile" (the
    /// settings stay, edits are no longer saved into a profile).
    fn select_profile(&self, index: usize) {
        let Some(key) = lock(&self.ui).profile_keys.get(index).cloned() else {
            return;
        };
        let Some(name) = key else {
            self.shared
                .update(Persist::Now, |prefs| prefs.active_profile = None);
            return;
        };
        let loaded = self.shared.update(Persist::Now, |prefs| {
            let Some(config) = prefs.profiles.get(&name).cloned() else {
                return false;
            };
            prefs.config = config;
            prefs.active_profile = Some(name.clone());
            true
        });
        if !loaded {
            // Translators: {} is the name of a profile
            self.warn(tr!("The profile “{}” no longer exists.", isolate(&name)));
        }
    }

    /// Saves the current settings (or the defaults) as a profile.
    fn save_profile_as(&self, entered: &str, defaults: bool) {
        let name = if entered.trim().is_empty() {
            // Translators: the name of a new profile when none was typed
            let untitled = tr!("Profile name" => "Untitled");
            self.shared
                .read_prefs(|p| unique_profile_name(&untitled, |n| p.profiles.contains_key(n)))
        } else {
            match normalize_profile_name(entered) {
                Ok(name) => name,
                Err(message) => {
                    self.warn(message);
                    return;
                }
            }
        };
        let config = if defaults {
            Config::default()
        } else {
            self.shared.read_prefs(|p| p.config.clone())
        };
        let (exists, full) = self.shared.read_prefs(|p| {
            (
                p.profiles.contains_key(&name),
                p.profiles.len() >= MAX_PROFILES,
            )
        });
        if exists {
            // Never overwrite a profile just because its name was typed
            // (GPD-8).
            let quoted = isolate(&name);
            let message = if defaults {
                // Translators: {} is the name of a profile
                tr!(
                    "A profile named “{}” already exists. Replace it with the default settings?",
                    quoted
                )
            } else {
                // Translators: {} is the name of a profile
                tr!(
                    "A profile named “{}” already exists. Replace it with the current settings?",
                    quoted
                )
            };
            let request = self.host.confirm(ConfirmArgs {
                title: tr!("Replace profile?"),
                message,
                // Translators: a button: replaces the profile
                confirm_label: tr!("Replace"),
                cancel_label: tr!("Cancel"),
                destructive: true,
            });
            match request {
                Ok(id) => {
                    lock(&self.ui)
                        .pending
                        .insert(id, Pending::OverwriteProfile { name, config });
                }
                Err(error) => self.warn(error.message),
            }
        } else if full {
            self.warn(tr!(
                "You already have {n} profile. Delete one to save another."
                    | "You already have {n} profiles. Delete one to save another." % MAX_PROFILES
            ));
            return;
        } else {
            self.store_profile(name, config);
        }
        self.reset_field("profile-name", json!(entered), json!(""));
    }

    fn store_profile(&self, name: String, config: Config) {
        self.shared.update(Persist::Now, |prefs| {
            prefs.profiles.insert(name.clone(), config.clone());
            prefs.config = config;
            prefs.active_profile = Some(name);
        });
    }

    fn delete_profile(&self) {
        let Some(name) = self.shared.read_prefs(|p| p.active_profile.clone()) else {
            self.warn(tr!("Choose a saved profile to delete."));
            return;
        };
        match self.host.confirm(ConfirmArgs {
            title: tr!("Delete profile?"),
            // Translators: {} is the name of a profile
            message: tr!(
                "Delete the profile “{}”? This cannot be undone.",
                isolate(&name)
            ),
            // Translators: a button: deletes the profile
            confirm_label: tr!("Delete"),
            cancel_label: tr!("Cancel"),
            destructive: true,
        }) {
            Ok(id) => {
                lock(&self.ui)
                    .pending
                    .insert(id, Pending::DeleteProfile(name));
            }
            Err(error) => self.warn(error.message),
        }
    }

    fn import_profile(&self) {
        let request = self.host.open_file_dialog(FileDialogArgs {
            title: Some(tr!("Import a gamepad profile")),
            filters: json_filter(),
            ..Default::default()
        });
        match request {
            Ok(id) => {
                lock(&self.ui).pending.insert(id, Pending::Import);
            }
            Err(error) => self.warn(error.message),
        }
    }

    fn export_profile(&self) {
        let (name, config) = self.shared.read_prefs(|p| {
            (
                p.active_profile.clone().unwrap_or_else(|| {
                    // Translators: the name of an exported profile when none is selected
                    tr!("Profile name" => "Gamepad profile")
                }),
                p.config.clone(),
            )
        });
        let request = self.host.save_file_dialog(FileDialogArgs {
            title: Some(tr!("Export the gamepad profile")),
            filters: json_filter(),
            default_name: Some(export_file_name(&name)),
            ..Default::default()
        });
        match request {
            Ok(id) => {
                lock(&self.ui)
                    .pending
                    .insert(id, Pending::Export { name, config });
            }
            Err(error) => self.warn(error.message),
        }
    }

    fn answered(&self, request_id: &str, confirmed: bool) {
        let pending = lock(&self.ui).pending.remove(request_id);
        match pending {
            Some(Pending::DeleteProfile(name)) if confirmed => {
                self.shared.update(Persist::Now, |prefs| {
                    prefs.profiles.remove(&name);
                    if prefs.active_profile.as_deref() == Some(name.as_str()) {
                        prefs.active_profile = None;
                    }
                });
            }
            Some(Pending::OverwriteProfile { name, config }) if confirmed => {
                self.store_profile(name, config);
            }
            _ => {}
        }
        self.push_ui();
    }

    fn files_chosen(&self, request_id: &str, paths: Vec<String>) {
        let pending = lock(&self.ui).pending.remove(request_id);
        let Some(path) = paths.into_iter().next().map(PathBuf::from) else {
            return;
        };
        match pending {
            Some(Pending::Import) => self.spawn_import(path),
            Some(Pending::Export { name, config }) => self.spawn_export(path, name, config),
            _ => {}
        }
    }

    /// Reads the file off the UI thread, then adds the profile.
    fn spawn_import(&self, path: PathBuf) {
        let shared = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new()
            .name("gamepad-import".to_string())
            .spawn(move || {
                let host = &shared.host;
                let profile = match read_profile(&path) {
                    Ok(profile) => profile,
                    Err(error) => {
                        host.notify_error(
                            // Translators: {} is the path of a file
                            tr!("Cannot import {}", isolate(path.display())),
                            error,
                        );
                        return;
                    }
                };
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .and_then(|s| normalize_profile_name(&s).ok())
                    .unwrap_or_else(|| {
                        // Translators: the name of an imported profile when its file name
                        // cannot be used
                        tr!("Profile name" => "Imported")
                    });
                let config = profile.to_config();
                let name = shared.update(Persist::Now, |prefs| {
                    if prefs.profiles.len() >= MAX_PROFILES {
                        return None;
                    }
                    let name = unique_profile_name(&stem, |n| prefs.profiles.contains_key(n));
                    prefs.profiles.insert(name.clone(), config.clone());
                    prefs.config = config;
                    prefs.active_profile = Some(name.clone());
                    Some(name)
                });
                match name {
                    Some(name) => {
                        host.notify(
                            NotifyLevel::Success,
                            // Translators: {} is the name of a profile
                            tr!("Imported the profile “{}”", isolate(&name)),
                        );
                    }
                    None => host.notify(
                        NotifyLevel::Warning,
                        tr!(
                            "You already have {n} profile. Delete one to import another."
                                | "You already have {n} profiles. Delete one to import another."
                                    % MAX_PROFILES
                        ),
                    ),
                }
                shared.request_refresh();
            });
        if let Err(error) = spawned {
            self.warn(error.to_string());
        }
    }

    /// Writes the file off the UI thread.
    fn spawn_export(&self, mut path: PathBuf, name: String, config: Config) {
        if path.extension().is_none() {
            path.set_extension("json");
        }
        let host = self.host.clone();
        let spawned = std::thread::Builder::new()
            .name("gamepad-export".to_string())
            .spawn(move || {
                let text = match serde_json::to_string_pretty(&ProfileFile::from(&config)) {
                    Ok(text) => text,
                    Err(error) => {
                        host.notify_error(tr!("Cannot export the profile"), error.to_string());
                        return;
                    }
                };
                match write_atomically(&path, text.as_bytes()) {
                    Ok(()) => host.notify(
                        NotifyLevel::Success,
                        // Translators: {name} is the name of a profile, {path} a file
                        tr!(
                            "Exported “{name}” to {path}",
                            name = isolate(&name),
                            path = isolate(path.display())
                        ),
                    ),
                    Err(error) => host.notify_error(
                        // Translators: {} is the path of a file
                        tr!("Cannot write {}", isolate(path.display())),
                        error.to_string(),
                    ),
                }
            });
        if let Err(error) = spawned {
            self.warn(error.to_string());
        }
    }

    // -- settings -----------------------------------------------------------------------

    fn set_tuning(&self, key: TuningKey, value: impl FnOnce(f32) -> f32) {
        self.shared.update(Persist::Soon, |prefs| {
            prefs.edit_config(|config| {
                let next = value(config.tuning.get(key));
                config.tuning.set(key, next);
            });
        });
    }

    fn set_mapping(&self, control: Control, action: Action) {
        self.shared.update(Persist::Now, |prefs| {
            prefs.edit_config(|config| {
                config.mappings.set(control, action);
            });
        });
    }

    fn select_device(&self, index: usize) {
        let key = lock(&self.ui).device_keys.get(index).cloned();
        if let Some(key) = key {
            self.shared.update(Persist::Now, |prefs| prefs.device = key);
            // The input thread republishes the effective selection.
        }
        self.reset_selection("device", index);
    }

    /// After a combo box choice: what the user picked, then what is selected
    /// now (the same unless the choice was refused). See [`Self::reset_field`].
    fn reset_selection(&self, list: &str, picked: usize) {
        let model = self.model();
        let property = |name: String| model.properties.get(&name).cloned().unwrap_or(Value::Null);
        let options = property(format!("{list}-options"));
        let index_name = format!("selected-{list}-index");
        let label_name = format!("selected-{list}-label");
        let picked_label = options.get(picked).cloned().unwrap_or(json!(""));
        self.reset_field(&index_name, json!(picked), property(index_name.clone()));
        self.reset_field(&label_name, picked_label, property(label_name.clone()));
    }

    /// Stops the threads; waits only for the settings writer to save what is
    /// pending (a single small write).
    fn stop_threads(&self) {
        let worker = lock(&self.worker).take();
        if let Some(mut worker) = worker {
            worker.stop(&self.shared);
        }
        self.shared.stop_persister();
        let persister = lock(&self.persister).take();
        if let Some(persister) = persister {
            let deadline = Instant::now() + PERSIST_STOP;
            while !persister.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            if persister.is_finished() {
                let _ = persister.join();
            }
        }
    }
}

impl<B: InputBackend> Plugin for GamepadPlugin<B> {
    fn create(host: Host, init: PluginInit) -> Result<Self, String> {
        let stored = init.settings.get(STATE_KEY);
        let (persisted, warnings) = PersistedState::from_value(stored);
        let mut prefs = Prefs::from_persisted(persisted);
        // First start of 1.0: keep what 0.x saved (read-only).
        let imported = match (stored, legacy::dir(init.legacy_config_dir.as_deref())) {
            (None, Some(dir)) => {
                let legacy = legacy::read(&dir);
                let found = (!legacy.is_empty()).then_some(legacy.profiles.len());
                legacy.apply(&mut prefs);
                found
            }
            _ => None,
        };
        let enabled = prefs.enabled;
        // The focus at start-up; afterwards WindowFocusChanged keeps it.
        let focused = host.snapshot().map_or(true, |s| s.window_focused);
        let shared = Arc::new(Shared::new(host.clone(), prefs, focused));
        if let Some(profiles) = imported {
            // Write the import once, so it is not repeated.
            shared.update(Persist::Now, |_| {});
            host.notify(
                NotifyLevel::Info,
                match profiles {
                    0 => tr!("Imported the gamepad settings of the previous plugin version"),
                    n => tr!(
                        "Imported the gamepad settings and {n} profile of the previous plugin version"
                            | "Imported the gamepad settings and {n} profiles of the previous plugin version"
                            % n
                    ),
                },
            );
        }
        if !warnings.is_empty() {
            shared.notify_once(
                "settings",
                NotifyLevel::Warning,
                &tr!("Some gamepad settings could not be read and use their defaults"),
                Some(warnings.join("; ")),
            );
        }
        host.add_toolbar_button(ToolbarButtonSpec {
            button_id: BUTTON_ID.to_string(),
            tooltip: button_tooltip(),
            icon_svg: ICON.to_string(),
            ..Default::default()
        })
        .map_err(String::from)?;
        // The only optional event the plugin needs (LanguageChanged needs no
        // subscription).
        let _ = host.subscribe(&[event_names::WINDOW_FOCUS_CHANGED]);
        if !enabled {
            host.set_status(Some(&paused_status()));
        }
        // The errors `create` returns are in English: eov logs them and
        // writes them to the audit log as they are, and shows them to the
        // user inside its own translated message.
        let persister = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("gamepad-settings".to_string())
                .spawn(move || shared.run_persister())
                .map_err(|e| format!("cannot start the settings writer: {e}"))?
        };
        let worker = Worker::spawn::<B>(
            Arc::clone(&shared),
            BackendContext {
                instance: init.data_dir.clone(),
            },
        );
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                shared.stop_persister();
                return Err(format!("cannot start the input thread: {error}"));
            }
        };
        Ok(Self {
            host,
            shared,
            ui: Mutex::new(UiState::default()),
            worker: Mutex::new(Some(worker)),
            persister: Mutex::new(Some(persister)),
            _backend: PhantomData,
        })
    }

    fn on_event(&self, event: HostEvent) -> Result<(), String> {
        match event {
            HostEvent::Ready { .. } => {
                if let Ok(snapshot) = self.host.snapshot() {
                    self.shared.focus_event(snapshot.window_focused);
                }
            }
            HostEvent::WindowFocusChanged { focused } => {
                self.shared.focus_event(focused);
                self.push_ui();
            }
            HostEvent::ToolbarButtonClicked { button_id } if button_id == BUTTON_ID => {
                self.toggle_window();
            }
            HostEvent::SurfaceClosed { surface_id } if self.is_window(&surface_id) => {
                self.window_closed();
            }
            HostEvent::Posted { token } if token == TOKEN_REFRESH => self.push_ui(),
            HostEvent::Posted { token } if token == TOKEN_TOGGLE_WINDOW => self.toggle_window(),
            HostEvent::ConfirmResult {
                request_id,
                confirmed,
            } => self.answered(&request_id, confirmed),
            HostEvent::DialogResult { request_id, paths } => self.files_chosen(&request_id, paths),
            HostEvent::LanguageChanged { .. } => self.language_changed(),
            HostEvent::Shutdown => self.stop_threads(),
            _ => {}
        }
        Ok(())
    }

    fn ui_callback(&self, surface_id: &str, name: &str, args: &[Value]) -> Result<Value, String> {
        if !self.is_window(surface_id) {
            return Ok(Value::Null);
        }
        match name {
            "enabled-toggled" => {
                if let Some(enabled) = arg_bool(args, 0) {
                    worker::set_enabled(&self.shared, enabled, false);
                }
            }
            "device-selected" => {
                if let Some(index) = arg_index(args, 0) {
                    self.select_device(index);
                }
            }
            "profile-selected" => {
                if let Some(index) = arg_index(args, 0) {
                    self.select_profile(index);
                    self.reset_selection("profile", index);
                }
            }
            "save-profile-as" => self.save_profile_as(arg_str(args, 0).unwrap_or(""), false),
            "new-profile" => self.save_profile_as(arg_str(args, 0).unwrap_or(""), true),
            "import-profile" => self.import_profile(),
            "export-profile" => self.export_profile(),
            "delete-profile" => self.delete_profile(),
            "tuning-changed" => {
                if let (Some(key), Some(value)) = (
                    arg_str(args, 0).and_then(TuningKey::from_key),
                    arg_f64(args, 1),
                ) {
                    self.set_tuning(key, |_| value as f32);
                }
            }
            "tuning-step" => {
                if let (Some(key), Some(direction)) = (
                    arg_str(args, 0).and_then(TuningKey::from_key),
                    arg_f64(args, 1).filter(|d| *d != 0.0),
                ) {
                    let step = key.range().step * direction.signum() as f32;
                    self.set_tuning(key, |current| current + step);
                }
            }
            "tuning-reset" => {
                if let Some(key) = arg_str(args, 0).and_then(TuningKey::from_key) {
                    self.set_tuning(key, |_| key.range().default);
                }
            }
            "pointer-control-toggled" => {
                if let Some(on) = arg_bool(args, 0) {
                    self.shared
                        .update(Persist::Now, |prefs| prefs.pointer_control = on);
                }
            }
            "mapping-selected" => {
                if let (Some(control), Some(index)) = (
                    arg_str(args, 0).and_then(Control::from_key),
                    arg_index(args, 1),
                ) && let Some(action) = Action::options(control.is_axis()).get(index)
                {
                    self.set_mapping(control, *action);
                }
            }
            "window-active-changed" => {
                if let Some(active) = arg_bool(args, 0) {
                    self.shared.set_settings_active(active);
                }
            }
            _ => return Ok(Value::Null),
        }
        self.push_ui();
        Ok(Value::Null)
    }

    fn shutdown(&self) {
        // Usually done on Shutdown already. After a revoke (the plugin was
        // disabled) a pending write fails quietly.
        self.stop_threads();
    }
}
