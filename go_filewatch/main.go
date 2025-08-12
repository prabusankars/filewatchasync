package main

import (
	"bufio"
	"context"
	"crypto/sha256"
	"encoding/csv"
	"encoding/json"
	"flag"
	"fmt"
	"hash"
	"io"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/fsnotify/fsnotify"
)

// Config represents the application configuration
type Config struct {
	WatchFolders                []WatchFolder `json:"watch_folders"`
	MaxConcurrentCopies         int           `json:"max_concurrent_copies"`
	HealthCheckIntervalMinutes  int           `json:"health_check_interval_minutes"`
	FileStabilityWaitSeconds    int           `json:"file_stability_wait_seconds"`
	CSVOutputPath               string        `json:"csv_output_path"`
	CSVPollingTime              int           `json:"csv_polling_time"`
	LogFilePath                 string        `json:"log_file_path"`
	LstFileStabilityWaitSeconds *int          `json:"lst_file_stability_wait_seconds,omitempty"`
}

// WatchFolder represents a folder to watch
type WatchFolder struct {
	SourcePath      string   `json:"source_path"`
	DestinationPath string   `json:"destination_path"`
	FilePatterns    []string `json:"file_patterns"`
	Recursive       bool     `json:"recursive"`
}

// FileInfo represents tracked file information
type FileInfo struct {
	Path         string
	ModifiedTime time.Time
	CheckSum     string
}

// CopiedFileRecord represents a record of file copy operation
type CopiedFileRecord struct {
	Timestamp       string  `json:"timestamp"`
	SourcePath      string  `json:"source_path"`
	DestinationPath string  `json:"destination_path"`
	FileSize        int64   `json:"file_size"`
	CheckSum        string  `json:"check_sum"`
	DestCheckSum    *string `json:"des_check_sum"`
	Status          string  `json:"status"`
	ErrorMessage    *string `json:"error_message"`
	EndTimestamp    *string `json:"end_timestamp"`
	IsLstReferenced *bool   `json:"is_lst_referenced"`
	LstSourceFile   *string `json:"lst_source_file"`
}

// HealthStatus represents the health status of the system
type HealthStatus struct {
	LastCheck                time.Time `json:"last_check"`
	FilesProcessedLastPeriod int64     `json:"files_processed_last_period"`
	ErrorsLastPeriod         int64     `json:"errors_last_period"`
	IsHealthy                bool      `json:"is_healthy"`
	mu                       sync.RWMutex
}

// FileMonitor is the main file monitoring system
type FileMonitor struct {
	config           *Config
	copiedFiles      []*CopiedFileRecord
	copiedFilesMu    sync.RWMutex
	pendingFiles     map[string]time.Time
	pendingFilesMu   sync.RWMutex
	pendingLstFiles  map[string]time.Time
	pendingLstMu     sync.RWMutex
	processedFiles   map[FileInfo]bool
	processedFilesMu sync.RWMutex
	semaphore        chan struct{}
	healthStatus     *HealthStatus
	ctx              context.Context
	cancel           context.CancelFunc
	wg               sync.WaitGroup
	watchers         []*fsnotify.Watcher
}

// NewFileMonitor creates a new FileMonitor instance
func NewFileMonitor(config *Config) *FileMonitor {
	ctx, cancel := context.WithCancel(context.Background())

	return &FileMonitor{
		config:          config,
		copiedFiles:     make([]*CopiedFileRecord, 0),
		pendingFiles:    make(map[string]time.Time),
		pendingLstFiles: make(map[string]time.Time),
		processedFiles:  make(map[FileInfo]bool),
		semaphore:       make(chan struct{}, config.MaxConcurrentCopies),
		healthStatus: &HealthStatus{
			LastCheck: time.Now(),
			IsHealthy: true,
		},
		ctx:      ctx,
		cancel:   cancel,
		watchers: make([]*fsnotify.Watcher, 0),
	}
}

