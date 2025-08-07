use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use glob::Pattern;
use tokio::fs;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::{interval, sleep};
use tokio::signal;
use serde::{Deserialize, Serialize};
use notify::{Watcher, RecursiveMode,EventKind};
use notify::event::{CreateKind, ModifyKind};
use csv::Writer;
use log::{info, warn, error, debug};
use anyhow::{Result, Context};
use clap::Parser;
use chrono::{DateTime, Utc};

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
#[derive(Clone,Debug, Eq, PartialEq, Hash)]
pub struct FileInfo {
    path: PathBuf,
    modified_time: SystemTime,
    check_sum:String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchFolder {
    pub source_path: String,
    pub destination_path: String,
    pub file_patterns: Vec<String>,
    pub recursive: bool,
}

impl WatchFolder {
    pub fn compile_patterns(&self) -> Result<Vec<Pattern>> {
        let mut compiled_patterns = Vec::new();
        
        for pattern_str in &self.file_patterns {
            // Compile original pattern
            let original_pattern = Pattern::new(pattern_str)
                .with_context(|| format!("Invalid glob pattern: {}", pattern_str))?;
            compiled_patterns.push(original_pattern);
            
            // Also compile lowercase version if different
            let lowercase_pattern_str = pattern_str.to_lowercase();
            if lowercase_pattern_str != *pattern_str {
                let lowercase_pattern = Pattern::new(&lowercase_pattern_str)
                    .with_context(|| format!("Invalid glob pattern (lowercase): {}", lowercase_pattern_str))?;
                compiled_patterns.push(lowercase_pattern);
            }
            
            // Also compile uppercase version if different
            let uppercase_pattern_str = pattern_str.to_uppercase();
            if uppercase_pattern_str != *pattern_str && uppercase_pattern_str != lowercase_pattern_str {
                let uppercase_pattern = Pattern::new(&uppercase_pattern_str)
                    .with_context(|| format!("Invalid glob pattern (uppercase): {}", uppercase_pattern_str))?;
                compiled_patterns.push(uppercase_pattern);
            }
        }
        
        Ok(compiled_patterns)
    }
}

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

#[derive(Debug)]
pub struct FileMonitor {
    config: Config,
    copied_files: Arc<RwLock<Vec<CopiedFileRecord>>>,
    pending_files: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
    pending_lst_files: Arc<Mutex<HashMap<PathBuf, SystemTime>>>, // Separate queue for .lst files
    processed_files: Arc<Mutex<HashSet<FileInfo>>>, // Track processed files to avoid duplicates
    semaphore: Arc<Semaphore>,
    health_status: Arc<RwLock<HealthStatus>>,
}

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

#[derive(Parser, Debug)]
#[command(name = "file-monitor")]
#[command(about = "A file monitoring and copying system")]
pub struct Args {
    #[arg(short, long, default_value = "config.json")]
    pub config: String,
}

impl FileMonitor {
    pub fn new(config: Config) -> Result<Self> {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_copies));
        
