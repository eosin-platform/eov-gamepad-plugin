//! The input thread.
//!
//! It blocks in the backend until input arrives or something is due, instead
//! of polling at a fixed rate and re-reading files (GPD-5):
//!
//! - idle, it waits up to [`IDLE_WAIT`];
//! - while a stick, trigger or zoom button is active, it wakes once per frame
//!   to integrate motion.
//!
//! It calls the host directly — every host method is non-blocking and may be
//! called from any thread — and never while holding a lock (GPD-1). Before
//! acting on input whose focus information may be stale it checks eov's
//! focus first ([`FocusCheck`], GPD-2).

use crate::backend::{BackendContext, InputBackend};
use crate::engine::{Command, Engine, Gate, InputEvent, ViewSize};
use crate::pointer::PointerLink;
use crate::shared::{BackendState, Persist, Shared, TOKEN_TOGGLE_WINDOW};
use eov_plugin_api::prelude::*;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Longest blocking wait; bounds how long a stop request takes.
pub const IDLE_WAIT: Duration = Duration::from_millis(250);
/// Battery levels are re-read this often while the window is open.
const DEVICE_REFRESH: Duration = Duration::from_secs(10);
/// The focused pane's size is re-read this often during motion.
const VIEW_REFRESH: Duration = Duration::from_secs(1);
/// How long "eov has focus" is believed after it was confirmed.
const TRUST_FOCUSED: Duration = Duration::from_secs(1);
/// How long "eov is in the background" is believed after it was confirmed.
/// Short, so input after switching back is not dropped for long; it also
/// bounds how often a background eov is asked to render.
const TRUST_UNFOCUSED: Duration = Duration::from_millis(500);
/// Time for eov to render the requested frame and republish its focus.
const CHECK_WAIT: Duration = Duration::from_millis(70);
/// Commands kept while a focus check runs.
const MAX_HELD: usize = 16;
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Status-bar text while input is paused.
pub const PAUSED_STATUS: &str = "Gamepad paused";

/// The running input thread.
pub struct Worker {
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn spawn<B: InputBackend>(
        shared: Arc<Shared>,
        context: BackendContext,
    ) -> std::io::Result<Self> {
        let thread = std::thread::Builder::new()
            .name("gamepad-input".to_string())
            .spawn(move || run::<B>(shared, context))?;
        Ok(Self {
            thread: Some(thread),
        })
    }

    /// Asks the thread to finish, without waiting for it on the UI thread:
    /// it ends within [`IDLE_WAIT`], and the host never unloads plugin
    /// libraries, so it may outlive the plugin's shutdown.
    pub fn stop(&mut self, shared: &Shared) {
        shared.request_stop();
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
        }
    }
}

/// Applies a pause/resume (from the window or the controller).
pub fn set_enabled(shared: &Shared, enabled: bool, from_controller: bool) {
    shared.update(Persist::Now, |p| p.enabled = enabled);
    let host = &shared.host;
    host.set_status((!enabled).then_some(PAUSED_STATUS));
    if from_controller {
        host.notify(
            NotifyLevel::Info,
            if enabled {
                "Gamepad input resumed"
            } else {
                "Gamepad input paused"
            },
        );
    }
    shared.request_refresh();
}

struct Input<B> {
    backend: Option<B>,
    failures: u32,
    retry_at: Instant,
}

impl<B: InputBackend> Input<B> {
    fn open(shared: &Shared, context: &BackendContext) -> Self {
        let mut input = Self {
            backend: None,
            failures: 0,
            retry_at: Instant::now(),
        };
        input.try_open(shared, context);
        input
    }

    fn try_open(&mut self, shared: &Shared, context: &BackendContext) {
        match B::open(context) {
            Ok(backend) => {
                self.backend = Some(backend);
                self.failures = 0;
                shared.forget_notification("backend");
                shared.update_status(|s| {
                    s.backend = BackendState::Running;
                    s.access_hint = None;
                });
            }
            Err(error) => {
                self.failures += 1;
                let delay = RETRY_FIRST
                    .saturating_mul(1 << self.failures.min(4))
                    .min(RETRY_MAX);
                self.retry_at = Instant::now() + delay;
                let hint = error.hint.clone();
                shared.update_status(|s| {
                    s.backend = BackendState::Unavailable(error.message.clone());
                    s.access_hint = hint.clone();
                });
                shared.notify_once(
                    "backend",
                    NotifyLevel::Warning,
                    "Game controllers are not available",
                    Some(hint.unwrap_or(error.message)),
                );
            }
        }
    }
}

