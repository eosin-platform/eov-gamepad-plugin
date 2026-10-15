//! Where controller input comes from.
//!
//! [`GilrsBackend`] reads real controllers. Tests use a scripted backend
//! that replays events, so the whole plugin runs without hardware.

use crate::config::Control;
use crate::engine::{DeviceId, DeviceInfo, InputEvent, Power};
use gilrs::ev::filter::{Filter, Jitter, axis_dpad_to_button, deadzone};
use gilrs::{Axis, Button, EventType, GamepadId, Gilrs, GilrsBuilder};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Why a backend could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub message: String,
    /// What the user can do about it, when known (GPD-3).
    pub hint: Option<String>,
}

/// Identifies the plugin instance a backend belongs to (its data directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendContext {
    pub instance: String,
}

/// A source of controller input. Created and used on the input thread only.
pub trait InputBackend: Sized + 'static {
    fn open(context: &BackendContext) -> Result<Self, BackendError>;

    /// Waits up to `timeout` for input and appends what arrived. Returns
    /// early as soon as anything is available.
    fn wait(&mut self, timeout: Duration, events: &mut Vec<InputEvent>);

    /// Re-reads slowly changing details (battery levels) and appends
    /// [`InputEvent::Updated`] for devices that changed.
    fn refresh(&mut self, events: &mut Vec<InputEvent>);

    /// Why no controller may be visible although the backend runs.
    fn access_hint(&self) -> Option<String> {
        None
    }
}

/// A raw event source with a blocking wait that returns for every raw event.
trait Pump {
    type Raw;
    fn poll(&mut self) -> Option<Self::Raw>;
    fn block(&mut self, timeout: Duration) -> Option<Self::Raw>;
    /// Filters and translates one raw event (possibly into nothing).
    fn handle(&mut self, raw: Self::Raw, events: &mut Vec<InputEvent>);
}

/// Waits until something lands in `events` or `timeout` passes, however
/// many raw events are filtered away meanwhile.
fn pump<P: Pump>(source: &mut P, timeout: Duration, events: &mut Vec<InputEvent>) {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(raw) = source.poll() {
            source.handle(raw, events);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !events.is_empty() || remaining.is_zero() {
            return;
        }
        match source.block(remaining) {
            Some(raw) => source.handle(raw, events),
            None => return,
        }
    }
}

// ---------------------------------------------------------------------------
// gilrs
// ---------------------------------------------------------------------------

struct Tracked {
    info: DeviceInfo,
    /// The base identity without the occurrence suffix.
    base: String,
    /// The mapping reports L2/R2 as analog buttons (the common case);
    /// otherwise triggers arrive as the LeftZ/RightZ axes.
    trigger_buttons: bool,
    /// Lowest value seen on each trigger axis: a trigger resting at -1 (full
    /// range) and one resting at 0 (half range) both map to 0–1 without the
    /// non-monotonic fold of 0.x (GPD-4).
    trigger_floor: [f32; 2],
}

/// Real controllers through `gilrs` (evdev, IOKit, Windows.Gaming.Input).
pub struct GilrsBackend {
    gilrs: Gilrs,
    tracked: HashMap<GamepadId, Tracked>,
    next_id: DeviceId,
    pending: Vec<InputEvent>,
    jitter: Jitter,
}

/// Controllers that can drive the default layout: a face or Start button and
/// a left stick. Wheels, pedals and joysticks without these are skipped.
fn is_supported(gamepad: &gilrs::Gamepad<'_>) -> bool {
    let has_button = [Button::South, Button::East, Button::Start]
        .into_iter()
        .any(|button| gamepad.button_code(button).is_some());
    let has_stick = gamepad.axis_code(Axis::LeftStickX).is_some()
        && gamepad.axis_code(Axis::LeftStickY).is_some();
    has_button && has_stick
}

fn base_identity(gamepad: &gilrs::Gamepad<'_>) -> String {
    let hex = |value: Option<u16>| value.map_or_else(|| "none".to_string(), |v| format!("{v:04x}"));
    let uuid: String = gamepad
        .uuid()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "{}:{}:{uuid}:{}",
        hex(gamepad.vendor_id()),
        hex(gamepad.product_id()),
        gamepad.name()
    )
}

fn power(info: gilrs::PowerInfo) -> Power {
    match info {
        gilrs::PowerInfo::Unknown => Power::Unknown,
        gilrs::PowerInfo::Wired => Power::Wired,
        gilrs::PowerInfo::Discharging(level) => Power::Discharging(level.min(100)),
        gilrs::PowerInfo::Charging(level) => Power::Charging(level.min(100)),
        gilrs::PowerInfo::Charged => Power::Charged,
    }
}