// Start starts the file monitoring system
func (fm *FileMonitor) Start() error {
	log.Println("🚀 Starting file monitor system")
	log.Printf("Configuration: %+v", fm.config)

	// Start health check task
	fm.wg.Add(1)
	go fm.healthCheckTask()

	// Start file watchers
	for _, watchFolder := range fm.config.WatchFolders {
		if err := fm.startFolderWatcher(watchFolder); err != nil {
			return fmt.Errorf("failed to start folder watcher: %w", err)
		}
	}

	// Start file processors
	fm.wg.Add(1)
	go fm.fileProcessorTask()

	fm.wg.Add(1)
	go fm.lstFileProcessorTask()

	// Start CSV writer
	fm.wg.Add(1)
	go fm.csvWriterTask()

	log.Println("✅ All tasks started successfully")
	return nil
}

// Stop stops the file monitoring system
func (fm *FileMonitor) Stop() {
	log.Println("⚠️ Stopping file monitor system...")

	// Stop all watchers
	for _, watcher := range fm.watchers {
		watcher.Close()
	}

	// Cancel context and wait for goroutines
	fm.cancel()
	fm.wg.Wait()

	log.Println("🛑 File monitor system stopped")
}

// startFolderWatcher starts watching a specific folder
func (fm *FileMonitor) startFolderWatcher(watchFolder WatchFolder) error {
	watcher, err := fsnotify.NewWatcher()
	if err != nil {
		return fmt.Errorf("failed to create watcher: %w", err)
	}

	fm.watchers = append(fm.watchers, watcher)

	// Add the watch path
	if err := watcher.Add(watchFolder.SourcePath); err != nil {
		return fmt.Errorf("failed to add watch path: %w", err)
	}

	// Add subdirectories if recursive
	if watchFolder.Recursive {
		if err := fm.addRecursiveWatches(watcher, watchFolder.SourcePath); err != nil {
			log.Printf("Warning: failed to add recursive watches: %v", err)
		}
	}

	log.Printf("👀 Started watching folder: %s, patterns: %v (recursive: %v)",
		watchFolder.SourcePath, watchFolder.FilePatterns, watchFolder.Recursive)

	// Start event handling goroutine
	fm.wg.Add(1)
	go fm.handleWatchEvents(watcher, watchFolder)

	return nil
}

// addRecursiveWatches adds watches for all subdirectories
func (fm *FileMonitor) addRecursiveWatches(watcher *fsnotify.Watcher, root string) error {
	return filepath.Walk(root, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if info.IsDir() && path != root {
			return watcher.Add(path)
		}
		return nil
	})
}

// handleWatchEvents handles file system events for a watcher
func (fm *FileMonitor) handleWatchEvents(watcher *fsnotify.Watcher, watchFolder WatchFolder) {
	defer fm.wg.Done()

	recentEvents := make(map[string]time.Time)
	eventDedupWindow := 500 * time.Millisecond

	for {
		select {
		case <-fm.ctx.Done():
			return
		case event, ok := <-watcher.Events:
			if !ok {
				return
			}

			// Clean up old events from dedup tracking
			now := time.Now()
			for path, timestamp := range recentEvents {
				if now.Sub(timestamp) > eventDedupWindow {
					delete(recentEvents, path)
				}
			}

			// Skip if recent duplicate
			if lastTime, exists := recentEvents[event.Name]; exists {
				if now.Sub(lastTime) <= eventDedupWindow {
					continue
				}
			}
			recentEvents[event.Name] = now

			// Process file events
			if event.Op&fsnotify.Write == fsnotify.Write || event.Op&fsnotify.Create == fsnotify.Create {
				fm.handleFileEvent(event.Name, watchFolder)
			} else if event.Op&fsnotify.Remove == fsnotify.Remove {
				fm.handleFileRemoval(event.Name)
			}

		case err, ok := <-watcher.Errors:
			if !ok {
				return
			}
			log.Printf("Watcher error: %v", err)
		}
	}
}