/// The focused pane's size, re-read from the host snapshot at the start of
/// a motion burst and once a second during it.
#[derive(Default)]
struct ViewCache {
    size: Option<ViewSize>,
    read_at: Option<Instant>,
}

impl ViewCache {
    fn get(&mut self, host: &Host, now: Instant, fresh: bool) -> Option<ViewSize> {
        let stale = self
            .read_at
            .is_none_or(|at| now.saturating_duration_since(at) >= VIEW_REFRESH);
        if fresh || stale {
            self.size = host.snapshot().ok().and_then(|snapshot| {
                snapshot.focused_view().map(|view| ViewSize {
                    width: view.width,
                    height: view.height,
                })
            });
            self.read_at = Some(now);
        }
        self.size
    }
}

/// Whether eov's focus flag can be acted on.
///
/// eov republishes its window focus only after it renders a frame, so the
/// flag is stale while eov idles: after the user switched to another
/// application it still says "focused", after switching back still
/// "unfocused". Information is fresh after a host focus event, after the
/// plugin's own commands made eov render, or after a check: request a frame,
/// hold the commands for [`CHECK_WAIT`], re-read the snapshot, then run or
/// drop them.
struct FocusCheck {
    trusted_until: Instant,
    seen_epoch: u64,
    /// A check is running; the snapshot is re-read at this time.
    due: Option<Instant>,
    held: Vec<Command>,
}

impl FocusCheck {
    fn new(now: Instant, focused: bool, epoch: u64) -> Self {
        let mut check = Self {
            trusted_until: now,
            seen_epoch: epoch,
            due: None,
            held: Vec::new(),
        };
        check.confirm(now, focused);
        check
    }

    /// Fresh focus information.
    fn confirm(&mut self, now: Instant, focused: bool) {
        self.trusted_until = now
            + if focused {
                TRUST_FOCUSED
            } else {
                TRUST_UNFOCUSED
            };
    }

    fn trusted(&self, now: Instant) -> bool {
        self.due.is_none() && now < self.trusted_until
    }

    /// Starts a check unless one runs; returns whether to request a frame.
    fn begin(&mut self, now: Instant) -> bool {
        if self.due.is_some() {
            return false;
        }
        self.due = Some(now + CHECK_WAIT);
        true
    }

    fn hold(&mut self, command: Command) {
        if self.held.len() < MAX_HELD {
            self.held.push(command);
        }
    }

    /// Ends a running check with fresh information; returns the held
    /// commands.
    fn finish(&mut self, now: Instant, focused: bool) -> Vec<Command> {
        self.due = None;
        self.confirm(now, focused);
        std::mem::take(&mut self.held)
    }
}

/// Whether a command may run now (held commands were made assuming focus).
fn allowed(command: &Command, gate: Gate) -> bool {
    match command {
        // Enable / Pause works while paused, but never in the background.
        Command::SetEnabled(_) => gate.focused,
        Command::PointerMove { .. } => gate.pointer(),
        _ => gate.navigation(),
    }
}

/// Commands after which eov renders, and so republishes its focus.
fn renders(command: &Command) -> bool {
    matches!(
        command,
        Command::Pan { .. }
            | Command::Zoom { .. }
            | Command::Fit
            | Command::CyclePane(_)
            | Command::CycleTab(_)
            | Command::Series(_)
    )
}

fn execute(shared: &Arc<Shared>, pointer: &mut PointerLink, command: Command) {
    let host = &shared.host;
    let result = match command {
        Command::Pan { dx, dy } => host.pan(None, dx, dy),
        Command::Zoom { factor } => host.zoom(None, factor),
        Command::Fit => host.fit(None),
        Command::CyclePane(offset) => host.cycle_pane(offset),
        Command::CycleTab(offset) => host.cycle_tab(offset),
        Command::Series(action) => host.series(action),
        // Window handling belongs to the UI thread.
        Command::ToggleWindow => host.post(TOKEN_TOGGLE_WINDOW),
        Command::SetEnabled(enabled) => {
            set_enabled(shared, enabled, true);
            Ok(())
        }
        Command::PointerMove { dx, dy } => {
            pointer.move_by(shared, dx, dy);
            Ok(())
        }
    };
    if let Err(error) = result
        && error.kind == FfiErrorKind::Revoked
    {
        // The plugin was disabled.
        shared.request_stop();
    }
}

