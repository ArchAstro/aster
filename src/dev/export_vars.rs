//! Private JSONL endpoints used by supervised variable-exporting targets.

use std::collections::HashMap;

pub(crate) const EXPORT_PATH_ENV: &str = "ASTER_EXPORT_VAR_PATH";
const MAX_RECORD_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) enum ExportEvent {
    Snapshot {
        producer: String,
        generation: u64,
        values: HashMap<String, String>,
    },
    Invalid {
        producer: String,
        generation: u64,
        reason: String,
    },
}

#[derive(Default)]
struct JsonlParser {
    buffer: Vec<u8>,
    discarding_oversized: bool,
}

impl JsonlParser {
    fn push(&mut self, mut bytes: &[u8]) -> Vec<Result<HashMap<String, String>, String>> {
        let mut records = Vec::new();
        if self.discarding_oversized {
            let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
                return records;
            };
            bytes = &bytes[newline + 1..];
            self.discarding_oversized = false;
        }
        self.buffer.extend_from_slice(bytes);

        loop {
            let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') else {
                if self.buffer.len() > MAX_RECORD_BYTES {
                    self.buffer.clear();
                    self.discarding_oversized = true;
                    records.push(Err("record exceeds 64 KiB".to_string()));
                }
                break;
            };
            let mut record = self.buffer.drain(..=newline).collect::<Vec<_>>();
            record.pop();
            if record.len() > MAX_RECORD_BYTES {
                records.push(Err("record exceeds 64 KiB".to_string()));
                continue;
            }
            records.push(parse_snapshot(&record));
        }
        records
    }
}

