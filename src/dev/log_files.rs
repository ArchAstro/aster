use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use super::plan::ServicePlan;
use super::process::LogEvent;
use crate::config::DevWorkspaceConfig;

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
        let base = workspace_root
            .join(".aster")
            .join("logs")
            .join(encode_path_component(worktree_name(workspace_root)));
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

/// Print a configured service's durable log, paging only for an interactive terminal.
pub fn show_service_logs(
    workspace_root: &Path,
    config: &DevWorkspaceConfig,
    service: &str,
) -> Result<()> {
    if !config.services.contains_key(service) {
        let mut available = config
            .services
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        available.sort_unstable();
        if available.is_empty() {
            bail!("unknown service '{service}'; no services are configured");
        }
        bail!(
            "unknown service '{service}'; configured services: {}",
            available.join(", ")
        );
    }

    let path = service_log_path(workspace_root, service);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "no logs found for service '{service}' at {}; run `aster services up` first",
            path.display()
        ),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open service log {}", path.display()));
        }
    };

    if !io::stdout().is_terminal() {
        return copy_to_stdout(&file);
    }

    show_in_pager(&file)
}

fn service_log_path(workspace_root: &Path, service: &str) -> PathBuf {
    workspace_root
        .join(".aster")
        .join("logs")
        .join(encode_path_component(worktree_name(workspace_root)))
        .join(encode_path_component(service))
        .join("logs.txt")
}

fn worktree_name(workspace_root: &Path) -> &str {
    workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("default")
}

fn show_in_pager(file: &File) -> Result<()> {
    if let Some(pager) = std::env::var_os("PAGER").filter(|value| !value.is_empty()) {
        let pager = pager
            .to_str()
            .ok_or_else(|| anyhow!("PAGER is not valid UTF-8"))?;
        let command = shell_words::split(pager).context("failed to parse PAGER")?;
        if command.is_empty() {
            return copy_to_stdout(file);
        }
        let command = command.iter().map(String::as_str).collect::<Vec<_>>();
        return run_pager(file, &command).with_context(|| format!("failed to run PAGER '{pager}'"));
    }

    #[cfg(windows)]
    let defaults: &[&[&str]] = &[&["more.com"]];
    #[cfg(not(windows))]
    let defaults: &[&[&str]] = &[&["less"], &["more"]];

    for command in defaults {
        match run_pager(file, command) {
            Ok(()) => return Ok(()),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }

    copy_to_stdout(file)
}

fn run_pager(file: &File, command: &[&str]) -> Result<()> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("pager command is empty"))?;
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::from(file.try_clone()?))
        .status()?;
    if !status.success() {
        bail!("pager '{program}' exited with {status}");
    }
    Ok(())
}

fn copy_to_stdout(file: &File) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let mut input = file;
    io::copy(&mut input, &mut stdout).context("failed to write service logs to stdout")?;
    Ok(())
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
