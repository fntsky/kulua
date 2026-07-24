pub mod adb_cmd;
pub mod ipc;

pub mod cli;
pub mod app;
pub mod notification;
pub mod protocol;
pub mod device_refresh;
pub mod scrcpy;
pub mod session;
pub mod audio_player;
pub mod types;
pub mod wireless_pair;
pub use app::Core;
