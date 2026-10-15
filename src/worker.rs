//! The input thread.
//!
//! It blocks in the backend until input arrives or something is due, instead
//! of polling at a fixed rate and re-reading files (GPD-5):
//!
//! - idle, it waits up to [`IDLE_WAIT`] (or until a settings write is due);
//! - while a stick, trigger or zoom button is active, it wakes once per frame
//!   to integrate motion.
//!
//! It calls the host directly — every host method is non-blocking and may be
//! called from any thread — and never while holding a lock (GPD-1).

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
/// Input after this much quiet starts a new burst.
const BURST_GAP: Duration = Duration::from_secs(1);
/// Minimum time between focus refresh requests.
const NUDGE_INTERVAL: Duration = Duration::from_secs(2);
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

    /// Asks the thread to finish and waits up to `timeout` for it.
    pub fn stop(&mut self, shared: &Shared, timeout: Duration) {
        shared.request_stop();
        let Some(thread) = self.thread.take() else {
            return;
        };
        thread.thread().unpark();
        let deadline = Instant::now() + timeout;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if thread.is_finished() {
            let _ = thread.join();
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

/// Requests a host frame when input starts, so the host republishes its
/// window focus: it only does that after rendering, and acting on a stale
/// "focused" (or ignoring input on a stale "unfocused") would be wrong.
#[derive(Default)]
struct FocusNudge {
    last_intent: Option<Instant>,
    last_nudge: Option<Instant>,
}

impl FocusNudge {
    fn intent(&mut self, host: &Host, now: Instant, focused: bool) {
        let burst = self
            .last_intent
            .is_none_or(|at| now.saturating_duration_since(at) >= BURST_GAP);
        self.last_intent = Some(now);
        let due = self
            .last_nudge
            .is_none_or(|at| now.saturating_duration_since(at) >= NUDGE_INTERVAL);
        if (burst || !focused) && due {
            self.last_nudge = Some(now);
            let _ = host.request_render();
        }
    }
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
    let view = shared.worker_view();
    let mut engine = Engine::new(view.config, view.device);
    let mut revision = shared.revision();
    let mut events: Vec<InputEvent> = Vec::new();
    let mut view_cache = ViewCache::default();
    let mut nudge = FocusNudge::default();
    let mut pointer = PointerLink::default();
    let mut next_refresh = Instant::now() + DEVICE_REFRESH;
    // The access hint is re-checked only when the device list changes: it
    // opens device nodes.
    let mut devices_changed = true;

    while !shared.stopping() {
        let now = Instant::now();
        let gate = shared.gate();
        let mut deadline = now + IDLE_WAIT;
        if let Some(tick) = engine.next_tick(now, gate) {
            deadline = deadline.min(tick);
        }
        if let Some(due) = shared.persist_due() {
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
        let gate = shared.gate();
        let mut commands = Vec::new();
        let mut pressed = false;
        for event in events.drain(..) {
            devices_changed |= event.changes_devices();
            pressed |= matches!(event, InputEvent::Button { pressed: true, .. });
            commands.extend(engine.handle(event, now, gate));
        }
        // Real intent (a press, or motion if eov were focused) refreshes the
        // host's focus state; stick noise does not.
        let wants_motion = engine
            .next_tick(
                now,
                Gate {
                    focused: true,
                    ..gate
                },
            )
            .is_some();
        if pressed || wants_motion {
            nudge.intent(&shared.host, now, gate.focused);
        }
        if engine.next_tick(now, gate).is_some_and(|tick| tick <= now) {
            let size = view_cache.get(&shared.host, now, engine.burst_starting());
            commands.extend(engine.tick(now, gate, size));
        }
        if !gate.pointer && pointer.is_running() {
            pointer.stop();
        }
        for command in commands {
            execute(&shared, &mut pointer, command);
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
        shared.persist_if_due(now);
        if input.backend.is_none() && now >= input.retry_at {
            input.try_open(&shared, &context);
        }
    }
    pointer.stop();
    shared.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use eov_plugin_api::testing::MockHost;

    #[test]
    fn focus_is_refreshed_once_per_burst_and_while_unfocused() {
        let mock = MockHost::new();
        let host = Host::from_ffi(mock.to_ffi());
        let mut nudge = FocusNudge::default();
        let t0 = Instant::now();
        let renders = || mock.state().calls_to("render.request").len();
        nudge.intent(&host, t0, true);
        assert_eq!(renders(), 1);
        // Continuous input while focused: no more.
        for ms in (16..3000).step_by(16) {
            nudge.intent(&host, t0 + Duration::from_millis(ms), true);
        }
        assert_eq!(renders(), 1);
        // While believed unfocused, at most every NUDGE_INTERVAL.
        for ms in (3000..7000).step_by(16) {
            nudge.intent(&host, t0 + Duration::from_millis(ms), false);
        }
        assert_eq!(renders(), 3);
        // A new burst after a pause.
        nudge.intent(&host, t0 + Duration::from_secs(20), true);
        assert_eq!(renders(), 4);
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
