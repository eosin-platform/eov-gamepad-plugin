//! One-time import of the settings written by the 0.x plugin.
//!
//! 0.x kept its live configuration in `state.json` and its profiles in
//! `profiles/*.json` under `plugins/gamepad/` of eov's per-user folder before
//! 1.0 (`<platform config dir>/eov`, which the host passes as
//! `PluginInit::legacy_config_dir`). 1.0 stores everything in the host's
//! settings store instead. On the first
//! start without 1.0 settings these files are read (never written or
//! deleted), so upgrading keeps the user's tuning and profiles. Mappings that
//! moved the system pointer are kept, but pointer control itself starts off:
//! the user opts in again (GPD-2).

use crate::config::{Config, MAX_PROFILES, Prefs, ProfileFile, normalize_profile_name};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Largest file read (0.x profiles are about 1 KiB).
const MAX_FILE: u64 = 256 * 1024;

/// What a 0.x installation left behind.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Legacy {
    pub config: Option<Config>,
    pub profiles: BTreeMap<String, Config>,
}

impl Legacy {
    pub fn is_empty(&self) -> bool {
        self.config.is_none() && self.profiles.is_empty()
    }

    /// Applies the import to fresh preferences.
    pub fn apply(self, prefs: &mut Prefs) {
        if let Some(config) = self.config {
            prefs.config = config;
        }
        prefs.profiles.extend(self.profiles);
    }
}

/// Where 0.x stored its files, given the host's `legacy_config_dir`. The
/// host knows that folder (and redirects it when eov runs isolated, as in
/// tests), so the plugin does not guess it from the environment.
pub fn dir(legacy_config_dir: Option<&str>) -> Option<PathBuf> {
    let root = Path::new(legacy_config_dir?);
    root.is_absolute()
        .then(|| root.join("plugins").join("gamepad"))
}

fn read_profile(path: &Path) -> Option<Config> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    ProfileFile::parse(&text).ok().map(|file| file.to_config())
}

/// Reads a 0.x settings directory; unreadable files are skipped.
pub fn read(dir: &Path) -> Legacy {
    let mut legacy = Legacy {
        config: read_profile(&dir.join("state.json")),
        ..Legacy::default()
    };
    let Ok(entries) = std::fs::read_dir(dir.join("profiles")) else {
        return legacy;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        if legacy.profiles.len() >= MAX_PROFILES {
            break;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| normalize_profile_name(stem).ok())
        else {
            continue;
        };
        if let Some(config) = read_profile(&path) {
            legacy.profiles.entry(name).or_insert(config);
        }
    }
    legacy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Action, Control};

    /// A temporary 0.x configuration directory, removed again when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("eov-gamepad-legacy-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("profiles")).unwrap();
            Self(dir)
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const PROFILE: &str = r#"{
        "left_dead_zone": 0.3, "right_dead_zone": 0.12, "trigger_dead_zone": 0.08,
        "pan_sensitivity": 1.5, "zoom_sensitivity": 0.78,
        "mappings": { "right_stick_x": "mouse_horizontal", "south": "fit_view" }
    }"#;

    #[test]
    fn state_and_profiles_are_read() {
        let dir = TempDir::new("full");
        std::fs::write(dir.join("state.json"), PROFILE).unwrap();
        std::fs::write(dir.join("profiles/Desk.json"), PROFILE).unwrap();
        std::fs::write(dir.join("profiles/broken.json"), "{").unwrap();
        std::fs::write(dir.join("profiles/notes.txt"), "x").unwrap();
        std::fs::write(dir.join("profiles/ .json"), PROFILE).unwrap();
        let legacy = read(&dir);
        let config = legacy.config.clone().unwrap();
        assert_eq!(config.tuning.left_dead_zone, 0.3);
        assert_eq!(config.mappings.get(Control::South), Action::FitView);
        assert_eq!(legacy.profiles.keys().collect::<Vec<_>>(), vec!["Desk"]);
        // Pointer mappings survive; pointer control stays off.
        let mut prefs = Prefs::default();
        legacy.apply(&mut prefs);
        assert!(prefs.config.mappings.uses_pointer());
        assert!(!prefs.pointer_control);
        assert_eq!(prefs.profiles["Desk"].tuning.pan_sensitivity, 1.5);
        assert_eq!(prefs.active_profile, None);
    }

    #[test]
    fn nothing_there_is_nothing_imported() {
        let dir = TempDir::new("empty");
        assert!(read(&dir).is_empty());
        assert!(read(&dir.join("missing")).is_empty());
    }

    #[test]
    fn the_folder_comes_from_the_host() {
        assert_eq!(super::dir(None), None);
        assert_eq!(super::dir(Some("relative/eov")), None);
        let root = std::env::temp_dir().join("eov");
        assert_eq!(
            super::dir(root.to_str()),
            Some(root.join("plugins").join("gamepad"))
        );
    }
}
