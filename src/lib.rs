//! The eov gamepad plugin: pan, zoom and switch panes, tabs and series items
//! with a game controller (plugin API 1.0).
//!
//! - [`config`]: controls, actions, tuning and the persisted format.
//! - [`engine`]: the pure input state machine (dead zones, gating, motion).
//! - [`backend`]: controller input through gilrs.
//! - `worker`: the input thread that runs the engine and calls the host.
//! - `pointer`: opt-in system pointer control (off by default).
//! - `legacy`: one-time, read-only import of the 0.x settings files.
//! - [`plugin`]: the [`eov_plugin_api::sdk::Plugin`] implementation and the
//!   settings window.

pub mod backend;
pub mod config;
pub mod engine;
mod environment;
mod legacy;
pub mod plugin;
mod pointer;
mod shared;
mod ui;
mod worker;

#[cfg(test)]
mod tests;

pub use plugin::GamepadPlugin;

eov_plugin_api::export_plugin!(GamepadPlugin);
