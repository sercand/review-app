//! The theme preference: what the user picks, and how it resolves against
//! the operating system's light/dark setting (Task 53).

use eframe::egui;
use serde::{Deserialize, Serialize};

/// The user's theme choice, persisted in `settings.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    /// Follow the operating system.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

impl Theme {
    /// The concrete egui theme for this preference, given the OS's current
    /// dark-mode flag (`System` follows it, the others ignore it).
    pub fn resolve(&self, system_dark: bool) -> egui::Theme {
        match self {
            Theme::System => {
                if system_dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                }
            }
            Theme::Light => egui::Theme::Light,
            Theme::Dark => egui::Theme::Dark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::search::Scope;
    use crate::persist::settings::Settings;
    use crate::ui::widgets::settings_window::{settings_vm, SettingsVm};

    #[test]
    fn theme_resolves_system_and_vm_round_trips() {
        assert_eq!(Theme::System.resolve(true), egui::Theme::Dark);
        assert_eq!(Theme::System.resolve(false), egui::Theme::Light);
        assert_eq!(Theme::Light.resolve(true), egui::Theme::Light);

        let mut s = Settings::default();
        let mut vm = settings_vm(&s);
        vm.host = "ghe.example.com".into();
        vm.default_scopes = vec![Scope::Assigned];
        assert!(vm.apply(&mut s)); // host changed
        assert_eq!(s.host, "ghe.example.com");
        assert_eq!(s.default_scopes, vec![Scope::Assigned]);
    }

    #[test]
    fn vm_flattens_the_client_id_and_reports_no_change_when_the_host_stays() {
        let mut s = Settings {
            client_id: Some("cid".into()),
            ..Settings::default()
        };
        let mut vm = settings_vm(&s);
        assert_eq!(vm.client_id, "cid");
        vm.client_id = String::new();
        assert!(!vm.apply(&mut s), "the host did not change");
        assert_eq!(s.client_id, None, "an empty client id flattens to None");
    }

    #[test]
    fn theme_defaults_to_system() {
        assert_eq!(Theme::default(), Theme::System);
        assert_eq!(Settings::default().theme, Theme::System);
        let _ = SettingsVm::default();
    }
}