// handleFileEvent processes a file event
func (fm *FileMonitor) handleFileEvent(filePath string, watchFolder WatchFolder) {
	// Check if it's a file
	info, err := os.Stat(filePath)
	if err != nil || info.IsDir() {
		return
	}

	// Check if file matches patterns
	if !fm.matchesPatterns(filePath, watchFolder.FilePatterns) {
		return
	}

	log.Printf("✨ File matches pattern: %s", filePath)

	// Check if already processed
	if fm.isFileAlreadyProcessed(filePath) {
		log.Printf("✨ File already processed, skipping: %s", filePath)
		return
	}

	// Route to appropriate queue based on file extension
	if strings.ToLower(filepath.Ext(filePath)) == ".lst" {
		log.Printf("📋 Adding .lst file to pending queue: %s", filePath)
		fm.pendingLstMu.Lock()
		fm.pendingLstFiles[filePath] = time.Now()
		fm.pendingLstMu.Unlock()
	} else {
		log.Printf("✚ Adding file to pending queue: %s", filePath)
		fm.pendingFilesMu.Lock()
		fm.pendingFiles[filePath] = time.Now()
		fm.pendingFilesMu.Unlock()
	}
}

// handleFileRemoval handles file removal events
func (fm *FileMonitor) handleFileRemoval(filePath string) {
	// Remove from pending queues
	fm.pendingFilesMu.Lock()
	delete(fm.pendingFiles, filePath)
	fm.pendingFilesMu.Unlock()

	fm.pendingLstMu.Lock()
	delete(fm.pendingLstFiles, filePath)
	fm.pendingLstMu.Unlock()

	// Remove from processed files
	fm.processedFilesMu.Lock()
	for fileInfo := range fm.processedFiles {
		if fileInfo.Path == filePath {
			delete(fm.processedFiles, fileInfo)
		}
	}
	fm.processedFilesMu.Unlock()
}

// matchesPatterns checks if a file matches any of the given patterns (case-insensitive)
func (fm *FileMonitor) matchesPatterns(filePath string, patterns []string) bool {
	fileName := strings.ToLower(filepath.Base(filePath))

	for _, pattern := range patterns {
		// Check both original and lowercase pattern
		patterns := []string{pattern, strings.ToLower(pattern), strings.ToUpper(pattern)}

		for _, p := range patterns {
			if matched, _ := filepath.Match(p, fileName); matched {
				log.Printf("✅ Pattern '%s' matches file '%s' (case-insensitive)", pattern, filepath.Base(filePath))
				return true
			}
			if matched, _ := filepath.Match(p, filepath.Base(filePath)); matched {
				log.Printf("✅ Pattern '%s' matches file '%s'", pattern, filepath.Base(filePath))
				return true
			}
		}
	}

	return false
}

// isFileAlreadyProcessed checks if a file has already been processed
func (fm *FileMonitor) isFileAlreadyProcessed(filePath string) bool {
	checksum := fm.calculateChecksum(filePath)
	if checksum == "" {
		return false
	}

	info, err := os.Stat(filePath)
	if err != nil {
		return false
	}

	fileInfo := FileInfo{
		Path:         filePath,
		ModifiedTime: info.ModTime(),
		CheckSum:     checksum,
	}

	fm.processedFilesMu.RLock()
	_, exists := fm.processedFiles[fileInfo]
	fm.processedFilesMu.RUnlock()

	return exists
}

// fileProcessorTask processes regular files
func (fm *FileMonitor) fileProcessorTask() {
	defer fm.wg.Done()

	ticker := time.NewTicker(1 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-fm.ctx.Done():
			return
		case <-ticker.C:
			fm.processStableFiles()
		}
	}
}

// lstFileProcessorTask processes .lst files
func (fm *FileMonitor) lstFileProcessorTask() {
	defer fm.wg.Done()

	ticker := time.NewTicker(1 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-fm.ctx.Done():
			return
		case <-ticker.C:
			fm.processStableLstFiles()
		}
	}
}

