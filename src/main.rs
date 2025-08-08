use tokio::fs;
use anyhow::{Result, Context};
use clap::Parser;
mod modimpl;
use modimpl::copyfilerecord::CopiedFileRecord;
use modimpl::config::Config;
use modimpl::watchfolder::WatchFolder;
use modimpl::fileinfo::FileInfo;
use modimpl::healthstatus::HealthStatus;
use modimpl::filemonitor::FileMonitor;

#[derive(Parser, Debug)]
#[command(name = "file-monitor")]
#[command(about = "A file monitoring and copying system")]
pub struct Args {
    #[arg(short, long, default_value = "config.json")]
    pub config: String,
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
    use std::path::Path;
    use super::*;
    use tempfile::TempDir;
    
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