fn button_control(button: Button) -> Option<Control> {
    Some(match button {
        Button::DPadLeft => Control::DPadLeft,
        Button::DPadRight => Control::DPadRight,
        Button::DPadUp => Control::DPadUp,
        Button::DPadDown => Control::DPadDown,
        Button::South => Control::South,
        Button::East => Control::East,
        Button::North => Control::North,
        Button::West => Control::West,
        Button::LeftTrigger => Control::LeftShoulder,
        Button::RightTrigger => Control::RightShoulder,
        Button::LeftThumb => Control::LeftStickClick,
        Button::RightThumb => Control::RightStickClick,
        Button::Start => Control::Start,
        Button::Select => Control::Select,
        _ => return None,
    })
}

fn stick_control(axis: Axis) -> Option<Control> {
    Some(match axis {
        Axis::LeftStickX => Control::LeftStickX,
        Axis::LeftStickY => Control::LeftStickY,
        Axis::RightStickX => Control::RightStickX,
        Axis::RightStickY => Control::RightStickY,
        _ => return None,
    })
}

/// Maps a trigger axis reading (-1–1) to 0–1, calibrating the rest position
/// from the lowest value seen.
fn trigger_from_axis(floor: &mut f32, value: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    *floor = floor.min(value.clamp(-1.0, 1.0));
    let span = (1.0 - *floor).max(1e-3);
    ((value - *floor) / span).clamp(0.0, 1.0)
}

impl GilrsBackend {
    /// gilrs's default filters (D-pad hats to buttons, jitter, device dead
    /// zones) and state update, as `Gilrs::next_event` applies them.
    fn filtered(&mut self, raw: gilrs::Event) -> Option<gilrs::Event> {
        let event = Some(raw)
            .filter_ev(&axis_dpad_to_button, &mut self.gilrs)
            .filter_ev(&self.jitter, &mut self.gilrs)
            .filter_ev(&deadzone, &mut self.gilrs)?;
        if event.is_dropped() {
            return None;
        }
        self.gilrs.update(&event);
        Some(event)
    }

    fn track(&mut self, id: GamepadId) -> Option<InputEvent> {
        let gamepad = self.gilrs.connected_gamepad(id)?;
        if !is_supported(&gamepad) {
            return None;
        }
        let base = base_identity(&gamepad);
        // The smallest occurrence number not used by an identical pad.
        let occurrence = (1..)
            .find(|n| {
                !self
                    .tracked
                    .values()
                    .any(|t| t.base == base && t.info.occurrence == *n)
            })
            .unwrap_or(1);
        let key = if occurrence == 1 {
            base.clone()
        } else {
            format!("{base}#{occurrence}")
        };
        let info = DeviceInfo {
            id: self.next_id,
            key,
            name: gamepad.name().to_string(),
            occurrence,
            power: power(gamepad.power_info()),
        };
        self.next_id += 1;
        let trigger_buttons = gamepad.button_code(Button::LeftTrigger2).is_some()
            || gamepad.button_code(Button::RightTrigger2).is_some();
        self.tracked.insert(
            id,
            Tracked {
                info: info.clone(),
                base,
                trigger_buttons,
                trigger_floor: [0.0; 2],
            },
        );
        Some(InputEvent::Connected(info))
    }

