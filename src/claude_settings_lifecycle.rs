use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::info;

use crate::claude_settings;

#[derive(Clone, Debug)]
struct ClaudeSettingsPaths {
    settings: PathBuf,
    backup: PathBuf,
    lock: PathBuf,
}

impl ClaudeSettingsPaths {
    fn from_home(home: &Path) -> Self {
        let adapter_dir = home.join(".claude-adapter");
        Self {
            settings: home.join(".claude").join("settings.json"),
            backup: adapter_dir.join("base_url_backup.json"),
            lock: adapter_dir.join("settings.lock"),
        }
    }
}

trait JsonFileWriter {
    fn write_json(&self, path: &Path, value: &Value) -> anyhow::Result<()>;
}

#[derive(Default)]
struct AtomicJsonFileWriter;

impl JsonFileWriter for AtomicJsonFileWriter {
    fn write_json(&self, path: &Path, value: &Value) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let destination = match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(path)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "Failed to resolve JSON file symlink '{}': {}",
                        path.display(),
                        error
                    )
                })?,
            Ok(_) => path.to_path_buf(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
            Err(error) => return Err(error.into()),
        };
        let content = serde_json::to_vec_pretty(value)?;
        let mut file = atomic_write_file::AtomicWriteFile::open(destination)?;
        file.write_all(&content)?;
        file.commit()?;
        Ok(())
    }
}

pub struct ClaudeSettingsManager {
    paths: ClaudeSettingsPaths,
    lock_file: std::fs::File,
}

fn read_settings(paths: &ClaudeSettingsPaths) -> anyhow::Result<Value> {
    if !paths.settings.exists() {
        return Ok(serde_json::json!({}));
    }
    let content = std::fs::read_to_string(&paths.settings).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read Claude settings '{}': {}",
            paths.settings.display(),
            e
        )
    })?;
    serde_json::from_str(&content).map_err(|e| {
        anyhow::anyhow!(
            "Claude settings '{}' is invalid JSON: {}",
            paths.settings.display(),
            e
        )
    })
}

fn read_backup(paths: &ClaudeSettingsPaths) -> anyhow::Result<Option<Value>> {
    if !paths.backup.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&paths.backup).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read Claude settings backup '{}': {}",
            paths.backup.display(),
            e
        )
    })?;
    serde_json::from_str(&content).map(Some).map_err(|e| {
        anyhow::anyhow!(
            "Claude settings backup '{}' is invalid JSON: {}",
            paths.backup.display(),
            e
        )
    })
}

fn inject_claude_settings_at(
    paths: &ClaudeSettingsPaths,
    proxy_url: &str,
    stream_idle_timeout_ms: u64,
    writer: &dyn JsonFileWriter,
) -> anyhow::Result<()> {
    let mut settings = read_settings(paths)?;
    let backup =
        claude_settings::apply_managed_env(&mut settings, proxy_url, stream_idle_timeout_ms)
            .map_err(anyhow::Error::msg)?;

    writer.write_json(&paths.backup, &backup)?;
    writer.write_json(&paths.settings, &settings)?;

    info!(
        path = %paths.settings.display(),
        url = %proxy_url,
        stream_idle_ms = stream_idle_timeout_ms,
        "已注入 Claude Code env 至 settings.json / Injected Claude Code env into settings.json"
    );
    Ok(())
}

fn restore_claude_settings_at(
    paths: &ClaudeSettingsPaths,
    writer: &dyn JsonFileWriter,
) -> anyhow::Result<()> {
    let Some(backup) = read_backup(paths)? else {
        return Ok(());
    };
    let mut settings = read_settings(paths)?;
    claude_settings::restore_managed_env(&mut settings, Some(&backup))
        .map_err(anyhow::Error::msg)?;

    writer.write_json(&paths.settings, &settings)?;
    std::fs::remove_file(&paths.backup).map_err(|e| {
        anyhow::anyhow!(
            "Failed to remove Claude settings backup '{}': {}",
            paths.backup.display(),
            e
        )
    })?;

    info!(
        path = %paths.settings.display(),
        "已還原 Claude Code env / Restored Claude Code env"
    );
    Ok(())
}

fn start_with_writer(
    paths: ClaudeSettingsPaths,
    proxy_url: &str,
    stream_idle_timeout_ms: u64,
    writer: &dyn JsonFileWriter,
) -> anyhow::Result<Option<ClaudeSettingsManager>> {
    if let Some(parent) = paths.lock.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.lock)?;
    match fs2::FileExt::try_lock_exclusive(&lock_file) {
        Ok(()) => {}
        Err(e)
            if e.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                || e.kind() == fs2::lock_contended_error().kind() =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    }

    let manager = ClaudeSettingsManager { paths, lock_file };
    if manager.paths.backup.exists() {
        restore_claude_settings_at(&manager.paths, writer)?;
    }
    inject_claude_settings_at(&manager.paths, proxy_url, stream_idle_timeout_ms, writer)?;
    Ok(Some(manager))
}

