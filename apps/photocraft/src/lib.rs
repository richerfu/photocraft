//! OpenHarmony host for the native PhotoCraft editor.
#![cfg(target_env = "ohos")]
mod app_dirs {
    use std::{path::PathBuf, sync::OnceLock};
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    pub fn set_ohos_dir(path: PathBuf) {
        let _ = DIRECTORY.set(path);
    }
    pub fn config_dir() -> Option<PathBuf> {
        DIRECTORY.get().cloned()
    }
}
mod control_server;
mod crash_guard;
mod ohos;
mod ohos_files;
mod services;

pub use ohos::start;