// processStableFiles processes stable regular files
func (fm *FileMonitor) processStableFiles() {
	stableTime := time.Duration(fm.config.FileStabilityWaitSeconds) * time.Second
	now := time.Now()

	var filesToProcess []string

	fm.pendingFilesMu.Lock()
	for filePath, timestamp := range fm.pendingFiles {
		if now.Sub(timestamp) >= stableTime {
			filesToProcess = append(filesToProcess, filePath)
			delete(fm.pendingFiles, filePath)
		}
	}
	fm.pendingFilesMu.Unlock()

	// Process files concurrently
	for _, filePath := range filesToProcess {
		go fm.processFile(filePath, nil)
	}
}

// processStableLstFiles processes stable .lst files
func (fm *FileMonitor) processStableLstFiles() {
	stableTime := time.Duration(fm.getLstStabilityWaitSeconds()) * time.Second
	now := time.Now()

	var lstFilesToProcess []string

	fm.pendingLstMu.Lock()
	for filePath, timestamp := range fm.pendingLstFiles {
		if now.Sub(timestamp) >= stableTime {
			lstFilesToProcess = append(lstFilesToProcess, filePath)
			delete(fm.pendingLstFiles, filePath)
		}
	}
	fm.pendingLstMu.Unlock()

	// Process .lst files concurrently
	for _, lstFilePath := range lstFilesToProcess {
		go fm.processLstFile(lstFilePath)
	}
}

// getLstStabilityWaitSeconds returns the stability wait time for .lst files
func (fm *FileMonitor) getLstStabilityWaitSeconds() int {
	if fm.config.LstFileStabilityWaitSeconds != nil {
		return *fm.config.LstFileStabilityWaitSeconds
	}
	return fm.config.FileStabilityWaitSeconds
}

// processFile processes a single file
func (fm *FileMonitor) processFile(sourcePath string, lstSourceFile *string) {
	// Acquire semaphore
	select {
	case fm.semaphore <- struct{}{}:
		defer func() { <-fm.semaphore }()
	case <-fm.ctx.Done():
		return
	}

	log.Printf("Processing file: %s", sourcePath)

	// Find matching watch folder
	watchFolder := fm.findMatchingWatchFolder(sourcePath)
	if watchFolder == nil {
		log.Printf("No matching watch folder found for: %s", sourcePath)
		return
	}

	// Validate file is complete
	if !fm.isFileComplete(sourcePath) {
		log.Printf("File is not complete yet: %s", sourcePath)
		return
	}
	log.Printf("file source: %s", sourcePath)
	// Calculate destination path
	relPath, err := filepath.Rel(watchFolder.SourcePath, sourcePath)
	if err != nil {
		log.Printf("Failed to calculate relative path: %v", err)
		return
	}
	destPath := filepath.Join(watchFolder.DestinationPath, relPath)
	log.Printf("file dest calculated: %s , %s", relPath, destPath)

	// Ensure destination directory exists
	if err := os.MkdirAll(filepath.Dir(destPath), 0755); err != nil {
		log.Printf("Failed to create destination directory: %v", err)
		return
	}

	// Get file info
	info, err := os.Stat(sourcePath)
	if err != nil {
		log.Printf("Failed to get file info: %v", err)
		return
	}

	startTime := time.Now()

	record := &CopiedFileRecord{
		Timestamp:       startTime.Format(time.RFC3339),
		SourcePath:      sourcePath,
		DestinationPath: destPath,
		FileSize:        info.Size(),
		CheckSum:        fm.calculateChecksum(sourcePath),
		Status:          "copying",
		IsLstReferenced: &[]bool{lstSourceFile != nil}[0],
		LstSourceFile:   lstSourceFile,
	}

	// Copy the file
	if err := fm.copyFile(sourcePath, destPath); err != nil {
		record.Status = "error"
		record.ErrorMessage = &[]string{err.Error()}[0]
		log.Printf("🗷 Failed to copy file: %s -> %s, error: %v", sourcePath, destPath, err)

		fm.healthStatus.mu.Lock()
		fm.healthStatus.ErrorsLastPeriod++
		fm.healthStatus.mu.Unlock()
	} else {
		record.Status = "success"
		destChecksum := fm.calculateChecksum(destPath)
		record.DestCheckSum = &destChecksum
		endTime := time.Now().Format(time.RFC3339)
		record.EndTimestamp = &endTime
		log.Printf("🗹 Successfully copied file: %s -> %s", sourcePath, destPath)

		fm.healthStatus.mu.Lock()
		fm.healthStatus.FilesProcessedLastPeriod++
		fm.healthStatus.mu.Unlock()
	}

	// Add to copied files
	fm.copiedFilesMu.Lock()
	fm.copiedFiles = append(fm.copiedFiles, record)
	fm.copiedFilesMu.Unlock()

	// Mark as processed
	fm.markAsProcessed(sourcePath)
}

