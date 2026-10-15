//! The input engine: turns controller events into navigation commands.
//!
//! The engine is a pure state machine — no threads, no I/O, time passed in —
//! so the mapping, gating and motion logic is unit-tested without hardware.
//! The worker feeds it events from an input backend and executes the
//! [`Command`]s it returns through the host.
//!
//! - Device state lives per connected device and disappears with it, so a
//!   pad that disconnects with a stick deflected stops moving the view
//!   (GPD-4).
//! - Sticks use a radial dead zone with rescale and a response curve, so
//!   diagonals keep their direction; triggers map monotonically to 0–1.
//! - Motion is integrated over elapsed time (pixels and zoom per second), not
//!   per loop iteration, and sent as relative pan/zoom of the focused pane.
//! - Every action is gated on "enabled and eov focused"; only Enable/Pause
//!   works while paused (GPD-2, GPD-4).

use crate::config::{Action, Config, Control};
use eov_plugin_api::protocol::SeriesAction;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Identifies a connected device for the lifetime of the backend.
pub type DeviceId = u64;

/// Motion is sent to the host at most once per frame.
pub const FRAME: Duration = Duration::from_micros(16_667);

/// Longest time step integrated at once (after a stall, motion does not jump).
const MAX_STEP: Duration = Duration::from_millis(50);

/// Full deflection at sensitivity 1 pans this many pane extents per second.
const PAN_EXTENTS_PER_SECOND: f64 = 2.0;

/// Full deflection at sensitivity 1 zooms by e^3 (×20) per second.
const ZOOM_E_FOLDS_PER_SECOND: f64 = 3.0;

/// Full deflection at speed 1 moves the system pointer this fast.
const POINTER_PIXELS_PER_SECOND: f64 = 1500.0;

/// A short press of a zoom button zooms for at least this long.
const TAP_DURATION: Duration = Duration::from_millis(120);

/// A press this soon after the release of the same control is contact
/// bounce (or a flickering D-pad hat) and triggers no action.
const BOUNCE: Duration = Duration::from_millis(40);

/// Smallest accumulated pan (logical pixels) and zoom (natural log of the
/// factor) worth a host command; smaller motion carries over.
const MIN_PAN: f64 = 0.25;
const MIN_LOG_ZOOM: f64 = 0.0005;

/// Battery and power state of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Power {
    #[default]
    Unknown,
    Wired,
    Discharging(u8),
    Charging(u8),
    Charged,
}

impl Power {
    /// Text for the device label, if there is something worth showing.
    pub fn describe(self) -> Option<String> {
        match self {
            Power::Unknown | Power::Wired => None,
            Power::Discharging(level) => Some(format!("battery {level}%")),
            Power::Charging(level) => Some(format!("charging, {level}%")),
            Power::Charged => Some("charged".to_string()),
        }
    }
}

/// A connected, supported controller.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub id: DeviceId,
    /// Stable identity (vendor, product, mapping GUID, name) plus an
    /// occurrence number, so identical controllers are told apart (GPD-8).
    pub key: String,
    pub name: String,
    /// 1 for the first of several identical controllers, 2 for the next, …
    pub occurrence: u32,
    pub power: Power,
}

impl DeviceInfo {
    /// The name shown in the device list.
    pub fn label(&self) -> String {
        let mut label = self.name.clone();
        if self.occurrence > 1 {
            label.push_str(&format!(" ({})", self.occurrence));
        }
        if let Some(power) = self.power.describe() {
            label.push_str(&format!(" · {power}"));
        }
        label
    }
}

/// Input from a backend. Stick values are -1–1 with right and **up**
/// positive; trigger values are 0 (released) to 1.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Connected(DeviceInfo),
    /// Name or power state changed; input state is kept.
    Updated(DeviceInfo),
    Disconnected(DeviceId),
    Button {
        device: DeviceId,
        control: Control,
        pressed: bool,
    },
    Axis {
        device: DeviceId,
        control: Control,
        value: f32,
    },
}

impl InputEvent {
    /// Whether the event changes the device list.
    pub fn changes_devices(&self) -> bool {
        matches!(
            self,
            InputEvent::Connected(_) | InputEvent::Updated(_) | InputEvent::Disconnected(_)
        )
    }
}

/// What input may do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gate {
    /// The user has not paused the controller.
    pub enabled: bool,
    /// eov (the main window or the gamepad window) has keyboard focus.
    pub focused: bool,
    /// The user opted into system pointer control.
    pub pointer: bool,
}

impl Gate {
    pub fn navigation(self) -> bool {
        self.enabled && self.focused
    }

    pub fn pointer(self) -> bool {
        self.navigation() && self.pointer
    }
}

