pub mod board;
mod board_clipboard;
mod board_code_copy;
pub mod board_interaction;
mod board_lifecycle;
#[cfg(test)]
mod board_lifecycle_reuse_tests;
#[cfg(test)]
mod board_lifecycle_tests;
mod board_links;
pub mod board_messages;
mod board_metadata;
mod board_notifications;
mod board_selection;
mod board_time;
pub mod board_tui;
pub mod broker;
pub mod clients;
pub use clients::claude_discovery;
mod codex_admission;
pub mod db;
pub mod delivery;
pub mod opencode_install;
mod opencode_state;
pub use clients::codex_discovery as discovery;
pub use clients::lifecycle as hook;
pub mod mcp;
pub mod migrations;
pub mod network;
pub mod protocol;
pub mod service;
pub mod update;

pub mod monitor;
mod monitor_board;
mod monitor_data;

mod pairing_notifications;

/// Application version selected by the release tag or packaged source metadata.
pub const VERSION: &str = env!("AGENT_BRIDGE_VERSION");

/// Source revision provided by the release build workflow.
pub const BUILD_COMMIT: Option<&str> = option_env!("AGENT_BRIDGE_BUILD_COMMIT");
