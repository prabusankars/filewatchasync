# FileWatchAsync

**FileWatchAsync** is an advanced asynchronous file monitoring and copying tool written in Rust. It supports configurable pattern matching, robust `.lst` file processing, comprehensive audit trails, and seamless CSV reporting.

---


## ✅ Memory Management

- **Auto-Cleanup:** Cleanup the HashMap stored at every Health Checkup
- **Health Check Monitoring:** Tracks memory usage in regular health checks.

## ❤️ Health Monitoring

- **5-Minute Heartbeats:** Periodic health logs with event and file stats.
- **Alerting:** Triggers alerts for high pending files or missing directories.
- **Notify Watchdog:** Detects if file event monitoring stops working.

## 🛠️ Concurrent Execution

- **Handles Files:** Safely processes files concurrently
- **Async/Await:** Async and Futures used to efficiently handle the concurrency.
- **Tokio:** uses Tokio runtime for concurrent threading.

## 📋 Production Logging

- **Startup Summary:** Configuration logged at startup.
- **Warnings & Cleanup Logs:** Issues and cleanups are clearly reported.
- **Health Logs:** Regular status updates every 5 minutes.

## 🚀 Linux & Windows 
- **OS Support:** Supports both Operating System and same codebase will work

## 🔍 File Verification

- **Full Scan:** Checksum of file at the end of copy to verify

## 📝 Audit Logging

- **Dual Format Logs:** Human-readable `.log` + structured `.csv`.


### 📊 Log Files

- `file_watcher.log`: Human-readable log.
- `file_operations.csv`: Structured audit data.


## 🧯 Robust External Failure Handling

Covers edge cases like:
- Power loss, process kill, crash → Full recovery.
- Network/disk issues → Detected and retried.
- File deletion or corruption → Validated with checksums.

## 🧵 Tracing Features

- **Structured Logging:** Function-level tracing with contextual info.
- **Log Levels:**
  - `ERROR`: Critical failures.
  - `WARN`: Operational risks.
  - `INFO`: Standard operations.
  - `DEBUG`: Deep tracing for diagnostics.

---

## 🎯 Summary

This system is production-grade, resilient, and audit-compliant, with ACID-like guarantees for file operations, detailed logs, health monitoring, and graceful recovery from real-world failure scenarios.