fn run<B: InputBackend>(shared: Arc<Shared>, context: BackendContext) {
    let mut input = Input::<B>::open(&shared, &context);
    let mut revision = shared.revision();
    let view = shared.worker_view();
    let mut engine = Engine::new(view.config, view.device);
    let mut events: Vec<InputEvent> = Vec::new();
    let mut view_cache = ViewCache::default();
    let mut focus = FocusCheck::new(Instant::now(), shared.main_focused(), shared.focus_epoch());
    let mut pointer = PointerLink::default();
    let mut next_refresh = Instant::now() + DEVICE_REFRESH;
    // The access hint is re-checked only when the device list changes: it
    // opens device nodes.
    let mut devices_changed = true;

    while !shared.stopping() {
        let now = Instant::now();
        let mut deadline = now + IDLE_WAIT;
        let motion = motion_gate(&shared, &focus, now);
        if let Some(tick) = engine.next_tick(now, motion) {
            deadline = deadline.min(tick);
        }
        if let Some(due) = focus.due {
            deadline = deadline.min(due);
        }
        let timeout = if events.is_empty() {
            deadline.saturating_duration_since(now)
        } else {
            // Events from `refresh` are waiting.
            Duration::ZERO
        };
        match input.backend.as_mut() {
            Some(backend) => backend.wait(timeout, &mut events),
            None => std::thread::park_timeout(timeout),
        }
        if shared.stopping() {
            break;
        }

        let now = Instant::now();
        let current = shared.revision();
        if current != revision {
            revision = current;
            let view = shared.worker_view();
            engine.set_config(view.config);
            engine.set_preferred(view.device);
        }
        let mut commands = Vec::new();
        // A host focus event is fresh information; it may end a check.
        let epoch = shared.focus_epoch();
        if epoch != focus.seen_epoch {
            focus.seen_epoch = epoch;
            let focused = shared.main_focused();
            if focus.due.is_some() {
                let gate = shared.gate();
                let held = focus.finish(now, focused);
                commands.extend(held.into_iter().filter(|c| allowed(c, gate)));
            } else {
                focus.confirm(now, focused);
            }
        }

        let gate = shared.gate();
        // The settings window reports its own focus directly.
        let trusted = shared.settings_active() || focus.trusted(now);
        let assumed = Gate {
            focused: true,
            ..gate
        };
        let mut pressed = false;
        for event in events.drain(..) {
            devices_changed |= event.changes_devices();
            pressed |= matches!(event, InputEvent::Button { pressed: true, .. });
            match engine.handle(event, now, if trusted { gate } else { assumed }) {
                Some(command) if trusted => commands.push(command),
                Some(command) => focus.hold(command),
                None => {}
            }
        }
        // Real intent (a press, or motion if eov had focus) starts a check;
        // stick noise does not.
        let wants_motion = engine.next_tick(now, assumed).is_some();
        if (pressed || wants_motion) && !trusted && focus.begin(now) {
            let _ = shared.host.request_render();
        }
        if focus.due.is_some_and(|due| now >= due) {
            let focused = shared
                .host
                .snapshot()
                .map_or_else(|_| shared.main_focused(), |s| s.window_focused);
            shared.set_main_focused(focused);
            let held = focus.finish(now, focused);
            let gate = shared.gate();
            commands.extend(held.into_iter().filter(|c| allowed(c, gate)));
        }

        let motion = motion_gate(&shared, &focus, now);
        if engine
            .next_tick(now, motion)
            .is_some_and(|tick| tick <= now)
        {
            let size = view_cache.get(&shared.host, now, engine.burst_starting());
            commands.extend(engine.tick(now, motion, size));
        } else {
            engine.settle(now, motion);
        }
        if !gate.pointer && pointer.is_running() {
            pointer.stop();
        }
        let rendering = commands.iter().any(renders);
        for command in commands {
            execute(&shared, &mut pointer, command);
        }
        if rendering {
            // eov renders for these and republishes its focus; a change
            // arrives as an event within a frame or two.
            focus.confirm(now, shared.main_focused());
        }

        if let Some(backend) = input.backend.as_mut()
            && shared.window_open()
            && now >= next_refresh
        {
            next_refresh = now + DEVICE_REFRESH;
            backend.refresh(&mut events);
        }
        if devices_changed {
            devices_changed = false;
            let devices = engine.devices();
            let hint = match input.backend.as_ref() {
                Some(backend) if devices.is_empty() => Some(backend.access_hint()),
                Some(_) => Some(None),
                None => None,
            };
            shared.update_status(|s| {
                s.devices = devices;
                if let Some(hint) = hint {
                    s.access_hint = hint;
                }
            });
        }
        if input.backend.is_none() && now >= input.retry_at {
            input.try_open(&shared, &context);
        }
    }
    pointer.stop();
}

