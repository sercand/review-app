//! egui UI: screens, widgets, and the view-models they render from.
//!
//! `egui` is used exclusively through `eframe::egui` so the versions can never
//! drift.

pub mod command;
pub mod components;
pub mod file_icons;
pub mod fonts;
pub mod icons;
pub mod menu;
pub mod screens;
#[cfg(test)]
mod shots;
pub mod theme;
pub mod traffic_lights;
pub mod widgets;