    fn translate(&mut self, event: gilrs::Event, out: &mut Vec<InputEvent>) {
        let gilrs::Event { id, event, .. } = event;
        if let EventType::Connected = event {
            // A reconnect after a missed disconnect: start fresh.
            if let Some(old) = self.tracked.remove(&id) {
                out.push(InputEvent::Disconnected(old.info.id));
            }
            out.extend(self.track(id));
            return;
        }
        if let EventType::Disconnected = event {
            if let Some(old) = self.tracked.remove(&id) {
                out.push(InputEvent::Disconnected(old.info.id));
            }
            return;
        }
        let Some(tracked) = self.tracked.get_mut(&id) else {
            return;
        };
        let device = tracked.info.id;
        match event {
            EventType::ButtonPressed(button, _) | EventType::ButtonReleased(button, _) => {
                if let Some(control) = button_control(button) {
                    out.push(InputEvent::Button {
                        device,
                        control,
                        pressed: matches!(event, EventType::ButtonPressed(..)),
                    });
                }
            }
            // Analog (or digital) L2/R2 as buttons: 0–1.
            EventType::ButtonChanged(Button::LeftTrigger2, value, _) => {
                out.push(InputEvent::Axis {
                    device,
                    control: Control::LeftTrigger,
                    value,
                })
            }
            EventType::ButtonChanged(Button::RightTrigger2, value, _) => {
                out.push(InputEvent::Axis {
                    device,
                    control: Control::RightTrigger,
                    value,
                })
            }
            EventType::AxisChanged(axis, value, _) => {
                if let Some(control) = stick_control(axis) {
                    out.push(InputEvent::Axis {
                        device,
                        control,
                        value,
                    });
                } else if !tracked.trigger_buttons
                    && let Some((slot, control)) = match axis {
                        Axis::LeftZ => Some((0, Control::LeftTrigger)),
                        Axis::RightZ => Some((1, Control::RightTrigger)),
                        _ => None,
                    }
                {
                    let value = trigger_from_axis(&mut tracked.trigger_floor[slot], value);
                    out.push(InputEvent::Axis {
                        device,
                        control,
                        value,
                    });
                }
            }
            _ => {}
        }
    }
}

impl Pump for GilrsBackend {
    type Raw = gilrs::Event;

    fn poll(&mut self) -> Option<gilrs::Event> {
        self.gilrs.next_event()
    }

    fn block(&mut self, timeout: Duration) -> Option<gilrs::Event> {
        // Returns for every raw event: the default filters are off.
        self.gilrs.next_event_blocking(Some(timeout))
    }

    fn handle(&mut self, raw: gilrs::Event, events: &mut Vec<InputEvent>) {
        if let Some(event) = self.filtered(raw) {
            self.translate(event, events);
        }
    }
}

impl InputBackend for GilrsBackend {
    fn open(_context: &BackendContext) -> Result<Self, BackendError> {
        // The default filters run in `filtered` instead (see `wait`).
        let gilrs = GilrsBuilder::new()
            .with_default_filters(false)
            .set_update_state(false)
            .build()
            .map_err(|error| BackendError {
                message: error.to_string(),
                hint: crate::environment::access_hint(),
            })?;
        let mut backend = Self {
            gilrs,
            tracked: HashMap::new(),
            next_id: 1,
            pending: Vec::new(),
            jitter: Jitter::new(),
        };
        // gilrs reports controllers that were connected before it started
        // without Connected events.
        let ids: Vec<GamepadId> = backend.gilrs.gamepads().map(|(id, _)| id).collect();
        for id in ids {
            if let Some(event) = backend.track(id) {
                backend.pending.push(event);
            }
        }
        Ok(backend)
    }

    /// With its default filters, gilrs's blocking wait restarts the full
    /// timeout whenever a filter drops an event, so a stick wobbling below
    /// the jitter threshold could keep it from ever returning (and stall
    /// motion, settings writes and shutdown). Its filters therefore run in
    /// [`Pump::handle`], one raw event at a time, and the deadline is ours.
    fn wait(&mut self, timeout: Duration, events: &mut Vec<InputEvent>) {
        events.append(&mut self.pending);
        pump(self, timeout, events);
    }

    fn refresh(&mut self, events: &mut Vec<InputEvent>) {
        for (id, tracked) in &mut self.tracked {
            let Some(gamepad) = self.gilrs.connected_gamepad(*id) else {
                continue;
            };
            let current = power(gamepad.power_info());
            if current != tracked.info.power {
                tracked.info.power = current;
                events.push(InputEvent::Updated(tracked.info.clone()));
            }
        }
    }

    fn access_hint(&self) -> Option<String> {
        crate::environment::access_hint()
    }
}

