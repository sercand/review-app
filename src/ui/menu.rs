//! The macOS menu bar.
//!
//! [`MENUS`] describes the menus as data: every item names an action the
//! command registry or [`crate::app`] dispatches, so a menu item does exactly
//! what the same shortcut or palette entry does. [`NativeMenu`] builds the
//! real menu bar from it on macOS (through `muda`) and is a no-op elsewhere,
//! where the window keeps no menu bar.
//!
//! Only `⌘` shortcuts become menu key equivalents: a menu claims its key
//! before the window sees it, so a single-key shortcut (`j`, `v`) there would
//! swallow typing in every text field. There is no Edit menu for the same
//! reason — its `⌘C`/`⌘V` would go to AppKit's responder chain instead of the
//! egui text field that has focus.

/// One menu item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    /// The action it dispatches.
    pub action: &'static str,
    /// The item's title.
    pub label: &'static str,
    /// Its key equivalent in `muda` syntax, e.g. `"CmdOrCtrl+K"`.
    pub accelerator: Option<&'static str>,
    /// Whether it only applies with a pull request open.
    pub needs_pr: bool,
}

/// One entry of a menu: an item or a separator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// An item.
    Item(Item),
    /// A separator line.
    Separator,
}

const fn item(action: &'static str, label: &'static str) -> Entry {
    Entry::Item(Item {
        action,
        label,
        accelerator: None,
        needs_pr: false,
    })
}

const fn key(action: &'static str, label: &'static str, accelerator: &'static str) -> Entry {
    Entry::Item(Item {
        action,
        label,
        accelerator: Some(accelerator),
        needs_pr: false,
    })
}

const fn pr(action: &'static str, label: &'static str) -> Entry {
    Entry::Item(Item {
        action,
        label,
        accelerator: None,
        needs_pr: true,
    })
}

const fn pr_key(action: &'static str, label: &'static str, accelerator: &'static str) -> Entry {
    Entry::Item(Item {
        action,
        label,
        accelerator: Some(accelerator),
        needs_pr: true,
    })
}

/// The app's own menus, after the application menu and before Window.
pub const MENUS: &[(&str, &[Entry])] = &[
    (
        "View",
        &[
            key("palette", "Command Palette…", "CmdOrCtrl+K"),
            key("toggle_theme", "Toggle Dark Mode", "CmdOrCtrl+Shift+D"),
            Entry::Separator,
            pr("split", "Split Diff"),
            pr("unified", "Unified Diff"),
            pr("toggle_side_pane", "Show or Hide Details"),
            Entry::Separator,
            key("refresh", "Refresh", "CmdOrCtrl+R"),
        ],
    ),
    (
        "Pull Request",
        &[
            pr_key("back", "Back to Pull Requests", "CmdOrCtrl+["),
            pr_key("open_github", "Open on GitHub", "CmdOrCtrl+Shift+O"),
            Entry::Separator,
            pr("next_file", "Next File"),
            pr("prev_file", "Previous File"),
            pr("next_hunk", "Next Change"),
            pr("prev_hunk", "Previous Change"),
            pr_key("find", "Find in File", "CmdOrCtrl+F"),
            Entry::Separator,
            pr("toggle_viewed", "Mark File Viewed"),
            pr("mark_viewed_and_next", "Mark Viewed and Go to Next"),
            pr("comment", "Comment on Line"),
            Entry::Separator,
            pr("submit_review", "Finish Review…"),
        ],
    ),
    ("Help", &[item("help", "Keyboard Shortcuts")]),
];

/// The application menu's own items, between About and the system items.
pub const APP_MENU: &[Entry] = &[
    key("settings", "Settings…", "CmdOrCtrl+,"),
    Entry::Separator,
    item("sign_out", "Sign Out"),
];

/// Every item, across all menus.
pub fn items() -> impl Iterator<Item = &'static Item> {
    APP_MENU
        .iter()
        .chain(MENUS.iter().flat_map(|(_, entries)| entries.iter()))
        .filter_map(|entry| match entry {
            Entry::Item(item) => Some(item),
            Entry::Separator => None,
        })
}

/// The action a menu event's id names, if it is one of ours.
pub fn action_for(id: &str) -> Option<&'static str> {
    items()
        .find(|item| item.action == id)
        .map(|item| item.action)
}

#[cfg(target_os = "macos")]
mod native {
    use std::sync::mpsc;

    use eframe::egui;
    use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    use super::{Entry, APP_MENU, MENUS};

    /// The installed menu bar and the channel its clicks arrive on.
    pub struct NativeMenu {
        // Kept alive: dropping the menu would tear the menu bar down.
        _menu: Menu,
        pr_items: Vec<MenuItem>,
        pr_open: Option<bool>,
        events: mpsc::Receiver<String>,
    }

