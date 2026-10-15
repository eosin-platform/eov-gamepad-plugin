//! Opt-in system pointer control for the "Move Pointer" mappings.
//!
//! Moving the system pointer is a system-wide side effect, so it is **off by
//! default** and only happens while eov has focus (GPD-2). When the user turns
//! it on:
//!
//! - The pointer mechanism is set up on its own thread, so a permission
//!   dialog never stalls controller input (GPD-4).
//! - Wayland uses the XDG RemoteDesktop portal and remembers the portal's
//!   restore token, so the permission prompt is not repeated every session.
//!   There is no X11 fallback on Wayland: it would only move the XWayland
//!   pointer.
//! - X11 uses XTest, macOS Quartz events (Accessibility permission) and
//!   Windows `SendInput`, all through `enigo`.
//! - Failures are retried with a growing delay instead of being latched for
//!   the rest of the session; a declined permission prompt turns the option
//!   off instead of asking again.

use crate::shared::{Persist, PointerState, Shared};
use eov_plugin_api::prelude::NotifyLevel;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Moves older than this when the mechanism comes up are dropped instead of
/// jumping the pointer.
const STALE_MOVE: Duration = Duration::from_millis(100);
const FIRST_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(120);

enum Message {
    Move { dx: i32, dy: i32, at: Instant },
    Stop,
}

/// Something that moves the system pointer. Created and used on the pointer
/// thread only.
pub trait PointerSink {
    fn move_by(&mut self, dx: i32, dy: i32) -> Result<(), String>;
}

/// Why connecting failed.
pub struct ConnectError {
    pub message: String,
    /// The user declined; turn the option off instead of retrying.
    pub declined: bool,
}

type Connector = fn(&Shared, u32) -> Result<(Box<dyn PointerSink>, String), ConnectError>;

/// The input thread's handle to the pointer thread, started on first use.
pub struct PointerLink {
    sender: Option<Sender<Message>>,
    /// The current (or last stopped) pointer thread.
    thread: Option<JoinHandle<()>>,
    connector: Connector,
}

impl Default for PointerLink {
    fn default() -> Self {
        Self::with_connector(platform::connect)
    }
}

impl PointerLink {
    pub fn with_connector(connector: Connector) -> Self {
        Self {
            sender: None,
            thread: None,
            connector,
        }
    }

    /// Queues relative pointer motion; never blocks.
    pub fn move_by(&mut self, shared: &Arc<Shared>, dx: i32, dy: i32) {
        if self.sender.is_none() {
            // A stopped thread may still wait for a permission dialog; a
            // second one would open another dialog.
            if self
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
            {
                return;
            }
            let (sender, receiver) = channel();
            let shared_for_thread = Arc::clone(shared);
            let connector = self.connector;
            match std::thread::Builder::new()
                .name("gamepad-pointer".to_string())
                .spawn(move || run(shared_for_thread, receiver, connector))
            {
                // Not joined: it ends after a Stop, or when this link is
                // dropped.
                Ok(thread) => {
                    self.sender = Some(sender);
                    self.thread = Some(thread);
                }
                Err(error) => {
                    shared.update_status(|s| s.pointer = PointerState::Failed(error.to_string()));
                    return;
                }
            }
        }
        if let Some(sender) = &self.sender
            && sender
                .send(Message::Move {
                    dx,
                    dy,
                    at: Instant::now(),
                })
                .is_err()
        {
            self.sender = None;
        }
    }

    /// Releases the pointer mechanism (the option was turned off). Does not
    /// wait: the thread may be waiting for a permission dialog, and it ends
    /// on its own once that returns.
    pub fn stop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Message::Stop);
        }
    }

    pub fn is_running(&self) -> bool {
        self.sender.is_some()
    }
}

