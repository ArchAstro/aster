use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::plan::ServicePlan;
use super::process::LogEvent;

pub const SERVICE_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const TRUNCATION_NOTICE: &str = "[aster] log truncated after reaching the 10 MiB limit\n";
const OVERSIZED_LINE_NOTICE: &str =
    "[aster] log line omitted because it exceeds the 10 MiB limit\n";

pub struct ServiceLogFiles {
    files: HashMap<String, CappedLogFile>,
}

impl ServiceLogFiles {
    pub fn open(workspace_root: &Path, services: &[ServicePlan]) -> Result<Self> {
        Self::open_with_limit(
            workspace_root,
            services.iter().map(|service| service.name.as_str()),
            SERVICE_LOG_MAX_BYTES,
        )
    }

    fn open_with_limit<'a>(
        workspace_root: &Path,
        services: impl IntoIterator<Item = &'a str>,
        max_bytes: u64,
    ) -> Result<Self> {
        let worktree = workspace_root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("default");
        let base = workspace_root
            .join(".aster")
            .join("logs")
            .join(encode_path_component(worktree));
        let mut files = HashMap::new();

        for service in services {
            let directory = base.join(encode_path_component(service));
            fs::create_dir_all(&directory).with_context(|| {
                format!(
                    "failed to create service log directory {}",
                    directory.display()
                )
            })?;
            let path = directory.join("logs.txt");
            files.insert(service.to_string(), CappedLogFile::open(path, max_bytes)?);
        }

        Ok(Self { files })
    }

    pub fn write(&mut self, event: &LogEvent) {
        if let Some(file) = self.files.get_mut(&event.service) {
            let _ = file.write_line(&event.line);
        }
    }
}

struct CappedLogFile {
    path: PathBuf,
    file: File,
    len: u64,
    max_bytes: u64,
}

impl CappedLogFile {
    fn open(path: PathBuf, max_bytes: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open service log file {}", path.display()))?;
        let len = file.metadata()?.len();
        let mut log = Self {
            path,
            file,
            len,
            max_bytes,
        };
        if len > max_bytes {
            log.truncate()?;
        }
        Ok(log)
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let line_bytes = line.len() as u64 + 1;
        if line_bytes > self.max_bytes {
            self.replace_with(OVERSIZED_LINE_NOTICE.as_bytes())?;
            return Ok(());
        }
        if self.len + line_bytes > self.max_bytes {
            let notice = TRUNCATION_NOTICE.as_bytes();
            if notice.len() as u64 + line_bytes <= self.max_bytes {
                self.replace_with(notice)?;
            } else {
                self.replace_with(&[])?;
            }
        }
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.len += line_bytes;
        Ok(())
    }

    fn truncate(&mut self) -> std::io::Result<()> {
        self.replace_with(TRUNCATION_NOTICE.as_bytes())
    }

    fn replace_with(&mut self, message: &[u8]) -> std::io::Result<()> {
        self.file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        let message = if message.len() as u64 <= self.max_bytes {
            message
        } else {
            &[]
        };
        self.file.write_all(message)?;
        self.len = message.len() as u64;
        self.file = OpenOptions::new().append(true).open(&self.path)?;
        Ok(())
    }
}

fn encode_path_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    if encoded.is_empty() {
        "default".to_string()
    } else if matches!(encoded.as_str(), "." | "..") {
        encoded.replace('.', "%2E")
    } else {
        encoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_paths_are_workspace_scoped_and_components_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("first landing-wt7");
        fs::create_dir(&workspace).unwrap();
        let logs =
            ServiceLogFiles::open_with_limit(&workspace, ["../platform/backend"], 1024).unwrap();
        drop(logs);

        assert!(workspace
            .join(".aster/logs/first%20landing-wt7/..%2Fplatform%2Fbackend/logs.txt")
            .is_file());
    }

    #[test]
    fn log_file_never_exceeds_its_cap() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("firstlanding-wt7");
        fs::create_dir(&workspace).unwrap();
        let mut logs =
            ServiceLogFiles::open_with_limit(&workspace, ["platform-backend"], 80).unwrap();
        let event = |line: &str| LogEvent {
            service: "platform-backend".to_string(),
            line: line.to_string(),
            stderr: false,
        };

        for index in 0..20 {
            logs.write(&event(&format!("service output line {index}")));
        }
        logs.write(&event(&"y".repeat(70)));
        let path = workspace.join(".aster/logs/firstlanding-wt7/platform-backend/logs.txt");
        assert!(fs::metadata(&path).unwrap().len() <= 80);
        logs.write(&event(&"x".repeat(100)));

        assert!(fs::metadata(path).unwrap().len() <= 80);
    }
}
