//! Explains why no controller can be seen (GPD-3).
//!
//! The eov Flatpak used to grant only `--device=dri`, which hides
//! `/dev/input` inside the sandbox, so gilrs cannot open any controller.
//! Instead of a generic "unavailable", the plugin tells the user what to
//! change.

use std::path::Path;

/// The Flatpak application id when running inside a Flatpak sandbox.
pub fn flatpak_app() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    if let Ok(id) = std::env::var("FLATPAK_ID")
        && !id.is_empty()
    {
        return Some(id);
    }
    let info = std::fs::read_to_string("/.flatpak-info").ok()?;
    Some(flatpak_app_from_info(&info).unwrap_or_else(|| "io.eosin.eov".to_string()))
}

/// The `name` of the `[Application]` group of a `/.flatpak-info` file.
fn flatpak_app_from_info(info: &str) -> Option<String> {
    let mut in_application = false;
    for line in info.lines().map(str::trim) {
        if line.starts_with('[') {
            in_application = line == "[Application]";
        } else if in_application && let Some(name) = line.strip_prefix("name=") {
            return Some(name.trim().to_string()).filter(|name| !name.is_empty());
        }
    }
    None
}

/// A hint for when controllers cannot be read, if the cause is known.
pub fn access_hint() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    access_hint_at(Path::new("/dev/input"), flatpak_app().as_deref())
}

/// The device nodes of game controllers in an input directory: udev's
/// `by-id/*-event-joystick` links and the joydev nodes (`js0`, …). Keyboard
/// and mouse nodes are normally unreadable for users, which says nothing
/// about controllers.
fn joystick_nodes(input_dir: &Path, names: &[String]) -> Vec<std::path::PathBuf> {
    let mut nodes: Vec<_> = std::fs::read_dir(input_dir.join("by-id"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with("-event-joystick")
        })
        .map(|entry| entry.path())
        .collect();
    nodes.extend(
        names
            .iter()
            .filter(|name| {
                name.strip_prefix("js")
                    .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            })
            .map(|name| input_dir.join(name)),
    );
    nodes
}

/// [`access_hint`] for an input directory; `flatpak` is the app id when
/// sandboxed.
pub fn access_hint_at(input_dir: &Path, flatpak: Option<&str>) -> Option<String> {
    let names: Vec<String> = match std::fs::read_dir(input_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => {
            return Some(match flatpak {
                Some(app) => flatpak_message(app),
                None => format!(
                    "{} is not available, so no controller can be opened.",
                    input_dir.display()
                ),
            });
        }
    };
    let joysticks = joystick_nodes(input_dir, &names);
    if joysticks.is_empty() {
        // Nothing connected; in a sandbox without any input device, the
        // devices are hidden instead.
        let hidden = !names.iter().any(|name| name.starts_with("event"));
        return flatpak.filter(|_| hidden).map(flatpak_message);
    }
    if joysticks
        .iter()
        .any(|node| std::fs::File::open(node).is_ok())
    {
        return None;
    }
    Some(match flatpak {
        Some(app) => flatpak_message(app),
        None => "A game controller is connected, but eov may not read it. udev normally \
                 gives the logged-in user access to controllers (the uaccess tag); if \
                 yours is not recognised as a joystick, add a udev rule that tags it \
                 with uaccess, then reconnect it."
            .to_string(),
    })
}

fn flatpak_message(app: &str) -> String {
    format!(
        "eov runs as a Flatpak without access to input devices. Allow it by running \
         flatpak override --user --device=input {app} (Flatpak 1.15.6 or newer; \
         older versions need --device=all), then restart eov."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory, removed again when dropped.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("eov-gamepad-env-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_hidden_input_directory_in_a_flatpak_explains_the_override() {
        let dir = TempDir::new("missing");
        let missing = dir.0.join("input");
        let hint = access_hint_at(&missing, Some("io.eosin.eov")).unwrap();
        assert!(hint.contains("--device=input io.eosin.eov"), "{hint}");
        assert!(!hint.contains('`'), "{hint}");
        // Outside a sandbox the message names the directory instead.
        let hint = access_hint_at(&missing, None).unwrap();
        assert!(hint.contains("is not available"), "{hint}");
        // An empty directory is only suspicious in a sandbox.
        let empty = TempDir::new("empty");
        assert!(access_hint_at(&empty.0, None).is_none());
        assert!(access_hint_at(&empty.0, Some("x.y")).is_some());
    }

    #[test]
    fn the_flatpak_app_id_is_read_from_the_info_file() {
        let info = "[Application]\nname=io.eosin.eov\nruntime=x\n\n[Instance]\nname=nope\n";
        assert_eq!(flatpak_app_from_info(info).as_deref(), Some("io.eosin.eov"));
        assert_eq!(flatpak_app_from_info("[Instance]\nname=x\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn only_controller_nodes_matter() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("nodes");
        let input = &dir.0;
        // Keyboards and mice: root:input 0660, unreadable for users.
        for name in ["event0", "event1"] {
            let node = input.join(name);
            std::fs::write(&node, b"").unwrap();
            std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        // No controller connected: no hint, even without access to them.
        assert!(access_hint_at(input, None).is_none());
        assert!(access_hint_at(input, Some("io.eosin.eov")).is_none());
        // A readable controller: no hint either.
        let pad = input.join("event7");
        std::fs::write(&pad, b"").unwrap();
        std::fs::create_dir_all(input.join("by-id")).unwrap();
        std::os::unix::fs::symlink(&pad, input.join("by-id/usb-Pad-event-joystick")).unwrap();
        assert!(access_hint_at(input, None).is_none());
        // An unreadable controller explains the udev tag, not group changes.
        std::fs::set_permissions(&pad, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&pad).is_err() {
            let hint = access_hint_at(input, None).unwrap();
            assert!(hint.contains("uaccess"), "{hint}");
            assert!(!hint.contains("input' group"), "{hint}");
        }
        for name in ["event0", "event1", "event7"] {
            let _ =
                std::fs::set_permissions(input.join(name), std::fs::Permissions::from_mode(0o600));
        }
    }

    #[test]
    fn joydev_nodes_count_as_controllers() {
        let dir = TempDir::new("joydev");
        std::fs::write(dir.0.join("js0"), b"").unwrap();
        std::fs::write(dir.0.join("jsx"), b"").unwrap();
        let names = vec!["js0".to_string(), "jsx".to_string()];
        assert_eq!(joystick_nodes(&dir.0, &names), vec![dir.0.join("js0")]);
    }
}