// processLstFile processes a .lst file
func (fm *FileMonitor) processLstFile(lstFilePath string) {
	log.Printf("📋 Processing .lst file: %s", lstFilePath)

	// First, copy the .lst file itself
	//fm.processFile(lstFilePath, nil)

	// Read the .lst file content
	file, err := os.Open(lstFilePath)
	if err != nil {
		log.Printf("🗷 Failed to open .lst file: %s, error: %v", lstFilePath, err)
		return
	}
	defer file.Close()

	scanner := bufio.NewScanner(file)
	var referencedFiles []string

	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		log.Printf("line: %s", line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue // Skip empty lines and comments
		}
		referencedFiles = append(referencedFiles, line)
	}

	if err := scanner.Err(); err != nil {
		log.Printf("🗷 Failed to read .lst file: %s, error: %v", lstFilePath, err)
		return
	}

	log.Printf("📋 Found %d file references in .lst file: %s", len(referencedFiles), lstFilePath)

	// Process each referenced file
	for _, filePathStr := range referencedFiles {
		var absoluteFilePath string

		if filepath.IsAbs(filePathStr) {
			absoluteFilePath = filePathStr
		} else {
			// Resolve relative to .lst file's directory
			absoluteFilePath = filepath.Join(filepath.Dir(lstFilePath), filePathStr)
		}

		log.Printf("📄 Checking referenced file: %s", absoluteFilePath)

		// Check if file exists
		if _, err := os.Stat(absoluteFilePath); os.IsNotExist(err) {
			log.Printf("⚠️ Referenced file does not exist: %s", absoluteFilePath)

			// Create error record
			errorRecord := &CopiedFileRecord{
				Timestamp:       time.Now().Format(time.RFC3339),
				SourcePath:      absoluteFilePath,
				DestinationPath: "N/A",
				FileSize:        0,
				CheckSum:        "",
				Status:          "error",
				ErrorMessage:    &[]string{"Referenced file does not exist"}[0],
				EndTimestamp:    &[]string{time.Now().Format(time.RFC3339)}[0],
				IsLstReferenced: &[]bool{true}[0],
				LstSourceFile:   &lstFilePath,
			}

			fm.copiedFilesMu.Lock()
			fm.copiedFiles = append(fm.copiedFiles, errorRecord)
			fm.copiedFilesMu.Unlock()

			fm.healthStatus.mu.Lock()
			fm.healthStatus.ErrorsLastPeriod++
			fm.healthStatus.mu.Unlock()

			continue
		}

		// Check if file is stable
		if !fm.isFileComplete(absoluteFilePath) {
			log.Printf("⚠️ Referenced file is not stable yet, adding to pending: %s", absoluteFilePath)
			fm.pendingFilesMu.Lock()
			fm.pendingFiles[absoluteFilePath] = time.Now()
			fm.pendingFilesMu.Unlock()
			continue
		}

		// Process the referenced file
		go fm.processFile(absoluteFilePath, &lstFilePath)
	}

	log.Printf("📋 Completed processing .lst file: %s", lstFilePath)
}

