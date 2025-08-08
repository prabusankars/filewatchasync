use serde::{Deserialize, Serialize};
use glob::Pattern;
use anyhow::{Result, Context};

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