impl Drop for PointerLink {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(shared: Arc<Shared>, receiver: Receiver<Message>, connector: Connector) {
    let mut sink: Option<Box<dyn PointerSink>> = None;
    let mut attempts = 0u32;
    let mut retry_at = Instant::now();
    loop {
        let first = match receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => {
                if shared.stopping() {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Coalesce whatever queued up.
        let (mut dx, mut dy, mut newest) = (0i64, 0i64, None);
        let mut stop = false;
        for message in std::iter::once(first).chain(receiver.try_iter()) {
            match message {
                Message::Move { dx: x, dy: y, at } => {
                    dx += i64::from(x);
                    dy += i64::from(y);
                    newest = Some(at);
                }
                Message::Stop => stop = true,
            }
        }
        if stop || shared.stopping() {
            break;
        }
        if sink.is_none() {
            if Instant::now() < retry_at {
                continue;
            }
            shared.update_status(|s| s.pointer = PointerState::Connecting);
            match connector(&shared, attempts) {
                Ok((connected, name)) => {
                    attempts = 0;
                    sink = Some(connected);
                    shared.forget_notification("pointer");
                    shared.update_status(|s| s.pointer = PointerState::Active(name));
                }
                Err(error) if error.declined => {
                    shared.update(Persist::Now, |p| p.pointer_control = false);
                    shared.update_status(|s| s.pointer = PointerState::Off);
                    shared.host.notify(
                        NotifyLevel::Warning,
                        format!("System pointer control was turned off: {}", error.message),
                    );
                    break;
                }
                Err(error) => {
                    attempts += 1;
                    let delay = FIRST_RETRY
                        .saturating_mul(1 << attempts.min(6))
                        .min(MAX_RETRY);
                    retry_at = Instant::now() + delay;
                    shared.notify_once(
                        "pointer",
                        NotifyLevel::Warning,
                        "System pointer control is not available",
                        Some(error.message.clone()),
                    );
                    shared.update_status(|s| s.pointer = PointerState::Failed(error.message));
                    continue;
                }
            }
            // Motion that queued up while connecting is stale.
            if newest.is_some_and(|at| at.elapsed() > STALE_MOVE) {
                continue;
            }
        }
        let clamp = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        if let Some(active) = sink.as_mut()
            && let Err(message) = active.move_by(clamp(dx), clamp(dy))
        {
            sink = None;
            attempts = attempts.max(1);
            retry_at = Instant::now() + FIRST_RETRY;
            shared.update_status(|s| s.pointer = PointerState::Failed(message));
        }
    }
    drop(sink);
    shared.update_status(|s| s.pointer = PointerState::Off);
}

// ---------------------------------------------------------------------------
// Platform mechanisms
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
struct EnigoSink(Box<enigo::Enigo>);

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
impl PointerSink for EnigoSink {
    fn move_by(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        use enigo::Mouse;
        self.0
            .move_mouse(dx, dy, enigo::Coordinate::Rel)
            .map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use ashpd::desktop::{
        PersistMode, ResponseError, Session,
        remote_desktop::{DeviceType, RemoteDesktop},
    };

    struct PortalSink {
        portal: RemoteDesktop<'static>,
        session: Session<'static, RemoteDesktop<'static>>,
    }

    impl PointerSink for PortalSink {
        fn move_by(&mut self, dx: i32, dy: i32) -> Result<(), String> {
            futures_lite::future::block_on(self.portal.notify_pointer_motion(
                &self.session,
                f64::from(dx),
                f64::from(dy),
            ))
            .map_err(|e| e.to_string())
        }
    }

    impl Drop for PortalSink {
        fn drop(&mut self) {
            let _ = futures_lite::future::block_on(self.session.close());
        }
    }

    fn is_wayland() -> bool {
        std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
            || std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty())
    }

    fn portal(shared: &Shared) -> Result<(Box<dyn PointerSink>, String), ConnectError> {
        let token = shared.read_prefs(|p| p.portal_restore_token.clone());
        let failed = |message: String| ConnectError {
            message,
            declined: false,
        };
        let (sink, new_token) = futures_lite::future::block_on(async {
            let portal = RemoteDesktop::new()
                .await
                .map_err(|e| failed(format!("the Remote Desktop portal is not available ({e})")))?;
            let session = portal
                .create_session()
                .await
                .map_err(|e| failed(format!("cannot create a portal session ({e})")))?;
            portal
                .select_devices(
                    &session,
                    DeviceType::Pointer.into(),
                    token.as_deref(),
                    PersistMode::ExplicitlyRevoked,
                )
                .await
                .map_err(|e| failed(format!("cannot request pointer access ({e})")))?;
            // No parent window: the plugin API does not expose the eov
            // window handle the portal would need.
            let response = portal
                .start(&session, None)
                .await
                .map_err(|e| failed(format!("cannot start the portal session ({e})")))?
                .response()
                .map_err(|e| match e {
                    ashpd::Error::Response(ResponseError::Cancelled) => ConnectError {
                        message: "pointer access was declined".to_string(),
                        declined: true,
                    },
                    other => failed(format!("pointer access was not granted ({other})")),
                })?;
            if !response.devices().contains(DeviceType::Pointer) {
                return Err(ConnectError {
                    message: "pointer access was not granted".to_string(),
                    declined: true,
                });
            }
            let new_token = response.restore_token().map(str::to_string);
            Ok((PortalSink { portal, session }, new_token))
        })?;
        if let Some(new_token) = new_token {
            // Restore tokens are single-use; keep the newest one.
            shared.update(Persist::Now, |p| p.portal_restore_token = Some(new_token));
        }
        Ok((Box::new(sink), "Remote Desktop portal".to_string()))
    }

    fn x11() -> Result<(Box<dyn PointerSink>, String), ConnectError> {
        let display = std::env::var("DISPLAY")
            .ok()
            .filter(|d| !d.is_empty())
            .ok_or_else(|| ConnectError {
                message: "no display server to send pointer motion to".to_string(),
                declined: false,
            })?;
        let settings = enigo::Settings {
            x11_display: Some(display),
            ..Default::default()
        };
        let enigo = enigo::Enigo::new(&settings).map_err(|e| ConnectError {
            message: e.to_string(),
            declined: false,
        })?;
        Ok((Box::new(EnigoSink(Box::new(enigo))), "XTest".to_string()))
    }

    pub fn connect(
        shared: &Shared,
        _attempt: u32,
    ) -> Result<(Box<dyn PointerSink>, String), ConnectError> {
        if is_wayland() { portal(shared) } else { x11() }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform {
    use super::*;

    pub fn connect(
        _shared: &Shared,
        attempt: u32,
    ) -> Result<(Box<dyn PointerSink>, String), ConnectError> {
        let settings = enigo::Settings {
            // Ask for the macOS Accessibility permission once, not on every
            // retry.
            open_prompt_to_get_permissions: attempt == 0,
            ..Default::default()
        };
        match enigo::Enigo::new(&settings) {
            Ok(enigo) => Ok((Box::new(EnigoSink(Box::new(enigo))), "system events".into())),
            Err(enigo::NewConError::NoPermission) => Err(ConnectError {
                message: "allow eov under System Settings › Privacy & Security › \
                          Accessibility; eov keeps trying"
                    .to_string(),
                declined: false,
            }),
            Err(error) => Err(ConnectError {
                message: error.to_string(),
                declined: false,
            }),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::*;

    pub fn connect(
        _shared: &Shared,
        _attempt: u32,
    ) -> Result<(Box<dyn PointerSink>, String), ConnectError> {
        Err(ConnectError {
            message: "not supported on this platform".to_string(),
            declined: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Prefs;
    use eov_plugin_api::prelude::Host;
    use eov_plugin_api::testing::MockHost;
    use std::sync::Mutex;

    static MOVES: Mutex<Vec<(i32, i32)>> = Mutex::new(Vec::new());
    static ATTEMPTS: Mutex<u32> = Mutex::new(0);

    struct Recorder;

    impl PointerSink for Recorder {
        fn move_by(&mut self, dx: i32, dy: i32) -> Result<(), String> {
            crate::shared::lock(&MOVES).push((dx, dy));
            Ok(())
        }
    }

    fn shared() -> (MockHost, Arc<Shared>) {
        let mock = MockHost::new();
        let prefs = Prefs {
            pointer_control: true,
            ..Prefs::default()
        };
        let shared = Arc::new(Shared::new(Host::from_ffi(mock.to_ffi()), prefs, true));
        (mock, shared)
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn moves_reach_the_sink_on_its_own_thread() {
        let (_mock, shared) = shared();
        let mut link =
            PointerLink::with_connector(|_, _| Ok((Box::new(Recorder), "test".to_string())));
        link.move_by(&shared, 3, -4);
        wait_for(|| !crate::shared::lock(&MOVES).is_empty());
        assert!(link.is_running());
        assert_eq!(
            shared.status().pointer,
            PointerState::Active("test".to_string())
        );
        link.stop();
        assert!(!link.is_running());
        wait_for(|| shared.status().pointer == PointerState::Off);
    }

    #[test]
    fn failures_are_retried_later_instead_of_latched() {
        let (mock, shared) = shared();
        let mut link = PointerLink::with_connector(|_, _| {
            *crate::shared::lock(&ATTEMPTS) += 1;
            Err(ConnectError {
                message: "nope".to_string(),
                declined: false,
            })
        });
        for _ in 0..20 {
            link.move_by(&shared, 1, 1);
            std::thread::sleep(Duration::from_millis(5));
        }
        wait_for(|| matches!(shared.status().pointer, PointerState::Failed(_)));
        // Backing off: not one attempt per move.
        assert_eq!(*crate::shared::lock(&ATTEMPTS), 1);
        // Reported once.
        assert_eq!(mock.state().notifications.len(), 1);
        assert!(shared.prefs().pointer_control);
        link.stop();
    }

    #[test]
    fn no_second_thread_while_a_prompt_is_pending() {
        static STARTED: Mutex<u32> = Mutex::new(0);
        let (_mock, shared) = shared();
        let mut link = PointerLink::with_connector(|_, _| {
            *crate::shared::lock(&STARTED) += 1;
            // A permission dialog that takes a while to be answered.
            std::thread::sleep(Duration::from_millis(300));
            Ok((Box::new(Recorder), "test".to_string()))
        });
        link.move_by(&shared, 1, 0);
        wait_for(|| *crate::shared::lock(&STARTED) == 1);
        // Turned off and on again while the dialog is open.
        link.stop();
        link.move_by(&shared, 1, 0);
        link.move_by(&shared, 1, 0);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*crate::shared::lock(&STARTED), 1);
        assert!(!link.is_running());
        // Once it has ended, pointer control starts again.
        wait_for(|| link.thread.as_ref().is_none_or(|t| t.is_finished()));
        link.move_by(&shared, 1, 0);
        wait_for(|| *crate::shared::lock(&STARTED) == 2);
        link.stop();
    }

    #[test]
    fn a_declined_prompt_turns_the_option_off() {
        let (mock, shared) = shared();
        let mut link = PointerLink::with_connector(|_, _| {
            Err(ConnectError {
                message: "declined".to_string(),
                declined: true,
            })
        });
        link.move_by(&shared, 1, 1);
        wait_for(|| !shared.prefs().pointer_control);
        wait_for(|| !mock.state().notifications.is_empty());
        link.stop();
    }
}
