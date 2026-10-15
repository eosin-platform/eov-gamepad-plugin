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

/// [`access_hint`] for an input directory; `flatpak` is the app id when
/// sandboxed.
pub fn access_hint_at(input_dir: &Path, flatpak: Option<&str>) -> Option<String> {
    let entries = match std::fs::read_dir(input_dir) {
        Ok(entries) => entries,
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
    let events: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("event"))
        .collect();
    if events.is_empty() {
        // Inside a sandbox an empty directory means the devices are hidden.
        return flatpak.map(flatpak_message);
    }
    let readable = events
        .iter()
        .any(|entry| std::fs::File::open(entry.path()).is_ok());
    if readable {
        return None;
    }
    Some(match flatpak {
        Some(app) => flatpak_message(app),
        None => format!(
            "eov may not read the input devices in {}. Controllers are usually \
             accessible to the logged-in user; otherwise add a udev rule for your \
             controller or add your user to the 'input' group, then reconnect it.",
            input_dir.display()
        ),
    })
}

fn flatpak_message(app: &str) -> String {
    format!(
        "eov runs as a Flatpak without access to input devices. Allow it with \
         `flatpak override --user --device=input {app}` (Flatpak 1.15.6 or newer; \
         older versions need --device=all), then restart eov."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eov-gamepad-env-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_hidden_input_directory_in_a_flatpak_explains_the_override() {
        let dir = temp_dir("missing");
        let missing = dir.join("input");
        let hint = access_hint_at(&missing, Some("io.eosin.eov")).unwrap();
        assert!(hint.contains("--device=input io.eosin.eov"), "{hint}");
        // Outside a sandbox the message names the directory instead.
        let hint = access_hint_at(&missing, None).unwrap();
        assert!(hint.contains("is not available"), "{hint}");
        // An empty directory is only suspicious in a sandbox.
        let empty = temp_dir("empty");
        assert!(access_hint_at(&empty, None).is_none());
        assert!(access_hint_at(&empty, Some("x.y")).is_some());
    }

    #[test]
    fn the_flatpak_app_id_is_read_from_the_info_file() {
        let info = "[Application]\nname=io.eosin.eov\nruntime=x\n\n[Instance]\nname=nope\n";
        assert_eq!(flatpak_app_from_info(info).as_deref(), Some("io.eosin.eov"));
        assert_eq!(flatpak_app_from_info("[Instance]\nname=x\n"), None);
    }

    #[test]
    fn readable_devices_need_no_hint() {
        let dir = temp_dir("readable");
        std::fs::write(dir.join("event0"), b"").unwrap();
        assert!(access_hint_at(&dir, None).is_none());
        assert!(access_hint_at(&dir, Some("io.eosin.eov")).is_none());
    }
}
