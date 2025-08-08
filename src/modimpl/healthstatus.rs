use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub last_check: DateTime<Utc>,
    pub files_processed_last_period: u64,
    pub errors_last_period: u64,
    pub is_healthy: bool,
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self {
            last_check: Utc::now(),
            files_processed_last_period: 0,
            errors_last_period: 0,
            is_healthy: true,
        }
    }
}