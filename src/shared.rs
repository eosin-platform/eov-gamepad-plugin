//! State shared by the UI thread (events, callbacks) and the plugin's
//! threads (input, pointer, file import/export).
//!
//! Locks guard plain data and are held only for short local updates. **No
//! lock is ever held while calling the host** (GPD-1): every host call is
//! made after the guard is dropped, from copies taken under it.
//!
//! Only the UI thread pushes properties to the settings window. Other threads
//! ask for a refresh with `host.post`, which the host answers with a
//! `Posted` event on the UI thread, so pushes are naturally serialised.
//!
//! Settings are written by one thread of their own ([`Shared::run_persister`]),
//! so a write is never concurrent with another, never runs on the UI thread
//! and never stalls controller input (GPD-5).

use crate::config::{Config, Prefs, STATE_KEY};
use crate::engine::{DeviceInfo, Gate};
use eov_plugin_api::prelude::*;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// `Posted` token: push the current state to the settings window.
pub const TOKEN_REFRESH: &str = "gamepad:refresh";
/// `Posted` token: show or hide the settings window (gamepad action).
pub const TOKEN_TOGGLE_WINDOW: &str = "gamepad:toggle-window";

/// Settings changes are written at most this long after the first change
/// of a burst (slider drags are coalesced, GPD-5).
const PERSIST_DELAY: Duration = Duration::from_millis(500);
/// After a failed write, try again this much later.
const PERSIST_RETRY: Duration = Duration::from_secs(10);

/// When an edit should be written to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persist {
    /// Debounced (continuous edits such as slider drags).
    Soon,
    /// Without delay (discrete actions such as profile operations).
    Now,
}

/// Poison-tolerant locking: a panic elsewhere must not wedge the plugin.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// State of the controller input subsystem.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum BackendState {
    #[default]
    Starting,
    Running,
    /// The subsystem failed to start; the message says why.
    Unavailable(String),
}

/// State of the opt-in system pointer control.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum PointerState {
    #[default]
    Off,
    /// Connecting (the system may be asking for permission).
    Connecting,
    /// Moving the pointer through the named mechanism.
    Active(String),
    /// Not working; retried later unless the user turns it off.
    Failed(String),
}

/// What the input thread reports for the UI.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Status {
    pub backend: BackendState,
    pub devices: Vec<DeviceInfo>,
    pub pointer: PointerState,
    /// Why no controller can be seen, when known (GPD-3).
    pub access_hint: Option<String>,
}

/// What the input thread needs from the preferences.
#[derive(Debug, Clone)]
pub struct WorkerView {
    pub config: Arc<Config>,
    pub device: Option<String>,
}

#[derive(Default)]
struct PersistState {
    /// When the next write is due.
    due: Option<Instant>,
    /// The persister should write what is pending and end.
    stop: bool,
}

pub struct Shared {
    pub host: Host,
    prefs: Mutex<Prefs>,
    worker_view: Mutex<WorkerView>,
    revision: AtomicU64,
    status: Mutex<Status>,
    enabled: AtomicBool,
    pointer_control: AtomicBool,
    main_focused: AtomicBool,
    /// Bumped whenever the host reports a window focus change.
    focus_epoch: AtomicU64,
    settings_active: AtomicBool,
    window_open: AtomicBool,
    persist: Mutex<PersistState>,
    persist_wake: Condvar,
    refresh_queued: AtomicBool,
    stop: AtomicBool,
    notified: Mutex<BTreeSet<String>>,
}

impl Shared {
    pub fn new(host: Host, prefs: Prefs, focused: bool) -> Self {
        let view = WorkerView {
            config: Arc::new(prefs.config.clone()),
            device: prefs.device.clone(),
        };
        Self {
            host,
            enabled: AtomicBool::new(prefs.enabled),
            pointer_control: AtomicBool::new(prefs.pointer_control),
            prefs: Mutex::new(prefs),
            worker_view: Mutex::new(view),
            revision: AtomicU64::new(1),
            status: Mutex::new(Status::default()),
            main_focused: AtomicBool::new(focused),
            focus_epoch: AtomicU64::new(0),
            settings_active: AtomicBool::new(false),
            window_open: AtomicBool::new(false),
            persist: Mutex::new(PersistState::default()),
            persist_wake: Condvar::new(),
            refresh_queued: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            notified: Mutex::new(BTreeSet::new()),
        }
    }

    // -- preferences ----------------------------------------------------------

    /// A copy of the preferences.
    #[cfg(test)]
    pub fn prefs(&self) -> Prefs {
        lock(&self.prefs).clone()
    }

    /// Reads the preferences without copying them.
    pub fn read_prefs<R>(&self, read: impl FnOnce(&Prefs) -> R) -> R {
        read(&lock(&self.prefs))
    }

