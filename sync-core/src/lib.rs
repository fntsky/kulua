pub mod adb_cmd;
pub mod autostart;
pub mod ipc;
pub mod settings;

pub mod app;
pub mod audio_player;
pub mod cli;
pub mod device_refresh;
pub mod notification;
pub mod protocol;
pub mod scrcpy;
pub mod session;
pub mod types;
pub mod wireless_pair;
pub use app::Core;
