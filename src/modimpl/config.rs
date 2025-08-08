use serde::{Deserialize, Serialize};
use crate::WatchFolder;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub watch_folders: Vec<WatchFolder>,
    pub max_concurrent_copies: usize,
    pub health_check_interval_minutes: u64,
    pub file_stability_wait_seconds: u64,
    pub csv_output_path: String,
    pub csv_polling_time: u64,
    pub log_file_path: String,
    pub lst_file_stability_wait_seconds: Option<u64>, // Additional wait time for .lst files
}