    /// Edits the preferences, republishes what the input thread uses and
    /// schedules a write.
    pub fn update<R>(&self, persist: Persist, edit: impl FnOnce(&mut Prefs) -> R) -> R {
        let result = {
            let mut prefs = lock(&self.prefs);
            let result = edit(&mut prefs);
            self.enabled.store(prefs.enabled, Ordering::Release);
            self.pointer_control
                .store(prefs.pointer_control, Ordering::Release);
            // Published under the prefs lock, so concurrent updates from
            // several threads cannot leave an older view in place.
            *lock(&self.worker_view) = WorkerView {
                config: Arc::new(prefs.config.clone()),
                device: prefs.device.clone(),
            };
            self.revision.fetch_add(1, Ordering::AcqRel);
            result
        };
        self.schedule_persist(persist);
        result
    }

    /// The revision of [`Self::worker_view`]; changes with every update.
    /// Read it before the view: a newer view then shows up as another
    /// revision change instead of being missed.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn worker_view(&self) -> WorkerView {
        lock(&self.worker_view).clone()
    }

    // -- persistence -------------------------------------------------------------

    fn schedule_persist(&self, persist: Persist) {
        let now = Instant::now();
        {
            let mut state = lock(&self.persist);
            state.due = Some(match (persist, state.due) {
                (Persist::Now, _) => now,
                (Persist::Soon, Some(existing)) => existing,
                (Persist::Soon, None) => now + PERSIST_DELAY,
            });
        }
        self.persist_wake.notify_all();
    }

    /// When the next write is due.
    #[cfg(test)]
    pub fn persist_due(&self) -> Option<Instant> {
        lock(&self.persist).due
    }

    /// The settings writer: sleeps until a write is due, writes, and on
    /// [`Self::stop_persister`] writes what is pending and returns.
    pub fn run_persister(&self) {
        let mut state = lock(&self.persist);
        loop {
            let now = Instant::now();
            let (due, stop) = (state.due, state.stop);
            match due {
                Some(at) if at <= now || stop => {
                    state.due = None;
                    drop(state);
                    self.write_prefs();
                    state = lock(&self.persist);
                }
                _ if stop => return,
                Some(at) => {
                    state = self
                        .persist_wake
                        .wait_timeout(state, at - now)
                        .map(|(guard, _)| guard)
                        .unwrap_or_else(|e| e.into_inner().0);
                }
                None => {
                    state = self
                        .persist_wake
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }

    /// Asks the settings writer to write what is pending and end.
    pub fn stop_persister(&self) {
        lock(&self.persist).stop = true;
        self.persist_wake.notify_all();
    }

    fn write_prefs(&self) {
        let state = lock(&self.prefs).to_persisted();
        // The host writes the settings file atomically (temp file + rename).
        if let Err(error) = self.host.set_setting(STATE_KEY, &state) {
            if error.kind == FfiErrorKind::Revoked {
                return;
            }
            {
                let mut persist = lock(&self.persist);
                if persist.due.is_none() && !persist.stop {
                    persist.due = Some(Instant::now() + PERSIST_RETRY);
                }
            }
            self.notify_once(
                "persist",
                NotifyLevel::Warning,
                "Gamepad settings could not be saved",
                Some(error.message),
            );
        }
    }

    // -- flags --------------------------------------------------------------------

    pub fn gate(&self) -> Gate {
        Gate {
            enabled: self.enabled.load(Ordering::Acquire),
            focused: self.main_focused.load(Ordering::Acquire)
                || self.settings_active.load(Ordering::Acquire),
            pointer: self.pointer_control.load(Ordering::Acquire),
        }
    }

    pub fn main_focused(&self) -> bool {
        self.main_focused.load(Ordering::Acquire)
    }

    /// The host reported a focus change (fresh information).
    pub fn focus_event(&self, focused: bool) {
        self.main_focused.store(focused, Ordering::Release);
        self.focus_epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// The focus as just read from the host snapshot.
    pub fn set_main_focused(&self, focused: bool) {
        self.main_focused.store(focused, Ordering::Release);
    }

    /// Changes whenever [`Self::focus_event`] is called.
    pub fn focus_epoch(&self) -> u64 {
        self.focus_epoch.load(Ordering::Acquire)
    }

    pub fn settings_active(&self) -> bool {
        self.settings_active.load(Ordering::Acquire)
    }

    pub fn set_settings_active(&self, active: bool) {
        self.settings_active.store(active, Ordering::Release);
    }

    pub fn window_open(&self) -> bool {
        self.window_open.load(Ordering::Acquire)
    }

    pub fn set_window_open(&self, open: bool) {
        self.window_open.store(open, Ordering::Release);
        if !open {
            self.settings_active.store(false, Ordering::Release);
        }
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    // -- status ------------------------------------------------------------------------

    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    /// Changes the status and refreshes the window if anything changed.
    pub fn update_status(&self, edit: impl FnOnce(&mut Status)) {
        let changed = {
            let mut status = lock(&self.status);
            let before = status.clone();
            edit(&mut status);
            *status != before
        };
        if changed {
            self.request_refresh();
        }
    }

    // -- UI and notifications ------------------------------------------------------------

    /// Asks the UI thread to push the state to the window (coalesced).
    pub fn request_refresh(&self) {
        if self.window_open() && !self.refresh_queued.swap(true, Ordering::AcqRel) {
            let _ = self.host.post(TOKEN_REFRESH);
        }
    }

    /// Called on the UI thread before it reads the state for a push.
    pub fn refresh_started(&self) {
        self.refresh_queued.store(false, Ordering::Release);
    }

    /// Shows a notification the first time `key` is reported.
    pub fn notify_once(
        &self,
        key: &str,
        level: NotifyLevel,
        message: &str,
        detail: Option<String>,
    ) {
        if !lock(&self.notified).insert(key.to_string()) {
            return;
        }
        match detail {
            Some(detail) if level == NotifyLevel::Error => {
                self.host.notify_error(message, detail);
            }
            Some(detail) => self.host.notify(level, format!("{message}: {detail}")),
            None => self.host.notify(level, message),
        }
    }

    /// Lets `key` be reported again (the problem went away).
    pub fn forget_notification(&self, key: &str) {
        lock(&self.notified).remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eov_plugin_api::testing::MockHost;
    use serde_json::Value;

    fn shared() -> (MockHost, Shared) {
        let mock = MockHost::new();
        let host = Host::from_ffi(mock.to_ffi());
        (mock, Shared::new(host, Prefs::default(), true))
    }

    fn writes(mock: &MockHost) -> Vec<Value> {
        mock.state().calls_to("settings.set")
    }

    fn eventually(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn edits_are_debounced_and_written_by_the_persister() {
        let (mock, shared) = shared();
        let shared = Arc::new(shared);
        let persister = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || shared.run_persister())
        };
        let start = Instant::now();
        for _ in 0..10 {
            shared.update(Persist::Soon, |p| p.enabled = !p.enabled);
        }
        let due = shared.persist_due().unwrap();
        assert!(due >= start + PERSIST_DELAY);
        // Nothing yet: a burst of edits is written once, after the delay.
        std::thread::sleep(Duration::from_millis(100));
        assert!(writes(&mock).is_empty());
        eventually(|| writes(&mock).len() == 1);
        assert!(Instant::now() >= due);
        assert_eq!(writes(&mock)[0]["key"], STATE_KEY);
        assert_eq!(writes(&mock)[0]["value"]["enabled"], true);
        assert_eq!(shared.persist_due(), None);
        // Discrete changes are written right away.
        let before = Instant::now();
        shared.update(Persist::Now, |p| p.enabled = false);
        eventually(|| writes(&mock).len() == 2);
        assert!(before.elapsed() < PERSIST_DELAY);
        // Stopping writes what is still pending.
        shared.update(Persist::Soon, |p| p.enabled = true);
        shared.stop_persister();
        persister.join().unwrap();
        assert_eq!(writes(&mock).len(), 3);
        assert_eq!(writes(&mock)[2]["value"]["enabled"], true);
    }

    #[test]
    fn the_input_thread_never_sees_an_older_view() {
        let (_mock, shared) = shared();
        let shared = Arc::new(shared);
        let threads: Vec<_> = (0..4)
            .map(|n| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    for i in 0..200 {
                        shared.update(Persist::Soon, |p| {
                            p.device = Some(format!("{n}-{i}"));
                        });
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let revision = shared.revision();
        let view = shared.worker_view();
        assert_eq!(view.device, shared.read_prefs(|p| p.device.clone()));
        assert_eq!(revision, 1 + 4 * 200);
    }

    #[test]
    fn the_gate_combines_focus_sources() {
        let (_mock, shared) = shared();
        assert!(shared.gate().navigation());
        shared.set_main_focused(false);
        assert!(!shared.gate().focused);
        shared.set_window_open(true);
        shared.set_settings_active(true);
        assert!(shared.gate().focused);
        // Closing the window forgets its focus.
        shared.set_window_open(false);
        assert!(!shared.gate().focused);
        shared.update(Persist::Soon, |p| p.enabled = false);
        shared.set_main_focused(true);
        assert!(!shared.gate().navigation());
    }

    #[test]
    fn refreshes_are_coalesced_and_only_sent_with_an_open_window() {
        let (mock, shared) = shared();
        shared.request_refresh();
        assert!(mock.state().posted.is_empty());
        shared.set_window_open(true);
        shared.request_refresh();
        shared.request_refresh();
        assert_eq!(mock.state().posted, vec![TOKEN_REFRESH.to_string()]);
        shared.refresh_started();
        shared.request_refresh();
        assert_eq!(mock.state().posted.len(), 2);
    }

    #[test]
    fn notifications_are_shown_once() {
        let (mock, shared) = shared();
        for _ in 0..3 {
            shared.notify_once("k", NotifyLevel::Warning, "Problem", None);
        }
        assert_eq!(mock.state().notifications.len(), 1);
        shared.forget_notification("k");
        shared.notify_once("k", NotifyLevel::Warning, "Problem", None);
        assert_eq!(mock.state().notifications.len(), 2);
    }
}