    impl NativeMenu {
        /// Build the menu bar and make it the app's. Clicks wake `ctx`.
        pub fn install(ctx: &egui::Context) -> Option<NativeMenu> {
            let menu = Menu::new();
            let mut pr_items = Vec::new();
            let mut add = |submenu: &Submenu, entries: &[Entry]| {
                for entry in entries {
                    match entry {
                        Entry::Separator => {
                            let _ = submenu.append(&PredefinedMenuItem::separator());
                        }
                        Entry::Item(item) => {
                            let accelerator = item.accelerator.and_then(|a| a.parse().ok());
                            let menu_item =
                                MenuItem::with_id(item.action, item.label, true, accelerator);
                            let _ = submenu.append(&menu_item);
                            if item.needs_pr {
                                pr_items.push(menu_item);
                            }
                        }
                    }
                }
            };

            let name = "gh-review-desktop";
            let app = Submenu::new(name, true);
            let _ = app.append(&PredefinedMenuItem::about(
                None,
                Some(AboutMetadata {
                    name: Some(name.to_owned()),
                    version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                    ..Default::default()
                }),
            ));
            let _ = app.append(&PredefinedMenuItem::separator());
            add(&app, APP_MENU);
            for item in [
                PredefinedMenuItem::separator(),
                PredefinedMenuItem::services(None),
                PredefinedMenuItem::separator(),
                PredefinedMenuItem::hide(None),
                PredefinedMenuItem::hide_others(None),
                PredefinedMenuItem::show_all(None),
                PredefinedMenuItem::separator(),
                PredefinedMenuItem::quit(None),
            ] {
                let _ = app.append(&item);
            }
            let _ = menu.append(&app);

            for (title, entries) in MENUS.iter().take(MENUS.len() - 1) {
                let submenu = Submenu::new(*title, true);
                add(&submenu, entries);
                let _ = menu.append(&submenu);
            }

            let window = Submenu::new("Window", true);
            for item in [
                PredefinedMenuItem::minimize(None),
                PredefinedMenuItem::maximize(None),
                PredefinedMenuItem::fullscreen(None),
                PredefinedMenuItem::separator(),
                PredefinedMenuItem::bring_all_to_front(None),
            ] {
                let _ = window.append(&item);
            }
            let _ = menu.append(&window);

            // Help comes last, after Window, as macOS expects.
            if let Some((title, entries)) = MENUS.last() {
                let help = Submenu::new(*title, true);
                add(&help, entries);
                let _ = menu.append(&help);
                help.set_as_help_menu_for_nsapp();
            }
            window.set_as_windows_menu_for_nsapp();
            menu.init_for_nsapp();

            let (sender, events) = mpsc::channel();
            let ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                let _ = sender.send(event.id().0.clone());
                ctx.request_repaint();
            }));

            Some(NativeMenu {
                _menu: menu,
                pr_items,
                pr_open: None,
                events,
            })
        }

        /// Enable the pull-request items only while one is open.
        pub fn set_pr_open(&mut self, open: bool) {
            if self.pr_open == Some(open) {
                return;
            }
            for item in &self.pr_items {
                item.set_enabled(open);
            }
            self.pr_open = Some(open);
        }

        /// The ids of the items clicked since the last call.
        pub fn drain(&mut self) -> Vec<String> {
            self.events.try_iter().collect()
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use eframe::egui;

    /// No menu bar outside macOS.
    pub struct NativeMenu;

    impl NativeMenu {
        /// Nothing to install.
        pub fn install(_ctx: &egui::Context) -> Option<NativeMenu> {
            None
        }

        /// Nothing to enable.
        pub fn set_pr_open(&mut self, _open: bool) {}

        /// Nothing is ever clicked.
        pub fn drain(&mut self) -> Vec<String> {
            Vec::new()
        }
    }
}

pub use native::NativeMenu;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_items_map_to_actions_and_only_take_command_shortcuts() {
        assert_eq!(action_for("palette"), Some("palette"));
        assert_eq!(action_for("open_github"), Some("open_github"));
        assert_eq!(action_for("nonsense"), None);
        for item in items() {
            if let Some(accelerator) = item.accelerator {
                // A plain-letter key equivalent would eat typing in every
                // text field, since the menu sees keys before the window.
                assert!(
                    accelerator.starts_with("CmdOrCtrl+"),
                    "{} takes a bare key: {accelerator}",
                    item.label
                );
            }
        }
        let actions: std::collections::HashSet<_> = items().map(|item| item.action).collect();
        assert_eq!(actions.len(), items().count(), "each action appears once");
    }

    #[test]
    fn registry_actions_in_the_menu_exist_in_the_registry() {
        let registry: Vec<_> = crate::ui::command::registry()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        let app_level = ["refresh", "open_github", "back", "toggle_theme", "sign_out"];
        for item in items() {
            assert!(
                registry.contains(&item.action) || app_level.contains(&item.action),
                "{} dispatches nothing",
                item.action
            );
        }
    }
}
