pub mod adb;
pub mod config;
pub mod save;
pub mod sync;
pub mod theme;
pub mod uad_lists;
pub mod update;
pub mod utils;

use std::path::PathBuf;
use std::sync::LazyLock;
pub static CONFIG_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| utils::setup_uad_dir(&dirs::config_dir().expect("Can't detect config dir")));
pub static CACHE_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| utils::setup_uad_dir(&dirs::cache_dir().expect("Can't detect cache dir")));
