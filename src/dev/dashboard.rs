use std::collections::{HashMap, VecDeque};
use std::io;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;

use super::plan::ServicePlan;
use super::process::LogEvent;

const MAX_LINES: usize = 5000;
const MAX_LOG_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum ServiceState {
    Starting,
    Running,
    Restarting,
    Stopped,
}

impl ServiceState {
    fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Stopped => "stopped",
        }
    }
}

pub struct Dashboard {
    services: Vec<String>,
    ports: HashMap<String, Option<u16>>,
    urls: HashMap<String, Option<String>>,
    states: HashMap<String, ServiceState>,
    logs: HashMap<String, VecDeque<Line<'static>>>,
    log_bytes: HashMap<String, usize>,
    active: usize,
    scroll: u16,
    max_scroll: u16,
    wrap: bool,
    fullscreen: bool,
}

impl Dashboard {
    pub fn new(services: &[ServicePlan]) -> Self {
        Self {
            services: services
                .iter()
                .map(|service| service.name.clone())
                .collect(),
            ports: services
                .iter()
                .map(|service| (service.name.clone(), service.port))
                .collect(),
            urls: services
                .iter()
                .map(|service| (service.name.clone(), service.open_url.clone()))
                .collect(),
            states: services
                .iter()
                .map(|service| (service.name.clone(), ServiceState::Stopped))
                .collect(),
            logs: services
                .iter()
                .map(|service| (service.name.clone(), VecDeque::new()))
                .collect(),
            log_bytes: services
                .iter()
                .map(|service| (service.name.clone(), 0))
                .collect(),
            active: 0,
            scroll: u16::MAX,
            max_scroll: 0,
            wrap: true,
            fullscreen: false,
        }
    }

    pub fn active_name(&self) -> &str {
        &self.services[self.active]
    }

    pub fn active_url(&self) -> Option<&str> {
        self.urls.get(self.active_name()).and_then(Option::as_deref)
    }

    pub fn set_state(&mut self, service: &str, state: ServiceState) {
        self.states.insert(service.to_string(), state);
    }

    pub fn push_log(&mut self, event: LogEvent) {
        let style = if event.stderr {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        self.push_styled(&event.service, event.line, style);
    }

    pub fn push_system(&mut self, service: &str, line: impl Into<String>) {
        self.push_styled(service, line.into(), Style::default().fg(Color::Cyan));
    }

    fn push_styled(&mut self, service: &str, line: String, style: Style) {
        let Some(lines) = self.logs.get_mut(service) else {
            return;
        };
        let line = strip_ansi_sequences(&line);
        let bytes = self.log_bytes.entry(service.to_string()).or_default();
        *bytes = bytes.saturating_add(line.len());
        lines.push_back(Line::styled(line, style));
        while lines.len() > MAX_LINES || *bytes > MAX_LOG_BYTES {
            let Some(removed) = lines.pop_front() else {
                break;
            };
            let removed_bytes = removed
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
            *bytes = bytes.saturating_sub(removed_bytes);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return DashboardAction::Quit;
        }
        match key.code {
            KeyCode::Char('q') => DashboardAction::Quit,
            KeyCode::Char('r') => DashboardAction::Restart(self.active_name().to_string()),
            KeyCode::Char('o') => DashboardAction::Open,
            KeyCode::Char('h') | KeyCode::Left => {
                self.active = self.active.saturating_sub(1);
                self.scroll = u16::MAX;
                DashboardAction::Draw
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                self.active = (self.active + 1).min(self.services.len() - 1);
                self.scroll = u16::MAX;
                DashboardAction::Draw
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.scroll != u16::MAX {
                    self.scroll = self.scroll.saturating_add(1).min(self.max_scroll);
                }
                DashboardAction::Draw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = if self.scroll == u16::MAX {
                    self.max_scroll.saturating_sub(1)
                } else {
                    self.scroll.saturating_sub(1)
                };
                DashboardAction::Draw
            }
            KeyCode::Char('w') => {
                self.wrap = !self.wrap;
                DashboardAction::Draw
            }
            KeyCode::Char('f') => {
                self.fullscreen = !self.fullscreen;
                DashboardAction::Draw
            }
            KeyCode::Enter => {
                self.scroll = u16::MAX;
                DashboardAction::Draw
            }
            _ => DashboardAction::None,
        }
    }

    pub fn draw(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        workspace: &str,
        control_token_path: Option<&str>,
    ) -> Result<()> {
        let active = self.active_name().to_string();
        let logs = self.logs.get(&active).cloned().unwrap_or_default();
        let services = self.services.clone();
        let states = self.states.clone();
        let ports = self.ports.clone();
        let active_index = self.active;
        let wrap = self.wrap;
        let fullscreen = self.fullscreen;
        let scroll = self.scroll;
        let footer_height = if control_token_path.is_some() { 3 } else { 1 };

        terminal.draw(|frame| {
            let outer = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(4),
                    Constraint::Length(footer_height),
                ])
                .split(frame.area());
            frame.render_widget(
                Paragraph::new(workspace).style(Style::default().fg(Color::DarkGray)),
                outer[0],
            );
            let body = if fullscreen {
                vec![outer[1]]
            } else {
                Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(32), Constraint::Min(30)])
                    .split(outer[1])
                    .to_vec()
            };

