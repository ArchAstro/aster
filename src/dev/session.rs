use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::dashboard::ServiceState;
use super::process::LogEvent;

pub(crate) const ATTACH_SOCKET_ENV: &str = "ASTER_INTERNAL_SUPERVISOR_SOCKET";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ServiceSpec {
    pub name: String,
    pub port: Option<u16>,
    pub open_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SessionEvent {
    Snapshot {
        services: Vec<ServiceSpec>,
    },
    State {
        service: String,
        state: ServiceState,
    },
    Log(LogEvent),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub(crate) enum SessionCommand {
    Restart { service: String },
}

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::{Context, Result};
    use std::collections::HashMap;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub(crate) struct SessionServer {
        path: PathBuf,
        clients: Arc<Mutex<Vec<SyncSender<SessionEvent>>>>,
        states: Arc<Mutex<HashMap<String, ServiceState>>>,
        commands: Receiver<SessionCommand>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl SessionServer {
        pub(crate) fn from_environment(services: Vec<ServiceSpec>) -> Result<Option<Self>> {
            let Some(path) = std::env::var_os(ATTACH_SOCKET_ENV).map(PathBuf::from) else {
                return Ok(None);
            };
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    fs::remove_file(&path).with_context(|| {
                        format!(
                            "failed to remove stale supervisor socket {}",
                            path.display()
                        )
                    })?;
                }
                Ok(_) => anyhow::bail!(
                    "refusing to replace non-socket supervisor endpoint {}",
                    path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let listener = UnixListener::bind(&path)
                .with_context(|| format!("failed to bind supervisor socket {}", path.display()))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let clients = Arc::new(Mutex::new(Vec::new()));
            let thread_clients = clients.clone();
            let states = Arc::new(Mutex::new(
                services
                    .iter()
                    .map(|service| (service.name.clone(), ServiceState::Stopped))
                    .collect::<HashMap<_, _>>(),
            ));
            let thread_states = states.clone();
            let (command_tx, commands) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let handle = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let capacity = services.len().saturating_add(4000);
                            let (event_tx, event_rx) = mpsc::sync_channel(capacity);
                            if let Ok(mut clients) = thread_clients.lock() {
                                clients.push(event_tx.clone());
                            }
                            let _ = event_tx.send(SessionEvent::Snapshot {
                                services: services.clone(),
                            });
                            if let Ok(states) = thread_states.lock() {
                                for (service, state) in states.iter() {
                                    let _ = event_tx.send(SessionEvent::State {
                                        service: service.clone(),
                                        state: *state,
                                    });
                                }
                            }
                            spawn_connection(stream, event_rx, command_tx.clone());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(25));
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(Some(Self {
                path,
                clients,
                states,
                commands,
                stop,
                handle: Some(handle),
            }))
        }

        pub(crate) fn publish(&self, event: SessionEvent) {
            if let SessionEvent::State { service, state } = &event {
                if let Ok(mut states) = self.states.lock() {
                    states.insert(service.clone(), *state);
                }
            }
            if let Ok(mut clients) = self.clients.lock() {
                clients.retain(|client| match client.try_send(event.clone()) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
                    Err(mpsc::TrySendError::Disconnected(_)) => false,
                });
            }
        }

        pub(crate) fn try_command(&self) -> Option<SessionCommand> {
            self.commands.try_recv().ok()
        }
    }

    impl Drop for SessionServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            let _ = fs::remove_file(&self.path);
        }
    }

    fn spawn_connection(
        stream: UnixStream,
        events: Receiver<SessionEvent>,
        commands: mpsc::Sender<SessionCommand>,
    ) {
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                if serde_json::to_writer(&mut writer, &event).is_err()
                    || writeln!(writer).is_err()
                    || writer.flush().is_err()
                {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if let Ok(command) = serde_json::from_str(&line) {
                    if commands.send(command).is_err() {
                        break;
                    }
                }
            }
        });
    }

    pub(crate) struct SessionClient {
        path: PathBuf,
        pub(crate) events: Receiver<SessionEvent>,
    }

    impl SessionClient {
        pub(crate) fn connect(path: &Path) -> Result<Self> {
            let stream = UnixStream::connect(path).with_context(|| {
                format!("failed to attach to service supervisor {}", path.display())
            })?;
            let (tx, events) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(event) = serde_json::from_str(&line) {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                }
            });
            Ok(Self {
                path: path.to_path_buf(),
                events,
            })
        }

        pub(crate) fn send(&mut self, command: &SessionCommand) -> Result<()> {
            let mut stream = UnixStream::connect(&self.path).with_context(|| {
                format!(
                    "failed to send command to service supervisor {}",
                    self.path.display()
                )
            })?;
            serde_json::to_writer(&mut stream, command)?;
            writeln!(stream)?;
            stream.flush()?;
            Ok(())
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::{SessionClient, SessionServer};