fn parse_snapshot(bytes: &[u8]) -> Result<HashMap<String, String>, String> {
    use serde::de::{Error as _, MapAccess, Visitor};

    struct SnapshotVisitor;
    impl<'de> Visitor<'de> for SnapshotVisitor {
        type Value = HashMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a JSON object with unique environment-name keys and string values")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = HashMap::new();
            while let Some(key) = map.next_key::<String>()? {
                let value = map.next_value::<String>()?;
                if values.insert(key.clone(), value).is_some() {
                    return Err(A::Error::custom(format!("duplicate key '{key}'")));
                }
            }
            Ok(values)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let values = serde::de::Deserializer::deserialize_map(&mut deserializer, SnapshotVisitor)
        .map_err(|error| format!("invalid JSON snapshot: {error}"))?;
    deserializer
        .end()
        .map_err(|error| format!("trailing JSON data: {error}"))?;
    for (name, value) in &values {
        if !valid_environment_name(name) {
            return Err(format!("invalid environment variable name '{name}'"));
        }
        if value.contains('\0') {
            return Err(format!("environment variable '{name}' contains NUL"));
        }
    }
    Ok(values)
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(unix)]
mod platform {
    use super::*;
    use anyhow::{Context, Result};
    use std::ffi::CString;
    use std::fs::{self, File, OpenOptions};
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{
        DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    pub(crate) struct ExportEndpoint {
        directory: PathBuf,
        path: PathBuf,
        _keepalive_writer: File,
        stop: Arc<AtomicBool>,
        reader: Option<JoinHandle<()>>,
    }

    impl ExportEndpoint {
        pub(crate) fn create(
            producer: &str,
            generation: u64,
            events: Sender<ExportEvent>,
        ) -> Result<Self> {
            cleanup_stale_endpoints();
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|error| {
                anyhow::anyhow!("failed to generate variable endpoint name: {error}")
            })?;
            let nonce = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let directory = std::env::temp_dir().join(format!(
                "aster-export-vars-{}-{generation}-{nonce}",
                std::process::id()
            ));
            let mut directory_builder = fs::DirBuilder::new();
            directory_builder.mode(0o700);
            directory_builder.create(&directory).with_context(|| {
                format!("failed to create variable endpoint {}", directory.display())
            })?;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
            let path = directory.join("vars.jsonl");
            make_fifo(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

            let reader = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
                .with_context(|| format!("failed to open variable reader {}", path.display()))?;
            let keepalive_writer = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
                .with_context(|| format!("failed to open variable keepalive {}", path.display()))?;
            let stop = Arc::new(AtomicBool::new(false));
            let reader_stop = stop.clone();
            let event_producer = producer.to_string();
            let handle = std::thread::spawn(move || {
                read_records(reader, &event_producer, generation, &events, &reader_stop);
            });
            Ok(Self {
                directory,
                path,
                _keepalive_writer: keepalive_writer,
                stop,
                reader: Some(handle),
            })
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for ExportEndpoint {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_dir(&self.directory);
        }
    }

    fn make_fifo(path: &Path) -> Result<()> {
        let path = CString::new(path.as_os_str().as_bytes())
            .context("variable endpoint path contains NUL")?;
        let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to create variable FIFO");
        }
        Ok(())
    }

    fn cleanup_stale_endpoints() {
        let temp = std::env::temp_dir();
        let Ok(entries) = fs::read_dir(&temp) else {
            return;
        };
        let current_uid = unsafe { libc::geteuid() };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix("aster-export-vars-") else {
                continue;
            };
            let Some(pid) = rest
                .split('-')
                .next()
                .and_then(|value| value.parse::<i32>().ok())
            else {
                continue;
            };
            if process_exists(pid) {
                continue;
            }
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_dir() || metadata.uid() != current_uid {
                continue;
            }
            let fifo = path.join("vars.jsonl");
            if fs::symlink_metadata(&fifo).is_ok_and(|metadata| {
                metadata.file_type().is_fifo() && metadata.uid() == current_uid
            }) {
                let _ = fs::remove_file(&fifo);
            }
            let _ = fs::remove_dir(&path);
        }
    }

    fn process_exists(pid: i32) -> bool {
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn read_records(
        mut reader: File,
        producer: &str,
        generation: u64,
        events: &Sender<ExportEvent>,
        stop: &AtomicBool,
    ) {
        let mut parser = JsonlParser::default();
        let mut buffer = [0_u8; 8192];
        while !stop.load(Ordering::SeqCst) {
            match reader.read(&mut buffer) {
                Ok(0) => std::thread::sleep(Duration::from_millis(10)),
                Ok(size) => {
                    for record in parser.push(&buffer[..size]) {
                        let event = match record {
                            Ok(values) => ExportEvent::Snapshot {
                                producer: producer.to_string(),
                                generation,
                                values,
                            },
                            Err(reason) => ExportEvent::Invalid {
                                producer: producer.to_string(),
                                generation,
                                reason,
                            },
                        };
                        if events.send(event).is_err() {
                            return;
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return,
            }
        }
    }
}

#[cfg(unix)]
pub(crate) use platform::ExportEndpoint;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_split_and_combined_records() {
        let mut parser = JsonlParser::default();
        assert!(parser.push(br#"{"ONE":"1""#).is_empty());
        let records = parser.push(b"}\n{\"TWO\":\"2\"}\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].as_ref().unwrap()["ONE"], "1");
        assert_eq!(records[1].as_ref().unwrap()["TWO"], "2");
    }

    #[test]
    fn parser_recovers_after_oversized_record() {
        let mut parser = JsonlParser::default();
        let oversized = vec![b'x'; MAX_RECORD_BYTES + 1];
        assert!(parser.push(&oversized).pop().unwrap().is_err());
        let records = parser.push(b"discarded\n{\"OK\":\"yes\"}\n");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].as_ref().unwrap()["OK"], "yes");
    }

    #[test]
    fn parser_rejects_duplicates_invalid_names_and_non_strings() {
        for input in [
            b"{\"A\":\"1\",\"A\":\"2\"}\n".as_slice(),
            b"{\"BAD-NAME\":\"1\"}\n".as_slice(),
            b"{\"A\":1}\n".as_slice(),
        ] {
            assert!(JsonlParser::default().push(input).pop().unwrap().is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_is_private_and_publishes_without_logging_values() {
        use std::io::Write;
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        let endpoint = ExportEndpoint::create("producer", 7, tx).unwrap();
        let metadata = std::fs::metadata(endpoint.path()).unwrap();
        assert!(metadata.file_type().is_fifo());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(endpoint.path())
            .unwrap();
        writer.write_all(b"{\"TOKEN\":\"secret\"}\n").unwrap();
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ExportEvent::Snapshot {
                producer,
                generation,
                values,
            } => {
                assert_eq!(producer, "producer");
                assert_eq!(generation, 7);
                assert_eq!(values["TOKEN"], "secret");
            }
            event => panic!("unexpected event: {event:?}"),
        }
    }
}