/// Size of the focused pane in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewSize {
    pub width: f64,
    pub height: f64,
}

/// Something the worker asks the host (or itself) to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Relative pan of the focused pane, in logical pixels.
    Pan {
        dx: f64,
        dy: f64,
    },
    /// Relative zoom of the focused pane around its centre.
    Zoom {
        factor: f64,
    },
    Fit,
    CyclePane(i32),
    CycleTab(i32),
    Series(SeriesAction),
    ToggleWindow,
    SetEnabled(bool),
    /// Relative system pointer motion in pixels (opt-in).
    PointerMove {
        dx: i32,
        dy: i32,
    },
}

/// Radial dead zone with rescale plus response curve: inside the dead zone
/// the stick reads zero; outside it the magnitude is rescaled to start at 0
/// at the dead-zone edge and reach 1 at full deflection, then raised to
/// `curve`. The direction is kept, so diagonals do not snap to the axes.
pub fn shape_stick(x: f32, y: f32, dead_zone: f32, curve: f32) -> (f32, f32) {
    let magnitude = x.hypot(y);
    if !magnitude.is_finite() || magnitude <= dead_zone || magnitude <= f32::EPSILON {
        return (0.0, 0.0);
    }
    let scaled = ((magnitude - dead_zone) / (1.0 - dead_zone).max(1e-3)).min(1.0);
    let shaped = scaled.powf(curve);
    (x / magnitude * shaped, y / magnitude * shaped)
}

/// Dead zone, rescale and curve for a trigger reading 0–1; monotonic.
pub fn shape_trigger(value: f32, dead_zone: f32, curve: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    let value = value.clamp(0.0, 1.0);
    if value <= dead_zone {
        return 0.0;
    }
    ((value - dead_zone) / (1.0 - dead_zone).max(1e-3))
        .min(1.0)
        .powf(curve)
}

struct Device {
    info: DeviceInfo,
    axes: [f32; Control::AXES],
    pressed: [bool; Control::COUNT],
    released_at: [Option<Instant>; Control::COUNT],
    /// Zoom buttons keep zooming until this time after a short press.
    tap_until: [Option<Instant>; Control::COUNT],
}

impl Device {
    fn new(info: DeviceInfo) -> Self {
        Self {
            info,
            axes: [0.0; Control::AXES],
            pressed: [false; Control::COUNT],
            released_at: [None; Control::COUNT],
            tap_until: [None; Control::COUNT],
        }
    }

    fn held(&self, control: Control, now: Instant) -> bool {
        let i = control.index();
        self.pressed[i] || self.tap_until[i].is_some_and(|until| now < until)
    }
}

/// Continuous output for one instant, before integration over time.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Signals {
    /// Pan direction and strength; right and down positive, length ≤ 1.
    pan: (f64, f64),
    /// Zoom rate, -1–1; positive zooms in.
    zoom: f64,
    /// Pointer direction and strength; right and down positive, length ≤ 1.
    pointer: (f64, f64),
}

impl Signals {
    fn is_idle(&self) -> bool {
        self.pan == (0.0, 0.0) && self.zoom == 0.0 && self.pointer == (0.0, 0.0)
    }
}

fn limit_length((x, y): (f64, f64)) -> (f64, f64) {
    let length = x.hypot(y);
    if length > 1.0 {
        (x / length, y / length)
    } else {
        (x, y)
    }
}

/// The input state machine; see the module documentation.
pub struct Engine {
    config: Arc<Config>,
    preferred: Option<String>,
    devices: BTreeMap<DeviceId, Device>,
    last_tick: Option<Instant>,
    pan_rest: (f64, f64),
    zoom_rest: f64,
    pointer_rest: (f64, f64),
}

impl Engine {
    pub fn new(config: Arc<Config>, preferred: Option<String>) -> Self {
        Self {
            config,
            preferred,
            devices: BTreeMap::new(),
            last_tick: None,
            pan_rest: (0.0, 0.0),
            zoom_rest: 0.0,
            pointer_rest: (0.0, 0.0),
        }
    }

    pub fn set_config(&mut self, config: Arc<Config>) {
        self.config = config;
    }

    /// The controller to take input from (`None`: every controller).
    pub fn set_preferred(&mut self, preferred: Option<String>) {
        self.preferred = preferred;
    }

