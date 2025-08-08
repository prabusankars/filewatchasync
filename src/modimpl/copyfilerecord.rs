use serde::{Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct CopiedFileRecord {
    pub timestamp: String,
    pub source_path: String,
    pub destination_path: String,
    pub file_size: u64,
    pub check_sum: String,
    pub des_check_sum: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub end_timestamp:Option<String>,
    pub is_lst_referenced: Option<bool>, // Flag to indicate if file was referenced from .lst
    pub lst_source_file: Option<String>, // Source .lst file that referenced this file
}