pub fn start(
    proxy_url: &str,
    stream_idle_timeout_ms: u64,
) -> anyhow::Result<Option<ClaudeSettingsManager>> {
    let Some(home) = dirs::home_dir() else {
        return Ok(None);
    };
    start_with_writer(
        ClaudeSettingsPaths::from_home(&home),
        proxy_url,
        stream_idle_timeout_ms,
        &AtomicJsonFileWriter,
    )
}

impl ClaudeSettingsManager {
    fn restore_with_writer(&self, writer: &dyn JsonFileWriter) -> anyhow::Result<()> {
        restore_claude_settings_at(&self.paths, writer)
    }

    pub fn restore(&self) -> anyhow::Result<()> {
        self.restore_with_writer(&AtomicJsonFileWriter)
    }
}

impl Drop for ClaudeSettingsManager {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.lock_file);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind, Write};
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;

    fn test_paths(temp: &TempDir) -> ClaudeSettingsPaths {
        ClaudeSettingsPaths::from_home(temp.path())
    }

    fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    struct FailOnWrite {
        fail_on: usize,
        calls: std::cell::Cell<usize>,
        delegate: AtomicJsonFileWriter,
    }

    impl FailOnWrite {
        fn new(fail_on: usize) -> Self {
            Self {
                fail_on,
                calls: std::cell::Cell::new(0),
                delegate: AtomicJsonFileWriter,
            }
        }
    }

    impl JsonFileWriter for FailOnWrite {
        fn write_json(&self, path: &Path, value: &Value) -> anyhow::Result<()> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if call == self.fail_on {
                return Err(
                    Error::new(ErrorKind::PermissionDenied, "injected replace failure").into(),
                );
            }
            self.delegate.write_json(path, value)
        }
    }

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogWriter(self.0.clone())
        }
    }

    #[test]
    fn lifecycle_backup_and_logs_never_contain_existing_api_key() {
        const SENTINEL_SECRET: &str = "sentinel-anthropic-secret-do-not-persist";
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let original = json!({"env": {"ANTHROPIC_API_KEY": SENTINEL_SECRET}});
        write_json(&paths.settings, &original);
        let logs = CapturedLogs::default();
        let disk_backup = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let manager = start_with_writer(
                paths.clone(),
                "http://127.0.0.1:8080",
                300_000,
                &AtomicJsonFileWriter,
            )
            .unwrap()
            .unwrap();
            *disk_backup.lock().unwrap() = std::fs::read(&paths.backup).unwrap();
            manager.restore_with_writer(&AtomicJsonFileWriter).unwrap();
        });

        let log_bytes = logs.0.lock().unwrap().clone();
        let captured_backup: Value = serde_json::from_slice(&disk_backup.lock().unwrap()).unwrap();
        assert!(captured_backup.get("anthropic_api_key").is_none());
        assert!(!String::from_utf8_lossy(&disk_backup.lock().unwrap()).contains(SENTINEL_SECRET));
        assert!(!String::from_utf8_lossy(&log_bytes).contains(SENTINEL_SECRET));
        assert_eq!(read_json(&paths.settings), original);
    }

    #[test]
    fn restore_is_replay_safe_after_backup_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let original = json!({
            "env": {
                "ENABLE_TOOL_SEARCH": "true",
                "UNRELATED": {"preserve": "exactly"}
            },
            "permissions": {"allow": ["Read"]}
        });
        write_json(&paths.settings, &original);
        let manager = start_with_writer(
            paths.clone(),
            "http://127.0.0.1:8080",
            300_000,
            &AtomicJsonFileWriter,
        )
        .unwrap()
        .unwrap();

        manager.restore_with_writer(&AtomicJsonFileWriter).unwrap();
        let settings_after_first_restore = std::fs::read(&paths.settings).unwrap();
        assert_eq!(read_json(&paths.settings), original);
        assert!(!paths.backup.exists());

        let fail_on_any_write = FailOnWrite::new(1);
        manager.restore_with_writer(&fail_on_any_write).unwrap();

        assert_eq!(fail_on_any_write.calls.get(), 0);
        assert_eq!(
            std::fs::read(&paths.settings).unwrap(),
            settings_after_first_restore
        );
        assert_eq!(read_json(&paths.settings), original);
        assert!(!paths.backup.exists());

        let competing_manager = start_with_writer(
            paths.clone(),
            "http://127.0.0.1:9090",
            600_000,
            &AtomicJsonFileWriter,
        )
        .unwrap();
        assert!(competing_manager.is_none());
        assert_eq!(read_json(&paths.settings), original);
    }

    #[test]
    fn stale_backup_is_restored_before_fresh_injection() {
        const SENTINEL_SECRET: &str = "legacy-stale-secret";
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let original = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://original.example",
                "ANTHROPIC_API_KEY": SENTINEL_SECRET,
                "ENABLE_TOOL_SEARCH": "auto:5",
                "UNRELATED": "keep"
            }
        });
        write_json(
            &paths.settings,
            &json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:8080",
                    "ANTHROPIC_API_KEY": "cc-adapter-local",
                    "ENABLE_TOOL_SEARCH": "true",
                    "CLAUDE_STREAM_IDLE_TIMEOUT_MS": "300000",
                    "UNRELATED": "keep"
                }
            }),
        );
        write_json(
            &paths.backup,
            &json!({
                "env_was_present": true,
                "anthropic_base_url": {"had_value": true, "old_value": "https://original.example"},
                "anthropic_api_key": {"had_value": true, "old_value": SENTINEL_SECRET},
                "enable_tool_search": {"had_value": true, "old_value": "auto:5"},
                "claude_stream_idle_timeout_ms": {"had_value": false, "old_value": null}
            }),
        );

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish();
        let manager = tracing::subscriber::with_default(subscriber, || {
            start_with_writer(
                paths.clone(),
                "http://127.0.0.1:9090",
                600_000,
                &AtomicJsonFileWriter,
            )
            .unwrap()
            .unwrap()
        });
        assert_eq!(
            read_json(&paths.settings)["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:9090"
        );
        assert_eq!(
            read_json(&paths.settings)["env"]["ANTHROPIC_API_KEY"],
            SENTINEL_SECRET
        );
        assert!(read_json(&paths.backup).get("anthropic_api_key").is_none());
        assert!(
            !std::fs::read_to_string(&paths.backup)
                .unwrap()
                .contains(SENTINEL_SECRET)
        );
        assert!(!String::from_utf8_lossy(&logs.0.lock().unwrap()).contains(SENTINEL_SECRET));

        manager.restore_with_writer(&AtomicJsonFileWriter).unwrap();

        assert_eq!(read_json(&paths.settings), original);
        assert!(!paths.backup.exists());
    }

    #[test]
    fn second_owner_cannot_modify_or_restore_first_owners_settings() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let original = json!({"env": {"UNRELATED": "keep"}});
        write_json(&paths.settings, &original);
        let first = start_with_writer(
            paths.clone(),
            "http://127.0.0.1:8080",
            300_000,
            &AtomicJsonFileWriter,
        )
        .unwrap()
        .unwrap();
        let settings_after_first = std::fs::read(&paths.settings).unwrap();
        let backup_after_first = std::fs::read(&paths.backup).unwrap();

        let second = start_with_writer(
            paths.clone(),
            "http://127.0.0.1:9090",
            600_000,
            &AtomicJsonFileWriter,
        )
        .unwrap();

        assert!(second.is_none());
        assert_eq!(
            std::fs::read(&paths.settings).unwrap(),
            settings_after_first
        );
        assert_eq!(std::fs::read(&paths.backup).unwrap(), backup_after_first);
        first.restore_with_writer(&AtomicJsonFileWriter).unwrap();
        assert_eq!(read_json(&paths.settings), original);
        assert!(!paths.backup.exists());
    }

    #[test]
    fn replacement_failure_leaves_existing_destination_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let original = json!({"env": {"UNRELATED": "keep"}});
        write_json(&paths.settings, &original);

        let result = inject_claude_settings_at(
            &paths,
            "http://127.0.0.1:8080",
            300_000,
            &FailOnWrite::new(1),
        );

        assert!(result.is_err());
        assert_eq!(read_json(&paths.settings), original);
        assert!(!paths.backup.exists());
    }

    #[test]
    fn settings_write_failure_keeps_original_settings_and_recoverable_backup() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let original = json!({"env": {"UNRELATED": "keep"}});
        write_json(&paths.settings, &original);

        let result = inject_claude_settings_at(
            &paths,
            "http://127.0.0.1:8080",
            300_000,
            &FailOnWrite::new(2),
        );

        assert!(result.is_err());
        assert_eq!(read_json(&paths.settings), original);
        assert!(paths.backup.exists());
        restore_claude_settings_at(&paths, &AtomicJsonFileWriter).unwrap();
        assert_eq!(read_json(&paths.settings), original);
        assert!(!paths.backup.exists());
    }

    #[test]
    fn restore_write_failure_keeps_settings_and_backup_intact() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let original = json!({"env": {"UNRELATED": "keep"}});
        write_json(&paths.settings, &original);
        inject_claude_settings_at(
            &paths,
            "http://127.0.0.1:8080",
            300_000,
            &AtomicJsonFileWriter,
        )
        .unwrap();
        let injected_settings = std::fs::read(&paths.settings).unwrap();
        let backup = std::fs::read(&paths.backup).unwrap();

        let result = restore_claude_settings_at(&paths, &FailOnWrite::new(1));

        assert!(result.is_err());
        assert_eq!(std::fs::read(&paths.settings).unwrap(), injected_settings);
        assert_eq!(std::fs::read(&paths.backup).unwrap(), backup);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writer_preserves_settings_symlink_and_updates_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(&temp);
        let target = temp.path().join("shared-settings.json");
        write_json(&target, &json!({"env": {"UNRELATED": "original"}}));
        std::fs::create_dir_all(paths.settings.parent().unwrap()).unwrap();
        symlink(&target, &paths.settings).unwrap();

        AtomicJsonFileWriter
            .write_json(&paths.settings, &json!({"env": {"UNRELATED": "updated"}}))
            .unwrap();

        assert!(
            std::fs::symlink_metadata(&paths.settings)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read_json(&target), json!({"env": {"UNRELATED": "updated"}}));
    }
}