    /// Connected devices in connection order.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.devices.values().map(|d| d.info.clone()).collect()
    }

    /// The device input is taken from: the preferred one while it is
    /// connected, otherwise `None` (every controller).
    pub fn active_selection(&self) -> Option<&str> {
        let preferred = self.preferred.as_deref()?;
        self.devices
            .values()
            .any(|d| d.info.key == preferred)
            .then_some(preferred)
    }

    fn accepted(&self) -> impl Iterator<Item = &Device> + '_ {
        let selection = self.active_selection();
        self.devices
            .values()
            .filter(move |d| selection.is_none_or(|key| d.info.key == key))
    }

    /// Whether a motion burst is starting (no motion was integrated since the
    /// last idle moment).
    pub fn burst_starting(&self) -> bool {
        self.last_tick.is_none()
    }

    /// Processes one event; button presses may produce a discrete command.
    pub fn handle(&mut self, event: InputEvent, now: Instant, gate: Gate) -> Option<Command> {
        match event {
            InputEvent::Connected(info) => {
                self.devices.insert(info.id, Device::new(info));
                None
            }
            InputEvent::Updated(info) => {
                if let Some(device) = self.devices.get_mut(&info.id) {
                    device.info = info;
                }
                None
            }
            InputEvent::Disconnected(id) => {
                // Forget everything, so nothing keeps moving (GPD-4).
                self.devices.remove(&id);
                None
            }
            InputEvent::Axis {
                device,
                control,
                value,
            } => {
                if let Some(device) = self.devices.get_mut(&device)
                    && control.is_axis()
                {
                    let value = if !value.is_finite() {
                        0.0
                    } else if control.is_trigger() {
                        value.clamp(0.0, 1.0)
                    } else {
                        value.clamp(-1.0, 1.0)
                    };
                    device.axes[control.index()] = value;
                }
                None
            }
            InputEvent::Button {
                device,
                control,
                pressed,
            } => self.button(device, control, pressed, now, gate),
        }
    }

    fn button(
        &mut self,
        id: DeviceId,
        control: Control,
        pressed: bool,
        now: Instant,
        gate: Gate,
    ) -> Option<Command> {
        if control.is_axis() {
            return None;
        }
        let selection = self.active_selection().map(str::to_owned);
        let action = self.config.mappings.get(control);
        let device = self.devices.get_mut(&id)?;
        let i = control.index();
        if !pressed {
            if device.pressed[i] {
                device.pressed[i] = false;
                device.released_at[i] = Some(now);
            }
            return None;
        }
        if device.pressed[i] {
            // A repeated press without a release.
            return None;
        }
        device.pressed[i] = true;
        if device.released_at[i].is_some_and(|at| now.saturating_duration_since(at) < BOUNCE) {
            return None;
        }
        if selection.is_some_and(|key| device.info.key != key) {
            return None;
        }
        if matches!(action, Action::ZoomIn | Action::ZoomOut) {
            // Continuous while held; a tap still zooms a visible step.
            device.tap_until[i] = Some(now + TAP_DURATION);
            return None;
        }
        if !gate.focused {
            return None;
        }
        if action == Action::TogglePause {
            // The one action that works while paused, so the pad can resume.
            return Some(Command::SetEnabled(!gate.enabled));
        }
        if !gate.enabled {
            return None;
        }
        Some(match action {
            Action::PreviousPane => Command::CyclePane(-1),
            Action::NextPane => Command::CyclePane(1),
            Action::PreviousTab => Command::CycleTab(-1),
            Action::NextTab => Command::CycleTab(1),
            Action::ToggleSeriesBar => Command::Series(SeriesAction::Toggle),
            Action::PreviousSeriesItem => Command::Series(SeriesAction::Previous),
            Action::NextSeriesItem => Command::Series(SeriesAction::Next),
            Action::OpenSeriesItem => Command::Series(SeriesAction::OpenSelected),
            Action::FitView => Command::Fit,
            Action::ToggleWindow => Command::ToggleWindow,
            _ => return None,
        })
    }

    fn signals(&self, now: Instant, gate: Gate) -> Signals {
        if !gate.navigation() {
            return Signals::default();
        }
        let tuning = &self.config.tuning;
        let mappings = &self.config.mappings;
        let curve = tuning.response_curve;
        // With several controllers, each stick and trigger follows the
        // controller that deflects it most (drift on an idle pad is not
        // added to the active one).
        let mut left = (0.0f32, 0.0f32);
        let mut right = (0.0f32, 0.0f32);
        let mut left_trigger = 0.0f32;
        let mut right_trigger = 0.0f32;
        let mut zoom_in_held = false;
        let mut zoom_out_held = false;
        let longer = |a: (f32, f32), b: (f32, f32)| {
            if b.0.hypot(b.1) > a.0.hypot(a.1) {
                b
            } else {
                a
            }
        };
        for device in self.accepted() {
            let a = &device.axes;
            left = longer(left, shape_stick(a[0], a[1], tuning.left_dead_zone, curve));
            right = longer(
                right,
                shape_stick(a[2], a[3], tuning.right_dead_zone, curve),
            );
            left_trigger = left_trigger.max(shape_trigger(a[4], tuning.trigger_dead_zone, curve));
            right_trigger = right_trigger.max(shape_trigger(a[5], tuning.trigger_dead_zone, curve));
            for control in Control::ALL.into_iter().filter(|c| !c.is_axis()) {
                match mappings.get(control) {
                    Action::ZoomIn if device.held(control, now) => zoom_in_held = true,
                    Action::ZoomOut if device.held(control, now) => zoom_out_held = true,
                    _ => {}
                }
            }
        }
        let mut signals = Signals::default();
        for (control, value) in [
            (Control::LeftStickX, left.0),
            (Control::LeftStickY, left.1),
            (Control::RightStickX, right.0),
            (Control::RightStickY, right.1),
            (Control::LeftTrigger, left_trigger),
            (Control::RightTrigger, right_trigger),
        ] {
            let value = f64::from(value);
            if value == 0.0 {
                continue;
            }
            match mappings.get(control) {
                Action::PanHorizontal => signals.pan.0 += value,
                // Up is positive on sticks; up pans towards the top (−y).
                Action::PanVertical => signals.pan.1 -= value,
                Action::Zoom => signals.zoom += value,
                Action::ZoomIn => signals.zoom += value.abs(),
                Action::ZoomOut => signals.zoom -= value.abs(),
                Action::PointerHorizontal => signals.pointer.0 += value,
                Action::PointerVertical => signals.pointer.1 -= value,
                _ => {}
            }
        }
        if zoom_in_held {
            signals.zoom += 1.0;
        }
        if zoom_out_held {
            signals.zoom -= 1.0;
        }
        signals.pan = limit_length(signals.pan);
        signals.zoom = signals.zoom.clamp(-1.0, 1.0);
        signals.pointer = if gate.pointer() {
            limit_length(signals.pointer)
        } else {
            (0.0, 0.0)
        };
        signals
    }

    /// When the next motion step is due, or `None` while nothing moves.
    pub fn next_tick(&self, now: Instant, gate: Gate) -> Option<Instant> {
        if self.signals(now, gate).is_idle() {
            return None;
        }
        Some(self.last_tick.map_or(now, |last| last + FRAME))
    }

    /// Ends the current motion burst if nothing moves any more, so the next
    /// burst starts with a single frame step (and a fresh view size). The
    /// worker calls this whenever no step is due.
    pub fn settle(&mut self, now: Instant, gate: Gate) {
        if self.last_tick.is_some() && self.signals(now, gate).is_idle() {
            self.rest();
        }
    }

    fn rest(&mut self) {
        self.last_tick = None;
        self.pan_rest = (0.0, 0.0);
        self.zoom_rest = 0.0;
        self.pointer_rest = (0.0, 0.0);
    }

    /// Integrates continuous motion since the previous step. `view` is the
    /// focused pane; without one (no slide) nothing is panned or zoomed.
    pub fn tick(&mut self, now: Instant, gate: Gate, view: Option<ViewSize>) -> Vec<Command> {
        let signals = self.signals(now, gate);
        if signals.is_idle() {
            self.rest();
            return Vec::new();
        }
        let step = match self.last_tick {
            // A long gap means motion stopped in between: a new burst.
            Some(last) if now.saturating_duration_since(last) <= MAX_STEP * 2 => {
                now.saturating_duration_since(last).min(MAX_STEP)
            }
            _ => FRAME,
        };
        self.last_tick = Some(now);
        let dt = step.as_secs_f64();
        let tuning = &self.config.tuning;
        let mut commands = Vec::new();

        if let Some(view) = view {
            let extent = view.width.max(view.height).max(1.0);
            let speed = PAN_EXTENTS_PER_SECOND * f64::from(tuning.pan_sensitivity) * extent * dt;
            self.pan_rest.0 += signals.pan.0 * speed;
            self.pan_rest.1 += signals.pan.1 * speed;
            if self.pan_rest.0.hypot(self.pan_rest.1) >= MIN_PAN {
                commands.push(Command::Pan {
                    dx: self.pan_rest.0,
                    dy: self.pan_rest.1,
                });
                self.pan_rest = (0.0, 0.0);
            }
            self.zoom_rest +=
                signals.zoom * ZOOM_E_FOLDS_PER_SECOND * f64::from(tuning.zoom_sensitivity) * dt;
            if self.zoom_rest.abs() >= MIN_LOG_ZOOM {
                commands.push(Command::Zoom {
                    factor: self.zoom_rest.exp(),
                });
                self.zoom_rest = 0.0;
            }
        }

        let speed = POINTER_PIXELS_PER_SECOND * f64::from(tuning.pointer_speed) * dt;
        self.pointer_rest.0 += signals.pointer.0 * speed;
        self.pointer_rest.1 += signals.pointer.1 * speed;
        let (dx, dy) = (self.pointer_rest.0.trunc(), self.pointer_rest.1.trunc());
        if dx != 0.0 || dy != 0.0 {
            // Fractions carry over, so slow motion is not lost to rounding.
            self.pointer_rest.0 -= dx;
            self.pointer_rest.1 -= dy;
            commands.push(Command::PointerMove {
                dx: dx as i32,
                dy: dy as i32,
            });
        }
        commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Mappings, TuningKey};

    const FOCUSED: Gate = Gate {
        enabled: true,
        focused: true,
        pointer: false,
    };
    const VIEW: Option<ViewSize> = Some(ViewSize {
        width: 1000.0,
        height: 600.0,
    });

    fn device(id: DeviceId, key: &str) -> DeviceInfo {
        DeviceInfo {
            id,
            key: key.to_string(),
            name: "Pad".to_string(),
            occurrence: 1,
            power: Power::Unknown,
        }
    }

    fn engine_with(config: Config) -> Engine {
        let mut engine = Engine::new(Arc::new(config), None);
        let now = Instant::now();
        engine.handle(InputEvent::Connected(device(1, "pad")), now, FOCUSED);
        engine
    }

    fn engine() -> Engine {
        engine_with(Config::default())
    }

    fn axis(engine: &mut Engine, device: DeviceId, control: Control, value: f32, now: Instant) {
        engine.handle(
            InputEvent::Axis {
                device,
                control,
                value,
            },
            now,
            FOCUSED,
        );
    }

    fn press(
        engine: &mut Engine,
        device: DeviceId,
        control: Control,
        now: Instant,
        gate: Gate,
    ) -> Option<Command> {
        let command = engine.handle(
            InputEvent::Button {
                device,
                control,
                pressed: true,
            },
            now,
            gate,
        );
        engine.handle(
            InputEvent::Button {
                device,
                control,
                pressed: false,
            },
            now,
            gate,
        );
        command
    }

    fn pans(commands: &[Command]) -> (f64, f64) {
        commands.iter().fold((0.0, 0.0), |acc, c| match c {
            Command::Pan { dx, dy } => (acc.0 + dx, acc.1 + dy),
            _ => acc,
        })
    }

    fn zoom(commands: &[Command]) -> f64 {
        commands
            .iter()
            .filter_map(|c| match c {
                Command::Zoom { factor } => Some(*factor),
                _ => None,
            })
            .product()
    }

    #[test]
    fn radial_dead_zone_rescales_and_keeps_direction() {
        assert_eq!(shape_stick(0.1, 0.1, 0.16, 1.0), (0.0, 0.0));
        // Just past the edge the output starts near zero, not at the edge.
        let (x, _) = shape_stick(0.17, 0.0, 0.16, 1.0);
        assert!(x > 0.0 && x < 0.02, "{x}");
        assert_eq!(shape_stick(1.0, 0.0, 0.16, 1.6), (1.0, 0.0));
        // A diagonal stays diagonal (a per-axis dead zone would zero x here).
        let (x, y) = shape_stick(0.15, 0.6, 0.16, 1.0);
        assert!(x > 0.0 && (y / x - 4.0).abs() < 1e-4, "{x} {y}");
        // Square-gate corners do not exceed full speed.
        let (x, y) = shape_stick(1.0, 1.0, 0.1, 1.0);
        assert!((x.hypot(y) - 1.0).abs() < 1e-5);
        // Monotonic in the magnitude; the curve lowers the middle.
        let mut previous = 0.0;
        for step in 0..=100 {
            let v = step as f32 / 100.0;
            let (x, _) = shape_stick(v, 0.0, 0.16, 1.6);
            assert!(x >= previous);
            previous = x;
        }
        assert!(shape_stick(0.5, 0.0, 0.0, 2.0).0 < shape_stick(0.5, 0.0, 0.0, 1.0).0);
        assert_eq!(shape_stick(f32::NAN, 0.0, 0.1, 1.0), (0.0, 0.0));
    }

    #[test]
    fn triggers_are_monotonic() {
        let mut previous = -1.0;
        for step in 0..=100 {
            let v = shape_trigger(step as f32 / 100.0, 0.08, 1.6);
            assert!(v >= previous && (0.0..=1.0).contains(&v));
            previous = v;
        }
        assert_eq!(shape_trigger(0.05, 0.08, 1.0), 0.0);
        assert_eq!(shape_trigger(1.0, 0.08, 1.6), 1.0);
        assert_eq!(shape_trigger(f32::NAN, 0.0, 1.0), 0.0);
    }

    #[test]
    fn motion_scales_with_elapsed_time() {
        let t0 = Instant::now();
        let mut e = engine();
        axis(&mut e, 1, Control::LeftStickX, 1.0, t0);
        let first = pans(&e.tick(t0, FOCUSED, VIEW));
        // The first step of a burst integrates one frame.
        let expected = 2.0 * 0.95 * 1000.0 * FRAME.as_secs_f64();
        assert!((first.0 - expected).abs() < 1e-6, "{first:?}");
        let short = pans(&e.tick(t0 + Duration::from_millis(10), FOCUSED, VIEW)).0;
        let long = pans(&e.tick(t0 + Duration::from_millis(30), FOCUSED, VIEW)).0;
        assert!((long / short - 2.0).abs() < 1e-6, "{short} {long}");
        // A stall is capped instead of jumping.
        let stalled = pans(&e.tick(t0 + Duration::from_millis(110), FOCUSED, VIEW)).0;
        assert!((stalled / short - 5.0).abs() < 1e-6, "{stalled}");
    }

    #[test]
    fn every_burst_starts_with_one_frame() {
        // As the worker drives it: ticks only while something moves, settle
        // otherwise.
        let t0 = Instant::now();
        let mut e = engine();
        axis(&mut e, 1, Control::LeftStickX, 1.0, t0);
        let first = pans(&e.tick(t0, FOCUSED, VIEW)).0;
        let t1 = t0 + Duration::from_millis(100);
        axis(&mut e, 1, Control::LeftStickX, 0.0, t1);
        assert_eq!(e.next_tick(t1, FOCUSED), None);
        e.settle(t1, FOCUSED);
        assert!(e.burst_starting());
        let t2 = t1 + Duration::from_secs(5);
        axis(&mut e, 1, Control::LeftStickX, 1.0, t2);
        assert_eq!(e.next_tick(t2, FOCUSED), Some(t2));
        let again = pans(&e.tick(t2, FOCUSED, VIEW)).0;
        assert!((again - first).abs() < 1e-9, "{first} {again}");
        // Even without settle, a long gap does not integrate MAX_STEP.
        axis(&mut e, 1, Control::LeftStickX, 0.0, t2);
        let t3 = t2 + Duration::from_secs(2);
        axis(&mut e, 1, Control::LeftStickX, 1.0, t3);
        let late = pans(&e.tick(t3, FOCUSED, VIEW)).0;
        assert!((late - first).abs() < 1e-9, "{first} {late}");
    }

    #[test]
    fn directions_follow_the_sticks() {
        let t0 = Instant::now();
        let mut e = engine();
        // Up on the left stick pans towards the top of the slide.
        axis(&mut e, 1, Control::LeftStickY, 1.0, t0);
        let (dx, dy) = pans(&e.tick(t0, FOCUSED, VIEW));
        assert!(dx.abs() < 1e-9 && dy < 0.0);
        axis(&mut e, 1, Control::LeftStickY, 0.0, t0);
        // Up on the right stick zooms in; the left trigger zooms out.
        axis(&mut e, 1, Control::RightStickY, 1.0, t0);
        assert!(zoom(&e.tick(t0 + FRAME, FOCUSED, VIEW)) > 1.0);
        axis(&mut e, 1, Control::RightStickY, 0.0, t0);
        axis(&mut e, 1, Control::LeftTrigger, 1.0, t0);
        assert!(zoom(&e.tick(t0 + FRAME * 2, FOCUSED, VIEW)) < 1.0);
    }

    #[test]
    fn nothing_moves_without_focus_or_while_paused() {
        let t0 = Instant::now();
        let mut e = engine();
        axis(&mut e, 1, Control::LeftStickX, 1.0, t0);
        for gate in [
            Gate {
                focused: false,
                ..FOCUSED
            },
            Gate {
                enabled: false,
                ..FOCUSED
            },
        ] {
            assert!(e.tick(t0, gate, VIEW).is_empty());
            assert_eq!(e.next_tick(t0, gate), None);
        }
        assert!(!e.tick(t0, FOCUSED, VIEW).is_empty());
        // Without a slide in the focused pane, nothing is panned.
        assert!(pans(&e.tick(t0 + FRAME, FOCUSED, None)) == (0.0, 0.0));
    }

    #[test]
    fn buttons_are_gated_too() {
        let t0 = Instant::now();
        let mut e = engine();
        let unfocused = Gate {
            focused: false,
            ..FOCUSED
        };
        let paused = Gate {
            enabled: false,
            ..FOCUSED
        };
        assert_eq!(
            press(&mut e, 1, Control::DPadDown, t0, FOCUSED),
            Some(Command::CycleTab(1))
        );
        let later = t0 + Duration::from_secs(1);
        assert_eq!(press(&mut e, 1, Control::DPadDown, later, unfocused), None);
        let later = later + Duration::from_secs(1);
        assert_eq!(press(&mut e, 1, Control::DPadDown, later, paused), None);
        // Enable/Pause still works while paused (but not in the background).
        let later = later + Duration::from_secs(1);
        assert_eq!(
            press(&mut e, 1, Control::Start, later, paused),
            Some(Command::SetEnabled(true))
        );
        let later = later + Duration::from_secs(1);
        assert_eq!(press(&mut e, 1, Control::Start, later, unfocused), None);
        let later = later + Duration::from_secs(1);
        assert_eq!(
            press(&mut e, 1, Control::Start, later, FOCUSED),
            Some(Command::SetEnabled(false))
        );
    }

    #[test]
    fn disconnecting_stops_motion() {
        let t0 = Instant::now();
        let mut e = engine();
        axis(&mut e, 1, Control::LeftStickX, 0.9, t0);
        assert!(!e.tick(t0, FOCUSED, VIEW).is_empty());
        e.handle(InputEvent::Disconnected(1), t0, FOCUSED);
        assert_eq!(e.next_tick(t0 + FRAME, FOCUSED), None);
        assert!(e.tick(t0 + FRAME, FOCUSED, VIEW).is_empty());
        // A reconnect starts from a centred stick.
        e.handle(InputEvent::Connected(device(1, "pad")), t0, FOCUSED);
        assert_eq!(e.next_tick(t0 + FRAME, FOCUSED), None);
    }

    #[test]
    fn identical_controllers_are_told_apart() {
        let t0 = Instant::now();
        let mut e = Engine::new(Arc::new(Config::default()), Some("pad#2".into()));
        e.handle(InputEvent::Connected(device(1, "pad")), t0, FOCUSED);
        e.handle(InputEvent::Connected(device(2, "pad#2")), t0, FOCUSED);
        assert_eq!(e.active_selection(), Some("pad#2"));
        // The other pad is ignored, buttons and sticks alike.
        assert_eq!(press(&mut e, 1, Control::DPadDown, t0, FOCUSED), None);
        axis(&mut e, 1, Control::LeftStickX, 1.0, t0);
        assert!(e.tick(t0, FOCUSED, VIEW).is_empty());
        assert_eq!(
            press(&mut e, 2, Control::DPadDown, t0, FOCUSED),
            Some(Command::CycleTab(1))
        );
        // When the preferred pad goes away, every pad works again.
        e.handle(InputEvent::Disconnected(2), t0, FOCUSED);
        assert_eq!(e.active_selection(), None);
        assert!(!e.tick(t0, FOCUSED, VIEW).is_empty());
    }

    #[test]
    fn several_controllers_do_not_add_up() {
        let t0 = Instant::now();
        let mut e = engine();
        e.handle(InputEvent::Connected(device(2, "other")), t0, FOCUSED);
        axis(&mut e, 1, Control::LeftStickX, 1.0, t0);
        let single = pans(&e.tick(t0, FOCUSED, VIEW)).0;
        axis(&mut e, 2, Control::LeftStickX, 0.6, t0);
        let both = pans(&e.tick(t0 + FRAME, FOCUSED, VIEW)).0;
        assert!((both - single).abs() < 1e-6, "{single} {both}");
    }

    #[test]
    fn presses_are_debounced() {
        let t0 = Instant::now();
        let mut e = engine();
        let pressed = |e: &mut Engine, at| {
            e.handle(
                InputEvent::Button {
                    device: 1,
                    control: Control::DPadRight,
                    pressed: true,
                },
                at,
                FOCUSED,
            )
        };
        let released = |e: &mut Engine, at| {
            e.handle(
                InputEvent::Button {
                    device: 1,
                    control: Control::DPadRight,
                    pressed: false,
                },
                at,
                FOCUSED,
            )
        };
        assert_eq!(pressed(&mut e, t0), Some(Command::CyclePane(1)));
        // A second press without a release is not a new press.
        assert_eq!(pressed(&mut e, t0 + Duration::from_millis(5)), None);
        released(&mut e, t0 + Duration::from_millis(60));
        // Contact bounce right after the release.
        assert_eq!(pressed(&mut e, t0 + Duration::from_millis(70)), None);
        released(&mut e, t0 + Duration::from_millis(80));
        assert_eq!(
            pressed(&mut e, t0 + Duration::from_millis(200)),
            Some(Command::CyclePane(1))
        );
    }

    #[test]
    fn zoom_buttons_zoom_while_held_and_taps_still_zoom() {
        let mut config = Config::default();
        config.mappings.set(Control::West, Action::ZoomIn);
        let t0 = Instant::now();
        let mut e = engine_with(config);
        assert_eq!(press(&mut e, 1, Control::West, t0, FOCUSED), None);
        // Released immediately, yet it zooms for TAP_DURATION.
        let mut total = 1.0;
        let mut t = t0;
        while let Some(due) = e.next_tick(t, FOCUSED) {
            t = due.max(t + Duration::from_millis(1));
            total *= zoom(&e.tick(t, FOCUSED, VIEW));
            assert!(t < t0 + Duration::from_secs(1), "the tap never ended");
        }
        let expected = (ZOOM_E_FOLDS_PER_SECOND * 0.78 * TAP_DURATION.as_secs_f64()).exp();
        assert!(
            total > 1.0 && (total / expected - 1.0).abs() < 0.2,
            "{total} {expected}"
        );
        // Held: keeps zooming.
        e.handle(
            InputEvent::Button {
                device: 1,
                control: Control::West,
                pressed: true,
            },
            t + Duration::from_secs(1),
            FOCUSED,
        );
        assert!(e.next_tick(t + Duration::from_secs(3), FOCUSED).is_some());
    }

    #[test]
    fn the_pointer_moves_only_when_opted_in() {
        let mut config = Config::default();
        config
            .mappings
            .set(Control::RightStickX, Action::PointerHorizontal);
        let t0 = Instant::now();
        let mut e = engine_with(config);
        axis(&mut e, 1, Control::RightStickX, 1.0, t0);
        assert!(e.tick(t0, FOCUSED, VIEW).is_empty());
        let opted_in = Gate {
            pointer: true,
            ..FOCUSED
        };
        let moved: i32 = e
            .tick(t0 + FRAME, opted_in, VIEW)
            .iter()
            .map(|c| match c {
                Command::PointerMove { dx, .. } => *dx,
                _ => 0,
            })
            .sum();
        assert!(moved > 0);
    }

    #[test]
    fn slow_pointer_motion_is_not_lost_to_rounding() {
        let mut config = Config::default();
        config
            .mappings
            .set(Control::RightStickX, Action::PointerHorizontal);
        config.tuning.set(TuningKey::PointerSpeed, 0.1);
        config.tuning.set(TuningKey::ResponseCurve, 1.0);
        config.tuning.set(TuningKey::RightDeadZone, 0.0);
        let gate = Gate {
            pointer: true,
            ..FOCUSED
        };
        let t0 = Instant::now();
        let mut e = engine_with(config);
        axis(&mut e, 1, Control::RightStickX, 0.2, t0);
        // 0.2 × 150 px/s = 30 px/s, i.e. half a pixel per frame.
        let moved: i32 = (0..60)
            .flat_map(|frame| e.tick(t0 + FRAME * frame, gate, VIEW))
            .map(|c| match c {
                Command::PointerMove { dx, .. } => dx,
                _ => 0,
            })
            .sum();
        assert!((28..=31).contains(&moved), "{moved}");
    }

    #[test]
    fn drift_below_the_threshold_sends_nothing_until_it_adds_up() {
        let t0 = Instant::now();
        let mut e = engine();
        // Just outside the dead zone: tiny motion, accumulated.
        axis(&mut e, 1, Control::LeftStickX, 0.18, t0);
        let first = e.tick(t0, FOCUSED, VIEW);
        assert!(first.is_empty(), "{first:?}");
        let total: f64 = (1..120)
            .map(|frame| pans(&e.tick(t0 + FRAME * frame, FOCUSED, VIEW)).0)
            .sum();
        assert!(total > 0.0);
    }

    #[test]
    fn the_default_layout_drives_navigation() {
        let t0 = Instant::now();
        let mut e = engine();
        let mut t = t0;
        let mut next = || {
            t += Duration::from_secs(1);
            t
        };
        let cases = [
            (Control::DPadLeft, Command::CyclePane(-1)),
            (Control::DPadRight, Command::CyclePane(1)),
            (Control::DPadUp, Command::CycleTab(-1)),
            (Control::DPadDown, Command::CycleTab(1)),
            (Control::North, Command::Series(SeriesAction::Toggle)),
            (
                Control::LeftShoulder,
                Command::Series(SeriesAction::Previous),
            ),
            (Control::RightShoulder, Command::Series(SeriesAction::Next)),
            (Control::South, Command::Series(SeriesAction::OpenSelected)),
            (Control::East, Command::Fit),
            (Control::Select, Command::ToggleWindow),
        ];
        for (control, expected) in cases {
            let at = next();
            assert_eq!(press(&mut e, 1, control, at, FOCUSED), Some(expected));
        }
        let at = next();
        assert_eq!(press(&mut e, 1, Control::West, at, FOCUSED), None);
        assert_eq!(Mappings::default().get(Control::West), Action::None);
    }
}