/// The gate for continuous motion: nothing moves while the focus is being
/// checked or is not trusted.
fn motion_gate(shared: &Shared, focus: &FocusCheck, now: Instant) -> Gate {
    let gate = shared.gate();
    if shared.settings_active() || focus.trusted(now) {
        gate
    } else {
        Gate {
            focused: false,
            ..gate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eov_plugin_api::testing::MockHost;

    #[test]
    fn focus_is_trusted_briefly_and_checked_otherwise() {
        let t0 = Instant::now();
        let mut check = FocusCheck::new(t0, true, 0);
        assert!(check.trusted(t0 + Duration::from_millis(900)));
        assert!(!check.trusted(t0 + TRUST_FOCUSED));
        // A check holds commands until the snapshot is re-read.
        let t1 = t0 + Duration::from_secs(2);
        assert!(check.begin(t1));
        assert!(!check.begin(t1), "one check at a time");
        assert!(!check.trusted(t1));
        check.hold(Command::CycleTab(1));
        assert_eq!(check.due, Some(t1 + CHECK_WAIT));
        let held = check.finish(t1 + CHECK_WAIT, false);
        assert_eq!(held, vec![Command::CycleTab(1)]);
        // "Unfocused" is believed only briefly, so input after switching
        // back is checked again soon.
        let t2 = t1 + CHECK_WAIT;
        assert!(check.trusted(t2 + Duration::from_millis(400)));
        assert!(!check.trusted(t2 + TRUST_UNFOCUSED));
        for _ in 0..100 {
            check.hold(Command::Fit);
        }
        assert_eq!(check.held.len(), MAX_HELD);
    }

    #[test]
    fn held_commands_run_only_with_focus() {
        let focused = Gate {
            enabled: true,
            focused: true,
            pointer: false,
        };
        let background = Gate {
            focused: false,
            ..focused
        };
        let paused = Gate {
            enabled: false,
            ..focused
        };
        assert!(allowed(&Command::CycleTab(1), focused));
        assert!(!allowed(&Command::CycleTab(1), background));
        assert!(!allowed(&Command::Fit, paused));
        assert!(allowed(&Command::SetEnabled(true), paused));
        assert!(!allowed(&Command::SetEnabled(true), background));
        assert!(!allowed(&Command::PointerMove { dx: 1, dy: 0 }, focused));
        assert!(renders(&Command::Pan { dx: 1.0, dy: 0.0 }));
        assert!(!renders(&Command::ToggleWindow));
    }

    #[test]
    fn the_view_size_comes_from_the_focused_pane() {
        let mock = MockHost::new();
        {
            let mut state = mock.state();
            state.snapshot.focused_pane = Some(7);
            state.snapshot.panes.push(PaneInfo {
                pane: 7,
                index: 0,
                file_id: Some(1),
                view: Some(ViewInfo {
                    pane: 7,
                    file_id: 1,
                    width: 800.0,
                    height: 500.0,
                    ..Default::default()
                }),
            });
        }
        let host = Host::from_ffi(mock.to_ffi());
        let mut cache = ViewCache::default();
        let t0 = Instant::now();
        let size = cache.get(&host, t0, true).unwrap();
        assert_eq!((size.width, size.height), (800.0, 500.0));
        cache.get(&host, t0 + Duration::from_millis(100), false);
        assert_eq!(mock.state().calls_to("snapshot").len(), 1);
        cache.get(&host, t0 + VIEW_REFRESH, false);
        assert_eq!(mock.state().calls_to("snapshot").len(), 2);
        // No slide in the focused pane: no size.
        mock.state().snapshot.focused_pane = None;
        assert!(cache.get(&host, t0 + VIEW_REFRESH * 3, true).is_none());
    }
}