// ---------------------------------------------------------------------------
// Test backends
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod scripted {
    //! A backend fed from the test through a channel, looked up by the
    //! plugin instance (its data directory).

    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender, channel};

    type Scripts = BTreeMap<String, Receiver<InputEvent>>;

    static SCRIPTS: Mutex<Scripts> = Mutex::new(BTreeMap::new());

    /// Registers the event source for the plugin instance using `data_dir`.
    pub fn script(data_dir: &str) -> Sender<InputEvent> {
        let (sender, receiver) = channel();
        SCRIPTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(data_dir.to_string(), receiver);
        sender
    }

    pub struct ScriptedBackend {
        receiver: Option<Receiver<InputEvent>>,
    }

    impl InputBackend for ScriptedBackend {
        fn open(context: &BackendContext) -> Result<Self, BackendError> {
            let receiver = SCRIPTS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&context.instance);
            Ok(Self { receiver })
        }

        fn wait(&mut self, timeout: Duration, events: &mut Vec<InputEvent>) {
            let Some(receiver) = &self.receiver else {
                std::thread::sleep(timeout);
                return;
            };
            match receiver.recv_timeout(timeout) {
                Ok(event) => events.push(event),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.receiver = None;
                    return;
                }
            }
            events.extend(receiver.try_iter());
        }

        fn refresh(&mut self, _events: &mut Vec<InputEvent>) {}
    }

    /// A backend that never starts (no input subsystem).
    pub struct FailingBackend;

    impl InputBackend for FailingBackend {
        fn open(_context: &BackendContext) -> Result<Self, BackendError> {
            Err(BackendError {
                message: "no input subsystem in this test".to_string(),
                hint: Some("a hint for the test".to_string()),
            })
        }

        fn wait(&mut self, timeout: Duration, _events: &mut Vec<InputEvent>) {
            std::thread::sleep(timeout);
        }

        fn refresh(&mut self, _events: &mut Vec<InputEvent>) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_axes_map_monotonically_whatever_their_rest_position() {
        // Full range: rests at -1.
        let mut floor = 0.0;
        let mut previous = -1.0;
        for step in -100..=100 {
            let v = trigger_from_axis(&mut floor, step as f32 / 100.0);
            assert!(v >= previous, "{step}: {v} < {previous}");
            previous = v;
        }
        assert_eq!(trigger_from_axis(&mut floor, -1.0), 0.0);
        assert_eq!(trigger_from_axis(&mut floor, 1.0), 1.0);
        assert!((trigger_from_axis(&mut floor, 0.0) - 0.5).abs() < 1e-6);
        // Half range: rests at 0 and never reads as half pressed.
        let mut floor = 0.0;
        assert_eq!(trigger_from_axis(&mut floor, 0.0), 0.0);
        assert_eq!(trigger_from_axis(&mut floor, 0.5), 0.5);
        assert_eq!(trigger_from_axis(&mut floor, f32::NAN), 0.0);
    }

    #[test]
    fn every_mapped_button_is_a_button_control() {
        for button in [
            Button::DPadLeft,
            Button::South,
            Button::LeftTrigger,
            Button::LeftThumb,
            Button::Start,
            Button::Select,
        ] {
            assert!(!button_control(button).unwrap().is_axis());
        }
        assert_eq!(button_control(Button::LeftTrigger2), None);
        assert_eq!(button_control(Button::Mode), None);
        assert!(stick_control(Axis::LeftZ).is_none());
    }

    /// Raw events every millisecond that the filters drop (stick jitter),
    /// optionally followed by a real one.
    struct Noisy {
        started: Instant,
        real_after: Option<Duration>,
    }

    impl Pump for Noisy {
        type Raw = bool;

        fn poll(&mut self) -> Option<bool> {
            None
        }

        fn block(&mut self, timeout: Duration) -> Option<bool> {
            std::thread::sleep(timeout.min(Duration::from_millis(1)));
            let real = self
                .real_after
                .is_some_and(|after| self.started.elapsed() >= after);
            Some(real)
        }

        fn handle(&mut self, real: bool, events: &mut Vec<InputEvent>) {
            if real {
                events.push(InputEvent::Disconnected(1));
            }
        }
    }

    #[test]
    fn filtered_noise_does_not_extend_the_wait() {
        let mut noisy = Noisy {
            started: Instant::now(),
            real_after: None,
        };
        let mut events = Vec::new();
        let started = Instant::now();
        pump(&mut noisy, Duration::from_millis(30), &mut events);
        let waited = started.elapsed();
        assert!(events.is_empty());
        assert!(
            waited >= Duration::from_millis(30) && waited < Duration::from_millis(500),
            "{waited:?}"
        );
        // A real event ends the wait early.
        let mut noisy = Noisy {
            started: Instant::now(),
            real_after: Some(Duration::from_millis(5)),
        };
        let started = Instant::now();
        pump(&mut noisy, Duration::from_secs(5), &mut events);
        assert_eq!(events, vec![InputEvent::Disconnected(1)]);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn power_states_are_described_for_people() {
        assert_eq!(
            power(gilrs::PowerInfo::Discharging(80))
                .describe()
                .as_deref(),
            Some("battery 80%")
        );
        assert_eq!(power(gilrs::PowerInfo::Wired).describe(), None);
        assert_eq!(power(gilrs::PowerInfo::Charging(250)), Power::Charging(100));
    }
}