// findMatchingWatchFolder finds the watch folder that matches the given path
func (fm *FileMonitor) findMatchingWatchFolder(filePath string) *WatchFolder {
	for i, watchFolder := range fm.config.WatchFolders {
		log.Printf("file path: %s, watch folder: %s", filePath, watchFolder.SourcePath)
		dir := filepath.Dir(filePath)
		if strings.HasPrefix(watchFolder.SourcePath, dir) {
			return &fm.config.WatchFolders[i]
		}
	}
	return nil
}

// isFileComplete checks if a file is complete and stable
func (fm *FileMonitor) isFileComplete(filePath string) bool {
	info1, err := os.Stat(filePath)
	if err != nil {
		return false
	}
	size1 := info1.Size()

	time.Sleep(100 * time.Millisecond)

	info2, err := os.Stat(filePath)
	if err != nil {
		return false
	}
	size2 := info2.Size()
	log.Printf("Processing file size1: %d, size2: %d", size1, size2)
	return size1 == size2 && size1 > 0
}

// copyFile copies a file from source to destination
func (fm *FileMonitor) copyFile(src, dst string) error {
	sourceFile, err := os.Open(src)
	if err != nil {
		return err
	}
	defer sourceFile.Close()

	destFile, err := os.Create(dst)
	if err != nil {
		return err
	}
	defer destFile.Close()

	_, err = io.Copy(destFile, sourceFile)
	if err != nil {
		return err
	}

	return destFile.Sync()
}

// calculateChecksum calculates SHA256 checksum of a file
func (fm *FileMonitor) calculateChecksum(filePath string) string {
	file, err := os.Open(filePath)
	if err != nil {
		return ""
	}
	defer file.Close()

	var hasher hash.Hash = sha256.New()
	if _, err := io.Copy(hasher, file); err != nil {
		return ""
	}

	return fmt.Sprintf("%x", hasher.Sum(nil))
}

// markAsProcessed marks a file as processed
func (fm *FileMonitor) markAsProcessed(filePath string) {
	info, err := os.Stat(filePath)
	if err != nil {
		return
	}

	fileInfo := FileInfo{
		Path:         filePath,
		ModifiedTime: info.ModTime(),
		CheckSum:     fm.calculateChecksum(filePath),
	}

	fm.processedFilesMu.Lock()
	fm.processedFiles[fileInfo] = true
	fm.processedFilesMu.Unlock()
}

// healthCheckTask performs periodic health checks
func (fm *FileMonitor) healthCheckTask() {
	defer fm.wg.Done()

	interval := time.Duration(fm.config.HealthCheckIntervalMinutes) * time.Minute
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-fm.ctx.Done():
			return
		case <-ticker.C:
			fm.healthStatus.mu.Lock()
			now := time.Now()
			fm.healthStatus.IsHealthy = fm.healthStatus.ErrorsLastPeriod == 0

			log.Printf("💓 Health check - Processed: %d, Errors: %d, Healthy: %v",
				fm.healthStatus.FilesProcessedLastPeriod,
				fm.healthStatus.ErrorsLastPeriod,
				fm.healthStatus.IsHealthy)

			// Reset counters
			fm.healthStatus.FilesProcessedLastPeriod = 0
			fm.healthStatus.ErrorsLastPeriod = 0
			fm.healthStatus.LastCheck = now
			fm.healthStatus.mu.Unlock()

			// Clear processed files periodically
			fm.processedFilesMu.Lock()
			fm.processedFiles = make(map[FileInfo]bool)
			fm.processedFilesMu.Unlock()
		}
	}
}

// csvWriterTask writes copied file records to CSV
func (fm *FileMonitor) csvWriterTask() {
	defer fm.wg.Done()

	ticker := time.NewTicker(time.Duration(fm.config.CSVPollingTime) * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-fm.ctx.Done():
			// Write remaining records before shutdown
			fm.writeCopiedFilesToCSV()
			return
		case <-ticker.C:
			fm.writeCopiedFilesToCSV()
		}
	}
}

