//! Shared widgets, one module per widget.

#[cfg(unix)]
pub mod ai_dialogs;
#[cfg(unix)]
pub mod ai_panel;
#[cfg(unix)]
pub mod ai_repo_dialog;
pub mod checks;
pub mod command_palette;
pub mod comment_composer;
pub mod commit_list;
pub mod diff_view;
pub mod file_header;
pub mod file_tree;
pub mod filter_bar;
pub mod image_diff;
#[cfg(unix)]
pub mod local_repos;
pub mod markdown;
pub mod merge_panel;
pub mod metadata_panel;
pub mod pr_row;
pub mod review_panel;
#[cfg(unix)]
pub mod settings_ai_tab;
pub mod settings_window;
pub mod thread_view;
pub mod title_bar;
pub mod toast;
pub mod top_bar;