        Ok(Self {
            config,
            copied_files: Arc::new(RwLock::new(Vec::new())),
            pending_files: Arc::new(Mutex::new(HashMap::new())),
            pending_lst_files: Arc::new(Mutex::new(HashMap::new())),
            processed_files: Arc::new(Mutex::new(HashSet::new())),
            semaphore,
            health_status: Arc::new(RwLock::new(HealthStatus::default())),
        })
    }

    pub async fn start(&self) -> Result<()> {
        self.setup_logging().await?;
        info!("🚀 Starting file monitor system with Configuration: {:?}", self.config);
        // Start health check task
        let health_task = self.start_health_check_task();
        // Start file watchers for each configured folder
        let mut watch_tasks = Vec::new();
        for watch_folder in &self.config.watch_folders {
            let task = self.start_folder_watcher(watch_folder.clone()).await?;
            watch_tasks.push(task);
        }
        
        // Start file processor task
        let processor_task = self.start_file_processor();

        // Start .lst file processor task
        let lst_processor_task = self.start_lst_file_processor();
        
        // Start CSV writer task
        let csv_writer_task = self.start_csv_writer();
        
        info!("✅ All tasks started successfully");
        
        // Wait for Ctrl+C
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("⚠️  Received Ctrl+C, waiting for pending files to process...");
                
                // Give the processor a chance to pick up any files added just before Ctrl+C
                tokio::time::sleep(Duration::from_secs(self.config.file_stability_wait_seconds + 2)).await;
                // Wait for the pending files queue to be empty
                loop {
                    let pending_count = self.pending_files.lock().await.len();
                    let lst_pending_count = self.pending_lst_files.lock().await.len();
                    if pending_count == 0  && lst_pending_count ==0 {
                        break;
                    }
                    info!("{} files and {} .lst files still pending processing...", pending_count, lst_pending_count);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            _ = health_task => {
                error!("Health check task terminated unexpectedly");
            }
            _ = processor_task => {
                error!("File processor task terminated unexpectedly");
            }
            _ = lst_processor_task =>{
                error!("LST processor task terminated unexpectedly");
            }
            _ = csv_writer_task => {
                error!("CSV writer task terminated unexpectedly");
            }
        }
        
        self.shutdown().await?;
        Ok(())
    }

    async fn setup_logging(&self) -> Result<()> {
        use tracing_appender::rolling::{RollingFileAppender, Rotation};
        use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
        let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
        let filename_prefix = "file_watcher_";
        let tmp_dir = &self.config.log_file_path.to_string();
        tracing_subscriber::registry()
        .with(fmt::layer()
            //.with_target(true)
            .with_thread_ids(true)
            .with_line_number(true)
        )
        .with(filter)
        .with(tracing_subscriber::fmt::layer()
            .with_writer(RollingFileAppender::builder()
                .rotation(Rotation::DAILY)
                .filename_prefix(filename_prefix)
                .filename_suffix("log")
                .build(&tmp_dir).unwrap()))
        .init();
        info!("🖹 Logger Initialized");
        Ok(())
    }

    async fn start_folder_watcher(&self, watch_folder: WatchFolder) -> Result<tokio::task::JoinHandle<()>> {
        let pending_files = Arc::clone(&self.pending_files);
        let pending_lst_files = Arc::clone(&self.pending_lst_files);
        let processed_files = Arc::clone(&self.processed_files);
        let config = self.config.clone();
        
        let task = tokio::spawn(async move {
            if let Err(e) = Self::watch_folder_impl(watch_folder, pending_files,pending_lst_files, processed_files, config).await {
                error!("Folder watcher error: {}", e);
            }
        });
        
        Ok(task)
    }
    

    async fn watch_folder_impl(
        watch_folder: WatchFolder,
        pending_files: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
        pending_lst_files:Arc<Mutex<HashMap<PathBuf,SystemTime>>>,
        processed_files: Arc<Mutex<HashSet<FileInfo>>>,
        _config: Config,
    ) -> Result<()> {
        use notify::Event;
        use tokio::sync::mpsc;
        
        // Compile glob patterns once
        let compiled_patterns = watch_folder.compile_patterns()
            .context("Failed to compile glob patterns")?;
        
        let (tx, mut rx) = mpsc::channel(1000);
        
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            match res {
                Ok(event) => {
                    debug!("Raw file event received: {:?}", event);
                    if let Err(e) = tx.blocking_send(event) {
                        error!("Failed to send file event: {}", e);
                    }
                }
                Err(e) => {
                    error!("Watch error: {}", e);
                }
            }
        })?;
        
        let watch_path = Path::new(&watch_folder.source_path);
        
        // Check if the path exists
        if !watch_path.exists() {
            error!("Watch path does not exist: {}", watch_folder.source_path);
            return Err(anyhow::anyhow!("Watch path does not exist: {}", watch_folder.source_path));
        }
        
        let mode = if watch_folder.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        
        watcher.watch(watch_path, mode)
            .context(format!("Failed to start watching {}", watch_folder.source_path))?;
        
        info!("👀 Started watching folder: {} , patterns:{:?} (recursive: {})", watch_folder.source_path,watch_folder.file_patterns,watch_folder.recursive);
        // Event deduplication - track recent events to avoid processing duplicates
        let mut recent_events: HashMap<PathBuf, SystemTime> = HashMap::new();
        let event_dedup_window = Duration::from_millis(500); // 500ms deduplication window
        
        while let Some(event) = rx.recv().await {
            let now = SystemTime::now();
            
            // Clean up old events from dedup tracking
            recent_events.retain(|_, time| {
                now.duration_since(*time).unwrap_or(Duration::ZERO) <= event_dedup_window
            });
            
            // info!("Processing file event: {:?}", event);
            
            match event.kind {
                EventKind::Create(CreateKind::File) | 
                EventKind::Modify(ModifyKind::Data(_)) | 
                EventKind::Modify(ModifyKind::Any) => {
                    for path in event.paths {
                        // Skip if not a file
                        if !path.is_file() {
                            continue;
                        }
                        // Check for recent duplicate event
                        if let Some(&last_time) = recent_events.get(&path) {
                            if now.duration_since(last_time).unwrap_or(Duration::ZERO) <= event_dedup_window {
                                debug!("Skipping duplicate event for: {:?}", path);
                                continue;
                            }
                        }
                        // Record this event
                        recent_events.insert(path.clone(), now);
                        // info!("Checking file: {:?}", path);
                        // Check if file matches any pattern
                        if Self::matches_glob_patterns(&path, &compiled_patterns) {
                            info!("✨ File matches pattern, checking if already processed: {:?}", path);
                            // Check if already processed
                            let processed = processed_files.lock().await;
                            for file_info in processed.iter() {
                                if file_info.path == path && 
                                file_info.check_sum == Self::calculate_checksum(&path).await 
                                {
                                    info!("✨ File already processed, skipping: {:?}", &path);
                                    continue;
                                }
                            } 
                            if path.extension().and_then(|s| s.to_str()) ==Some("lst") {
                                info!("✚  Adding .lst file to pending queue: {:?}", path);
                                let mut pending_lst = pending_lst_files.lock().await;
                                pending_lst.insert(path.clone(), now);
                            }else{
                                info!("✚  Adding file to pending queue: {:?}", path);
                                let mut pending = pending_files.lock().await;
                                pending.insert(path.clone(), now);
                            }                          
                            
                        } else {
                            info!("❗  File does not match any patterns: {:?}", path);
                        }
                    }
                }
                EventKind::Create(CreateKind::Folder) => {
                    debug!("Folder created: {:?}", event.paths);
                }
                EventKind::Remove(_) => {
                    debug!("File/folder removed: {:?}", event.paths);
                    // Clean up from our tracking
                    for path in &event.paths {
                        let mut processed = processed_files.lock().await;
                        let files_to_remove:Vec<_> = processed.iter()
                                                    .filter(|file_info| file_info.path == *path)
                                                    .cloned()
                                                    .collect();
                        for file_info in files_to_remove {
                            processed.remove(&file_info);
                        }
                        // processed.remove(path);
                        let mut pending = pending_files.lock().await;
                        pending.remove(path);
                    }
                }
                _ => {
                    debug!("Other file event: {:?}", event);
                }
            }
        }
        
        warn!("❗ File watcher event loop ended for {}", watch_folder.source_path);
        Ok(())
    }

    async fn calculate_checksum(path: &Path) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        // use std::io::Read;
        use tokio::io::AsyncReadExt;

        let mut file = match fs::File::open(path).await {
            Ok(f) => f,
            Err(_) => return String::new(),
        };

        let mut hasher = DefaultHasher::new();
        let mut buffer = [0; 8192];
        
        loop {
            match file.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => buffer[..n].hash(&mut hasher),
                Err(_) => return String::new(),
            }
        }

        format!("{:x}", hasher.finish())
    }
    
    fn matches_glob_patterns(path: &Path, compiled_patterns: &[Pattern]) -> bool {
        let filename = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let filename_lower = filename.to_lowercase();
        debug!("Checking file '{}' against {} compiled patterns", filename, compiled_patterns.len());
        
        for pattern in compiled_patterns {
            //check both original and lowercase
            let pattern_matches =pattern.matches(filename) || pattern.matches(&filename_lower);
            if pattern_matches {
                info!("✅ Pattern '{}' matches file '{}'", pattern.as_str(), filename);
                return true;
            } else {
                debug!("❌ Pattern '{}' does not match file '{}'", pattern.as_str(), filename);
            }
        }
        
        false
    }

    fn start_file_processor(&self) -> tokio::task::JoinHandle<()> {
        let pending_files = Arc::clone(&self.pending_files);
        let processed_files = Arc::clone(&self.processed_files);
        let copied_files = Arc::clone(&self.copied_files);
        let semaphore = Arc::clone(&self.semaphore);
        let health_status = Arc::clone(&self.health_status);
        let config = self.config.clone();
        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(1));
            
            loop {
                interval.tick().await;
                
                let files_to_process = {
                    let mut pending = pending_files.lock().await;
                    let now = SystemTime::now();
                    let stable_time = Duration::from_secs(config.file_stability_wait_seconds);
                    
                    // Debug: Print pending files
                    if !pending.is_empty() {
                        debug!("Checking {} files in pending queue", pending.len());
                    }
                    
                    let stable_files: Vec<PathBuf> = pending
                        .iter()
                        .filter(|(_, &time)| {
                            let elapsed = now.duration_since(time).unwrap_or(Duration::ZERO);
                            elapsed >= stable_time
                        })
                        .map(|(path, _)| path.clone())
                        .collect();
                    
                    // Remove stable files from pending
                    for path in &stable_files {
                        pending.remove(path);
                        debug!("Moving file to processing: {:?}", path);
                    }
                    
                    stable_files
                };
                                
                for file_path in files_to_process {
                    let semaphore = Arc::clone(&semaphore);
                    let config = config.clone();
                    let processed_files = Arc::clone(&processed_files);
                    let copied_files = Arc::clone(&copied_files);
                    let health_status = Arc::clone(&health_status);
                    tokio::spawn(async move {
                        let _permit = semaphore.acquire().await.unwrap();
                        
                        if let Err(e) = Self::process_file(
                            &file_path,
                            &config,
                            Arc::clone(&copied_files),
                            Arc::clone(&health_status),
                        ).await {
                            error!("Failed to process file: {}", e);
                        }
                        // Mark as processed if it was successfully added to copied_files
                        if copied_files.read().await.iter().any(|r| PathBuf::from(&r.source_path) == file_path) {
                            let mut processed = processed_files.lock().await;
                            let time = SystemTime::now();
                            let check_sum = Self::calculate_checksum(&file_path).await;
                            let file_info = FileInfo { path:file_path.clone(), modified_time: time , check_sum: check_sum};
                            processed.insert(file_info);//(file_path.clone());
                        }

                    });
                }
            }
        })
    }

    async fn process_file(
        source_path: &PathBuf,
        config: &Config,
        copied_files: Arc<RwLock<Vec<CopiedFileRecord>>>,
        health_status: Arc<RwLock<HealthStatus>>,
    ) -> Result<()> {
        info!("Processing file: {:?}", source_path);
        
        // Find matching watch folder
        let watch_folder = config
            .watch_folders
            .iter()
            .find(|wf| source_path.starts_with(&wf.source_path))
            .context("No matching watch folder found")?;
        
        // Validate file is complete and not being written to
        if !Self::is_file_complete(&source_path).await? {
            warn!("File is not complete yet: {:?}", source_path);
            return Ok(());
        }
        
        // Calculate destination path
        let relative_path = source_path.strip_prefix(&watch_folder.source_path)?;
        let dest_path = Path::new(&watch_folder.destination_path).join(relative_path);
        
        // Ensure destination directory exists
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        
        let file_size = fs::metadata(&source_path).await?.len();
        let start_time = Utc::now();
        
        let mut record = CopiedFileRecord {
            timestamp: start_time.to_rfc3339(),
            source_path: source_path.to_string_lossy().to_string(),
            destination_path: dest_path.to_string_lossy().to_string(),
            file_size,
            check_sum: Self::calculate_checksum(&source_path).await,
            des_check_sum: None,
            status: "copying".to_string(),
            error_message: None,
            end_timestamp: None,
            is_lst_referenced: None,
            lst_source_file: None,
        };
        
        // Copy the file
        match fs::copy(&source_path, &dest_path).await {
            Ok(_) => {
                record.status = "success".to_string();
                info!("🗹  Successfully copied file: {:?} -> {:?}", source_path, dest_path);
                record.des_check_sum = Some(Self::calculate_checksum(&dest_path).await);
                record.end_timestamp = Some(Utc::now().to_rfc3339());
                // Update health status
                let mut health = health_status.write().await;
                health.files_processed_last_period += 1;
            }
            Err(e) => {
                record.status = "error".to_string();
                record.error_message = Some(e.to_string());
                error!("🗷  Failed to copy file: {:?} -> {:?}, error: {}", source_path, dest_path, e);
                
                // Update health status
                let mut health = health_status.write().await;
                health.errors_last_period += 1;
            }
        }
        
        // Add record to copied files
        let mut copied = copied_files.write().await;
        copied.push(record);
        
        Ok(())
    }
    // New function to start .lst file processor
    fn start_lst_file_processor(&self) -> tokio::task::JoinHandle<()> {
        let pending_lst_files = Arc::clone(&self.pending_lst_files);
        let processed_files = Arc::clone(&self.processed_files);
        let copied_files = Arc::clone(&self.copied_files);
        let pending_files = Arc::clone(&self.pending_files);
        let semaphore = Arc::clone(&self.semaphore);
        let health_status = Arc::clone(&self.health_status);
        let config = self.config.clone();
        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(1));
            
            loop {
                interval.tick().await;
                let lst_files_to_process = {
                    let mut pending_lst = pending_lst_files.lock().await;
                    let now = SystemTime::now();
                    let stable_time = Duration::from_secs(
                        config.lst_file_stability_wait_seconds.unwrap_or(config.file_stability_wait_seconds)
                    );
                    
                    if !pending_lst.is_empty() {
                        debug!("Checking {} .lst files in pending queue", pending_lst.len());
                    }
                    
                    let stable_lst_files: Vec<PathBuf> = pending_lst
                        .iter()
                        .filter(|(_, &time)| {
                            let elapsed = now.duration_since(time).unwrap_or(Duration::ZERO);
                            elapsed >= stable_time
                        })
                        .map(|(path, _)| path.clone())
                        .collect();
                    
                    // Remove stable .lst files from pending
                    for path in &stable_lst_files {
                        pending_lst.remove(path);
                        info!("Moving .lst file to processing: {:?}", path);
                    }
                    
                    stable_lst_files
                };
                                
                for lst_file_path in lst_files_to_process {
                    let semaphore = Arc::clone(&semaphore);
                    let config = config.clone();
                    let processed_files = Arc::clone(&processed_files);
                    let copied_files = Arc::clone(&copied_files);
                    let pending_files = Arc::clone(&pending_files);
                    let health_status = Arc::clone(&health_status);
                    
                    tokio::spawn(async move {
                        let _permit = semaphore.acquire().await.unwrap();
                        
                        if let Err(e) = Self::process_lst_file(
                            &lst_file_path,
                            &config,
                            Arc::clone(&copied_files),
                            Arc::clone(&pending_files),
                            Arc::clone(&health_status),
                        ).await {
                            error!("Failed to process .lst file: {}", e);
                        }
                        
                        // Mark .lst file as processed
                        let mut processed = processed_files.lock().await;
                        let time = SystemTime::now();
                        let check_sum = Self::calculate_checksum(&lst_file_path).await;
                        let file_info = FileInfo { 
                            path: lst_file_path.clone(), 
                            modified_time: time, 
                            check_sum: check_sum
                        };
                        processed.insert(file_info);
                    });
                }
            }
        })
    }

    // New function to process .lst files
    async fn process_lst_file(
        lst_file_path: &PathBuf,
        config: &Config,
        copied_files: Arc<RwLock<Vec<CopiedFileRecord>>>,
        pending_files: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
        health_status: Arc<RwLock<HealthStatus>>,
    ) -> Result<()> {
        info!("📋 Processing .lst file: {:?}", lst_file_path);
        
        // First, copy the .lst file itself
        if let Err(e) = Self::process_file(
            lst_file_path,
            config,
            Arc::clone(&copied_files),
            Arc::clone(&health_status),
        ).await {
            error!("Failed to copy .lst file: {}", e);
            return Err(e);
        }
        
        // Read the .lst file content
        let content = match fs::read_to_string(lst_file_path).await {
            Ok(content) => content,
            Err(e) => {
                error!("🗷  Failed to read .lst file: {:?}, error: {}", lst_file_path, e);
                return Err(anyhow::anyhow!("Failed to read .lst file: {}", e));
            }
        };
        
        // Parse file paths from .lst file (assuming one file path per line)
        let file_paths: Vec<String> = content
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#')) // Skip empty lines and comments
            .map(|line| line.to_string())
            .collect();
        
        info!("📋 Found {} file references in .lst file: {:?}", file_paths.len(), lst_file_path);
        
        // Process each file referenced in the .lst
        for file_path_str in file_paths {
            let referenced_file_path = PathBuf::from(&file_path_str);
            
            // Check if the file path is absolute or relative
            let absolute_file_path = if referenced_file_path.is_absolute() {
                referenced_file_path
            } else {
                // If relative, resolve it relative to the .lst file's directory
                if let Some(lst_parent) = lst_file_path.parent() {
                    lst_parent.join(referenced_file_path)
                } else {
                    referenced_file_path
                }
            };
            
            info!("📄 Checking referenced file: {:?}", absolute_file_path);
            
            // Check if file exists and is stable
            if !absolute_file_path.exists() {
                warn!("⚠️  Referenced file does not exist: {:?}", absolute_file_path);
                
                // Create an error record for missing file
                let error_record = CopiedFileRecord {
                    timestamp: Utc::now().to_rfc3339(),
                    source_path: absolute_file_path.to_string_lossy().to_string(),
                    destination_path: "N/A".to_string(),
                    file_size: 0,
                    check_sum: String::new(),
                    des_check_sum: None,
                    status: "error".to_string(),
                    error_message: Some("Referenced file does not exist".to_string()),
                    end_timestamp: Some(Utc::now().to_rfc3339()),
                    is_lst_referenced: Some(true),
                    lst_source_file: Some(lst_file_path.to_string_lossy().to_string()),
                };
                
                let mut copied = copied_files.write().await;
                copied.push(error_record);
                
                let mut health = health_status.write().await;
                health.errors_last_period += 1;
                
                continue;
            }
            
            // Check if file is stable
            if !Self::is_file_complete(&absolute_file_path).await.unwrap_or(false) {
                warn!("⚠️  Referenced file is not stable yet, adding to pending: {:?}", absolute_file_path);
                let mut pending = pending_files.lock().await;
                pending.insert(absolute_file_path, SystemTime::now());
                continue;
            }
            
            // Process the referenced file
            if let Err(e) = Self::process_file(
                &absolute_file_path,
                config,
                Arc::clone(&copied_files),
                Arc::clone(&health_status),
            ).await {
                error!("Failed to process referenced file {:?}: {}", absolute_file_path, e);
            }
        }
        
        info!("📋 Completed processing .lst file: {:?}", lst_file_path);
        Ok(())
    }
    async fn is_file_complete(path: &Path) -> Result<bool> {
        // Simple file completeness check - compare file size over time
        let size1 = fs::metadata(path).await?.len();
        sleep(Duration::from_millis(100)).await;
        let size2 = fs::metadata(path).await?.len();
        
        Ok(size1 == size2 && size1 > 0)
    }

    fn start_health_check_task(&self) -> tokio::task::JoinHandle<()> {
        let health_status = Arc::clone(&self.health_status);
        let interval_minutes = self.config.health_check_interval_minutes;
        let processed_files = Arc::clone(&self.processed_files);        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(interval_minutes * 60));
            
            loop {
                interval.tick().await;
                let processed_files = Arc::clone(&processed_files);
                let mut processed = processed_files.lock().await;
                processed.clear();
                let mut health = health_status.write().await;
                let now = Utc::now();
                // Simple health check logic
                health.is_healthy = !health.errors_last_period > 0;
                
                info!(
                    "💓 Health check Files since last health check - Processed: {}, Errors: {}, Healthy: {}",
                    health.files_processed_last_period,
                    health.errors_last_period,
                    health.is_healthy
                );
                // Reset counters
                health.files_processed_last_period = 0;
                health.errors_last_period = 0;
                health.last_check = now;
            }
        })
    }
    
    fn get_today_csv_path(base_path: String, date: &str) -> String {
        format!("{}.{}.csv", base_path, date)
    }

    fn create_directory(path: PathBuf) -> Result<()> {
        if !path.exists() {
            let _ = tokio::spawn(async move 
                        { fs::create_dir_all(path).await }
                    ); // Create the directory asynchronously
        }
        Ok(())
    }

    fn start_csv_writer(&self) -> tokio::task::JoinHandle<()> {
        let copied_files = Arc::clone(&self.copied_files);
        let mut csv_path = self.config.csv_output_path.clone();
        let csv_polling_time = self.config.csv_polling_time;
        csv_path = FileMonitor::get_today_csv_path(csv_path, &Utc::now().format("%Y-%m-%d").to_string());
        let csv_file_path = Path::new(&csv_path);
        if let Some(parent_dir) = &csv_file_path.parent() {
            if !parent_dir.exists() {
                println!("Dir not exists: {:?}",parent_dir);
                // let _ = fs::create_dir_all(parent_dir); // Create the directory asynchronously
                let _ = FileMonitor::create_directory(parent_dir.to_path_buf());
            }
        } 
            
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(csv_polling_time));
            let mut last_written = 0;
            
            loop {
                interval.tick().await;
                debug!("Copied data pushed to CSV:{}",last_written);
                let records = {
                    let mut copied = copied_files.write().await;
                    // if copied.len() > last_written {
                    //     let new_records = copied[last_written..].to_vec();
                    //     last_written = copied.len();
                    //     new_records
                    // } else {
                    //     continue;
                    // }
                    let new_records = copied[0..].to_vec();
                    last_written = copied.len();
                    copied.clear();
                    new_records
                };
                
                if let Err(e) = Self::write_csv_records(&csv_path, &records, last_written == 0).await {
                    error!("Failed to write CSV records: {}", e);
                }
            }
        })
    }

    async fn write_csv_records(
        csv_path: &str,
        records: &[CopiedFileRecord],
        write_header: bool,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        
        let file_exists = Path::new(csv_path).exists();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(csv_path)
            .await?;
        
        let mut output = Vec::new();
        {
            let mut writer = Writer::from_writer(&mut output);
            
            if write_header && !file_exists {
                writer.write_record(&[
                    "timestamp",
                    "source_path",
                    "destination_path",
                    "file_size",
                    "checksum",
                    "destination_checksum",
                    "status",
                    "error_message",
                    "end_timestamp",
                    "is_lst_referenced",
                    "lst_source_file",
                ])?;
            }
            
            for record in records {
                writer.write_record(&[
                    &record.timestamp,
                    &record.source_path,
                    &record.destination_path,
                    &record.file_size.to_string(),
                    &record.check_sum,
                    &record.des_check_sum.as_ref().unwrap_or(&String::new()).to_string(),
                    &record.status,
                    &record.error_message.as_deref().unwrap_or("").to_string(),
                    &record.end_timestamp.as_deref().unwrap_or("").to_string(),
                    &record.is_lst_referenced.map(|b| b.to_string()).unwrap_or_default(),
                    &record.lst_source_file.as_deref().unwrap_or("").to_string(),
                ])?;
            }
            
            writer.flush()?;
        }
        
        file.write_all(&output).await?;
        file.flush().await?;
        
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        info!("🛑 Shutting down file monitor system");
        
        // // Write any remaining records to CSV
        // let records = {
        //     let copied = self.copied_files.read().await;
        //     copied.clone()
        // };
        
        // if !records.is_empty() {
        //     Self::write_csv_records(&self.config.csv_output_path, &records, true).await?;
        // }
        
        // info!("Shutdown complete");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    
    // Load configuration
    let config_content = fs::read_to_string(&args.config).await
        .context("Failed to read configuration file")?;
    let config: Config = serde_json::from_str(&config_content)
        .context("Failed to parse configuration file")?;
    
    // println!("Loaded configuration: {:?}", config);
    // Create and start the file monitor
    let monitor = FileMonitor::new(config)?;
    monitor.start().await?;
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    // #[test]
    // fn test_pattern_matching() {
    //     let path = Path::new("/test/file.txt");
    //     // assert!(FileMonitor::matches_patterns(path, &["*.txt".to_string()]));
    //     // assert!(FileMonitor::matches_patterns(path, &["file.txt".to_string()]));
    //     // assert!(!FileMonitor::matches_patterns(path, &["*.pdf".to_string()]));
    // }
    
    #[tokio::test]
    async fn test_file_completeness() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        
        fs::write(&file_path, "test content").await.unwrap();
        assert!(FileMonitor::is_file_complete(&file_path).await.unwrap());
    }

    #[tokio::test]
    async fn test_lst_file_processing() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create a test .lst file
        let lst_content = "file1.txt\nfile2.txt\n# This is a comment\nsubdir/file3.txt";
        let lst_file_path = temp_dir.path().join("test.lst");
        fs::write(&lst_file_path, lst_content).await.unwrap();
        
        // Create referenced files
        fs::write(temp_dir.path().join("file1.txt"), "content1").await.unwrap();
        fs::write(temp_dir.path().join("file2.txt"), "content2").await.unwrap();
        
        // Create subdirectory and file
        fs::create_dir_all(temp_dir.path().join("subdir")).await.unwrap();
        fs::write(temp_dir.path().join("subdir/file3.txt"), "content3").await.unwrap();
        
        // Test the .lst file processing logic here
        // This is a basic test structure - you'd need to set up the full config and test context
        assert!(lst_file_path.exists());
    }

    #[test]
    fn test_pattern_compilation() {
        let watch_folder = WatchFolder {
            source_path: "/test".to_string(),
            destination_path: "/dest".to_string(),
            file_patterns: vec!["*.TxT".to_string()], // Mixed case pattern
            recursive: false,
        };
        
        let compiled_patterns = watch_folder.compile_patterns().unwrap();
        
        // Should have compiled multiple versions: original, lowercase, uppercase
        assert!(compiled_patterns.len() >= 1);
        
        // Verify it can match different cases
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/file.txt"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/file.TXT"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/file.TxT"), &compiled_patterns));
    }
    #[test]
    fn test_case_insensitive_pattern_matching() {
        // Test case-insensitive pattern matching
        let watch_folder = WatchFolder {
            source_path: "/test".to_string(),
            destination_path: "/dest".to_string(),
            file_patterns: vec!["*.txt".to_string(), "*.PDF".to_string(), "Data*.csv".to_string()],
            recursive: false,
        };
        
        let compiled_patterns = watch_folder.compile_patterns().unwrap();
        
        // Test various case combinations
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/file.txt"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/file.TXT"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/FILE.txt"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/document.pdf"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/document.PDF"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/Data123.csv"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/data456.csv"), &compiled_patterns));
        assert!(FileMonitor::matches_glob_patterns(Path::new("/test/DATA789.CSV"), &compiled_patterns));
        
        // Test non-matching patterns
        assert!(!FileMonitor::matches_glob_patterns(Path::new("/test/file.doc"), &compiled_patterns));
        assert!(!FileMonitor::matches_glob_patterns(Path::new("/test/info.csv"), &compiled_patterns)); // Doesn't start with "Data"
    }

}