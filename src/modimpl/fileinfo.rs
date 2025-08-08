use std::time::{SystemTime};
use std::path::{PathBuf};

#[derive(Clone,Debug, Eq, PartialEq, Hash)]
pub struct FileInfo {
    pub path: PathBuf,
    pub modified_time: SystemTime,
    pub check_sum:String,
}