// writeCopiedFilesToCSV writes copied files to CSV
func (fm *FileMonitor) writeCopiedFilesToCSV() {
	fm.copiedFilesMu.Lock()
	if len(fm.copiedFiles) == 0 {
		fm.copiedFilesMu.Unlock()
		return
	}

	records := make([]*CopiedFileRecord, len(fm.copiedFiles))
	copy(records, fm.copiedFiles)
	fm.copiedFiles = fm.copiedFiles[:0] // Clear the slice
	fm.copiedFilesMu.Unlock()

	csvPath := fmt.Sprintf("%s.%s.csv", fm.config.CSVOutputPath, time.Now().Format("2006-01-02"))

	// Ensure CSV directory exists
	if err := os.MkdirAll(filepath.Dir(csvPath), 0755); err != nil {
		log.Printf("Failed to create CSV directory: %v", err)
		return
	}

	// Check if file exists to determine if we need to write headers
	_, err := os.Stat(csvPath)
	writeHeaders := os.IsNotExist(err)

	file, err := os.OpenFile(csvPath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
	if err != nil {
		log.Printf("Failed to open CSV file: %v", err)
		return
	}
	defer file.Close()

	writer := csv.NewWriter(file)
	defer writer.Flush()

	// Write headers if file is new
	if writeHeaders {
		headers := []string{
			"timestamp", "source_path", "destination_path", "file_size",
			"checksum", "destination_checksum", "status", "error_message",
			"end_timestamp", "is_lst_referenced", "lst_source_file",
		}
		if err := writer.Write(headers); err != nil {
			log.Printf("Failed to write CSV headers: %v", err)
			return
		}
	}

	// Write records
	for _, record := range records {
		row := []string{
			record.Timestamp,
			record.SourcePath,
			record.DestinationPath,
			fmt.Sprintf("%d", record.FileSize),
			record.CheckSum,
			fm.stringPtrToString(record.DestCheckSum),
			record.Status,
			fm.stringPtrToString(record.ErrorMessage),
			fm.stringPtrToString(record.EndTimestamp),
			fmt.Sprintf("%v", fm.boolPtrToBool(record.IsLstReferenced)),
			fm.stringPtrToString(record.LstSourceFile),
		}
		if err := writer.Write(row); err != nil {
			log.Printf("Failed to write CSV record: %v", err)
			return
		}
	}
}

// Helper functions
func (fm *FileMonitor) stringPtrToString(ptr *string) string {
	if ptr == nil {
		return ""
	}
	return *ptr
}

func (fm *FileMonitor) boolPtrToBool(ptr *bool) bool {
	if ptr == nil {
		return false
	}
	return *ptr
}

// loadConfig loads configuration from file
func loadConfig(configPath string) (*Config, error) {
	file, err := os.Open(configPath)
	if err != nil {
		return nil, fmt.Errorf("failed to open config file: %w", err)
	}
	defer file.Close()

	var config Config
	decoder := json.NewDecoder(file)
	if err := decoder.Decode(&config); err != nil {
		return nil, fmt.Errorf("failed to parse config file: %w", err)
	}

	return &config, nil
}

func main() {
	var configPath = flag.String("config", "config.json", "Path to configuration file")
	flag.Parse()

	// Load configuration
	config, err := loadConfig(*configPath)
	if err != nil {
		log.Fatalf("Failed to load configuration: %v", err)
	}

	// Create file monitor
	monitor := NewFileMonitor(config)

	// Start the monitor
	if err := monitor.Start(); err != nil {
		log.Fatalf("Failed to start file monitor: %v", err)
	}

	// Wait for interrupt signal
	sigChan := make(chan os.Signal, 1)
	signal.Notify(sigChan, os.Interrupt, syscall.SIGTERM)

	<-sigChan
	log.Println("⚠️ Received interrupt signal, shutting down...")

	// Stop the monitor
	monitor.Stop()

	log.Println("✅ Shutdown complete")
}