            if !fullscreen {
                let items = services.iter().enumerate().map(|(index, name)| {
                    let state = states.get(name).copied().unwrap_or(ServiceState::Stopped);
                    let port = ports
                        .get(name)
                        .copied()
                        .flatten()
                        .map(|port| format!(" :{port}"))
                        .unwrap_or_default();
                    let style = if index == active_index {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{name}{port}"), style),
                        Span::styled(
                            format!("  {}", state.label()),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                });
                frame.render_widget(
                    List::new(items).block(Block::default().borders(Borders::ALL).title("services")),
                    body[0],
                );
            }

            let log_area = *body.last().expect("body always has a log area");
            let height = log_area.height.saturating_sub(2) as usize;
            let log_lines = logs.into_iter().collect::<Vec<_>>();
            let rendered_height = if wrap {
                Paragraph::new(log_lines.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(log_area.width.saturating_sub(2))
            } else {
                log_lines.len()
            };
            let max_scroll =
                u16::try_from(rendered_height.saturating_sub(height)).unwrap_or(u16::MAX);
            self.max_scroll = max_scroll;
            let scroll = if scroll == u16::MAX {
                max_scroll
            } else {
                let clamped = scroll.min(max_scroll);
                self.scroll = clamped;
                clamped
            };
            let paragraph = Paragraph::new(log_lines)
                .block(Block::default().borders(Borders::ALL).title(active))
                .scroll((scroll, 0));
            let paragraph = if wrap {
                paragraph.wrap(Wrap { trim: false })
            } else {
                paragraph
            };
            frame.render_widget(paragraph, log_area);
            let help =
                "h/l service  j/k scroll  Enter bottom  r restart  o open  w wrap  f fullscreen  q quit";
            let footer = control_token_path
                .map(|path| format!("control token: {path}\n{help}"))
                .unwrap_or_else(|| help.to_string());
            frame.render_widget(
                Paragraph::new(footer)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::DarkGray)),
                outer[2],
            );
        })?;
        Ok(())
    }
}

fn strip_ansi_sequences(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            output.push(bytes[index]);
            index += 1;
            continue;
        }

        index += 1;
        let Some(&kind) = bytes.get(index) else {
            break;
        };
        // ESC can be followed by ordinary UTF-8 in malformed service output.
        // Leave the complete non-ASCII code point for the normal copy path.
        if !kind.is_ascii() {
            continue;
        }
        index += 1;
        match kind {
            // Control Sequence Introducer: consume through its final byte.
            b'[' => {
                while let Some(&byte) = bytes.get(index) {
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            // Operating System Command: terminated by BEL or String Terminator.
            b']' => {
                while let Some(&byte) = bytes.get(index) {
                    index += 1;
                    if byte == 0x07 {
                        break;
                    }
                    if byte == 0x1b && bytes.get(index) == Some(&b'\\') {
                        index += 1;
                        break;
                    }
                }
            }
            // DCS, SOS, PM, and APC strings are terminated by ESC backslash.
            b'P' | b'X' | b'^' | b'_' => {
                while let Some(&byte) = bytes.get(index) {
                    index += 1;
                    if byte == 0x1b && bytes.get(index) == Some(&b'\\') {
                        index += 1;
                        break;
                    }
                }
            }
            // Other escape sequences are two bytes long.
            _ => {}
        }
    }
    String::from_utf8(output).expect("removing ASCII escape sequences preserves UTF-8")
}

pub enum DashboardAction {
    None,
    Draw,
    Restart(String),
    Open,
    Quit,
}

pub struct TerminalGuard {
    pub terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = stdout.execute(EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use super::strip_ansi_sequences;

    #[test]
    fn strips_terminal_control_sequences_from_service_logs() {
        assert_eq!(
            strip_ansi_sequences(
                "\u{1b}[31mred\u{1b}[0m plain \u{1b}]0;title\u{7}text \u{1b}[2Kdone"
            ),
            "red plain text done"
        );
        assert_eq!(strip_ansi_sequences("unicode 🦀 stays"), "unicode 🦀 stays");
        assert_eq!(strip_ansi_sequences("\u{1b}é"), "é");
    }
}
