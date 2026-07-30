use std::collections::VecDeque;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::plan::ServicePlan;
use super::process::LogEvent;

const MAX_LINES: usize = 5000;
const MAX_LOG_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_SERVICE_LIST_WIDTH: u16 = 38;
const MIN_SERVICE_LIST_WIDTH: u16 = 30;
const MIN_LOG_PANEL_WIDTH: u16 = 28;
const DIVIDER_WIDTH: u16 = 1;
const DIVIDER_RESIZE_STEP: i16 = 2;
const SERVICE_LIST_HEADER_ROWS: u16 = 2;
const SERVICE_TAB_ROWS: u16 = 3;
const OPEN_LINK_LABEL: &str = "[open]";
const COPY_FLASH_DURATION: Duration = Duration::from_millis(350);
const SERVICE_COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Green,
    Color::LightCyan,
    Color::Yellow,
    Color::Blue,
];

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

#[derive(Clone)]
struct LogLine {
    text: String,
    style: Style,
}

struct ServicePane {
    name: String,
    port: Option<u16>,
    url: Option<String>,
    color: Color,
    state: ServiceState,
    lines: VecDeque<LogLine>,
    log_bytes: usize,
    wrap_cache: Option<WrapRowCache>,
    scroll_top: usize,
    last_view_height: usize,
    last_view_width: usize,
}

struct WrapRowCache {
    width: usize,
    rows: VecDeque<usize>,
    total_rows: usize,
}

impl ServicePane {
    fn ensure_wrap_cache(&mut self) {
        let width = self.last_view_width.max(1);
        if self
            .wrap_cache
            .as_ref()
            .is_some_and(|cache| cache.width == width && cache.rows.len() == self.lines.len())
        {
            return;
        }
        let rows = self
            .lines
            .iter()
            .map(|line| Self::visual_rows(&line.text, width))
            .collect::<VecDeque<_>>();
        let total_rows = rows.iter().sum();
        self.wrap_cache = Some(WrapRowCache {
            width,
            rows,
            total_rows,
        });
    }

    fn visual_rows(text: &str, width: usize) -> usize {
        if width == 0 {
            return 1;
        }
        let line = Line::from(text.to_string());
        wrapped_line_rows(&line, width, 0, usize::MAX).len().max(1)
    }

    fn line_position_at_visual_offset(&mut self, visual_offset: usize) -> Option<(usize, usize)> {
        if self.lines.is_empty() {
            return None;
        }
        self.ensure_wrap_cache();
        let cache = self.wrap_cache.as_ref()?;
        let mut cumulative = 0;
        for (index, rows) in cache.rows.iter().copied().enumerate() {
            if cumulative + rows > visual_offset {
                return Some((index, visual_offset.saturating_sub(cumulative)));
            }
            cumulative += rows;
        }
        let rows = cache.rows.back().copied().unwrap_or(1);
        Some((self.lines.len() - 1, rows.saturating_sub(1)))
    }

    fn visual_offset_for_line(&mut self, line_index: usize) -> usize {
        self.ensure_wrap_cache();
        self.wrap_cache
            .as_ref()
            .map(|cache| cache.rows.iter().take(line_index).sum())
            .unwrap_or(0)
    }

    fn max_scroll_top(&mut self, wrap: bool) -> usize {
        if wrap {
            self.ensure_wrap_cache();
            self.wrap_cache
                .as_ref()
                .map(|cache| cache.total_rows.saturating_sub(self.last_view_height))
                .unwrap_or(0)
        } else {
            self.lines.len().saturating_sub(self.last_view_height)
        }
    }

    fn resolved_scroll_top(&mut self, wrap: bool) -> usize {
        if self.scroll_top == usize::MAX {
            self.max_scroll_top(wrap)
        } else {
            self.scroll_top.min(self.max_scroll_top(wrap))
        }
    }

    fn scroll_lines(&mut self, delta: isize, wrap: bool) {
        let current = self.resolved_scroll_top(wrap);
        self.scroll_top = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(self.max_scroll_top(wrap))
        };
    }

    fn scroll_page(&mut self, direction: isize, half: bool, wrap: bool) {
        let amount = if half {
            (self.last_view_height / 2).max(1)
        } else {
            self.last_view_height.max(1)
        };
        self.scroll_lines(direction.saturating_mul(amount as isize), wrap);
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_top = usize::MAX;
    }

    fn push(&mut self, line: String, style: Style, wrap: bool) {
        let following = self.scroll_top == usize::MAX
            || self.scroll_top >= self.max_scroll_top(wrap).saturating_sub(1);
        self.log_bytes = self.log_bytes.saturating_add(line.len());
        self.lines.push_back(LogLine { text: line, style });
        self.wrap_cache = None;
        while self.lines.len() > MAX_LINES || self.log_bytes > MAX_LOG_BYTES {
            let Some(removed) = self.lines.pop_front() else {
                break;
            };
            self.log_bytes = self.log_bytes.saturating_sub(removed.text.len());
            if self.scroll_top != usize::MAX {
                let removed_rows = if wrap {
                    Self::visual_rows(&removed.text, self.last_view_width.max(1))
                } else {
                    1
                };
                self.scroll_top = self.scroll_top.saturating_sub(removed_rows);
            }
        }
        if following {
            self.scroll_top = usize::MAX;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search,
}

pub struct Dashboard {
    panes: Vec<ServicePane>,
    active: usize,
    wrap: bool,
    fullscreen: bool,
    show_help: bool,
    input_mode: InputMode,
    search_input: String,
    search_hits: Vec<usize>,
    search_cursor: usize,
    status: String,
    service_list_width: u16,
    service_list_scroll: usize,
    service_list_manual_scroll: bool,
    service_list_view_height: usize,
    last_workspace_width: u16,
    divider_dragging: bool,
    mouse_enabled: bool,
    selection_anchor: Option<(usize, usize)>,
    selection_range: Option<(usize, usize, usize)>,
    selection_dragging: bool,
    copied_flash_range: Option<(usize, usize, usize)>,
    copied_flash_until: Option<Instant>,
}

impl Dashboard {
    pub fn new(services: &[ServicePlan]) -> Self {
        Self::from_specs(
            services
                .iter()
                .map(|service| (service.name.clone(), service.port, service.open_url.clone()))
                .collect(),
        )
    }

    fn from_specs(services: Vec<(String, Option<u16>, Option<String>)>) -> Self {
        Self {
            panes: services
                .into_iter()
                .enumerate()
                .map(|(index, (name, port, url))| ServicePane {
                    name,
                    port,
                    url,
                    color: SERVICE_COLORS[index % SERVICE_COLORS.len()],
                    state: ServiceState::Stopped,
                    lines: VecDeque::new(),
                    log_bytes: 0,
                    wrap_cache: None,
                    scroll_top: usize::MAX,
                    last_view_height: 1,
                    last_view_width: 80,
                })
                .collect(),
            active: 0,
            wrap: true,
            fullscreen: false,
            show_help: false,
            input_mode: InputMode::Normal,
            search_input: String::new(),
            search_hits: Vec::new(),
            search_cursor: 0,
            status: "ready".to_string(),
            service_list_width: DEFAULT_SERVICE_LIST_WIDTH,
            service_list_scroll: 0,
            service_list_manual_scroll: false,
            service_list_view_height: 1,
            last_workspace_width: 120,
            divider_dragging: false,
            mouse_enabled: true,
            selection_anchor: None,
            selection_range: None,
            selection_dragging: false,
            copied_flash_range: None,
            copied_flash_until: None,
        }
    }

    pub fn active_name(&self) -> &str {
        &self.panes[self.active].name
    }

    pub fn active_url(&self) -> Option<&str> {
        self.panes[self.active].url.as_deref()
    }

    pub fn set_state(&mut self, service: &str, state: ServiceState) {
        if let Some(pane) = self.panes.iter_mut().find(|pane| pane.name == service) {
            pane.state = state;
        }
    }

    pub fn push_log(&mut self, event: LogEvent) {
        let style = if event.stderr {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            let lowercase = event.line.to_ascii_lowercase();
            if lowercase.contains("error") {
                Style::default().fg(Color::Red)
            } else if lowercase.contains("warn") {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            }
        };
        self.push_styled(&event.service, event.line, style);
    }

    pub fn push_system(&mut self, service: &str, line: impl Into<String>) {
        self.push_styled(service, line.into(), Style::default().fg(Color::Cyan));
    }

    fn push_styled(&mut self, service: &str, line: String, style: Style) {
        let wrap = self.wrap;
        if let Some(pane) = self.panes.iter_mut().find(|pane| pane.name == service) {
            pane.push(strip_ansi_sequences(&line), style, wrap);
        }
    }

    fn set_active(&mut self, index: usize) {
        if index >= self.panes.len() {
            return;
        }
        self.active = index;
        self.service_list_manual_scroll = false;
        self.clear_selection();
        if self.input_mode == InputMode::Search {
            self.refresh_search();
        }
        self.status = format!("active service: {}", self.active_name());
    }

    fn active_pane_mut(&mut self) -> &mut ServicePane {
        &mut self.panes[self.active]
    }

    fn root_layout(&self, area: Rect, control_token_path: Option<&str>) -> Vec<Rect> {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(if self.input_mode == InputMode::Search {
                    1
                } else {
                    0
                }),
                Constraint::Length(u16::from(control_token_path.is_some())),
            ])
            .split(area)
            .to_vec()
    }

    fn workspace_layout(
        &self,
        area: Rect,
        control_token_path: Option<&str>,
    ) -> (Option<Rect>, Option<Rect>, Rect) {
        let workspace = self.root_layout(area, control_token_path)[1];
        if self.fullscreen {
            return (None, None, workspace);
        }
        let list_width = self.clamp_service_list_width(self.service_list_width, workspace.width);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(list_width),
                Constraint::Length(DIVIDER_WIDTH),
                Constraint::Min(1),
            ])
            .split(workspace);
        (Some(columns[0]), Some(columns[1]), columns[2])
    }

    fn clamp_service_list_width(&self, requested: u16, total_width: u16) -> u16 {
        let available = total_width.saturating_sub(DIVIDER_WIDTH);
        if available <= 2 {
            return available / 2;
        }
        let min_left = MIN_SERVICE_LIST_WIDTH.min(available / 2).max(1);
        let min_right = MIN_LOG_PANEL_WIDTH
            .min(available.saturating_sub(min_left))
            .max(1);
        requested.clamp(min_left, available.saturating_sub(min_right).max(min_left))
    }

    fn resize_service_list(&mut self, delta: i16) {
        let requested = if delta.is_negative() {
            self.service_list_width.saturating_sub(delta.unsigned_abs())
        } else {
            self.service_list_width.saturating_add(delta as u16)
        };
        self.service_list_width =
            self.clamp_service_list_width(requested, self.last_workspace_width);
        self.status = format!("service list width: {}", self.service_list_width);
    }

    fn set_service_list_width_from_column(&mut self, column: u16, workspace: Rect) {
        let requested = column.saturating_sub(workspace.x);
        self.service_list_width = self.clamp_service_list_width(requested, workspace.width);
        self.status = format!("service list width: {}", self.service_list_width);
    }

    fn service_list_body_rect(area: Rect) -> Rect {
        Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(SERVICE_LIST_HEADER_ROWS),
            area.width.saturating_sub(2),
            area.height.saturating_sub(SERVICE_LIST_HEADER_ROWS),
        )
    }

    fn keep_active_service_visible(&mut self) {
        let visible = self.service_list_view_height.max(1);
        if self.active < self.service_list_scroll {
            self.service_list_scroll = self.active;
        } else if self.active >= self.service_list_scroll.saturating_add(visible) {
            self.service_list_scroll = self.active.saturating_add(1).saturating_sub(visible);
        }
        self.service_list_scroll = self
            .service_list_scroll
            .min(self.panes.len().saturating_sub(visible));
    }

    fn scroll_service_list(&mut self, delta: isize) {
        let max = self
            .panes
            .len()
            .saturating_sub(self.service_list_view_height.max(1));
        self.service_list_scroll = if delta.is_negative() {
            self.service_list_scroll
                .saturating_sub(delta.unsigned_abs())
        } else {
            self.service_list_scroll
                .saturating_add(delta as usize)
                .min(max)
        };
        self.service_list_manual_scroll = true;
        self.status = format!(
            "services {}-{}/{}",
            self.service_list_scroll.saturating_add(1),
            self.service_list_scroll
                .saturating_add(self.service_list_view_height)
                .min(self.panes.len()),
            self.panes.len()
        );
    }

    fn render_service_list(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "services",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )),
            Rect::new(
                area.x.saturating_add(1),
                area.y,
                area.width.saturating_sub(2),
                1,
            ),
        );

        let inner = Self::service_list_body_rect(area);
        let visible = (inner.height / SERVICE_TAB_ROWS) as usize;
        if visible == 0 {
            return;
        }
        self.service_list_view_height = visible;
        if self.service_list_manual_scroll {
            self.service_list_scroll = self
                .service_list_scroll
                .min(self.panes.len().saturating_sub(visible));
        } else {
            self.keep_active_service_visible();
        }

        let range = if self.panes.len() > visible {
            format!(
                "{}-{}/{}",
                self.service_list_scroll + 1,
                (self.service_list_scroll + visible).min(self.panes.len()),
                self.panes.len()
            )
        } else {
            self.panes.len().to_string()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(range, Style::default().fg(Color::DarkGray)))
                .alignment(Alignment::Right),
            Rect::new(
                area.x.saturating_add(1),
                area.y,
                area.width.saturating_sub(2),
                1,
            ),
        );

        for (slot, index) in (self.service_list_scroll..self.panes.len())
            .take(visible)
            .enumerate()
        {
            let pane = &self.panes[index];
            let selected = index == self.active;
            let tab_area = Rect::new(
                inner.x,
                inner.y + slot as u16 * SERVICE_TAB_ROWS,
                inner.width,
                SERVICE_TAB_ROWS,
            );
            let row_style = if selected {
                Style::default()
                    .bg(Color::Rgb(38, 40, 50))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            if selected {
                frame.render_widget(Block::default().style(row_style), tab_area);
            }
            let title_style = if selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("● ", Style::default().fg(pane.color)),
                    Span::styled(pane.name.clone(), title_style),
                ]))
                .style(row_style),
                Rect::new(tab_area.x, tab_area.y, tab_area.width, 1),
            );

            let state_color = match pane.state {
                ServiceState::Starting | ServiceState::Restarting => Color::Yellow,
                ServiceState::Running => Color::Green,
                ServiceState::Stopped => Color::DarkGray,
            };
            let mut metadata = vec![
                Span::raw("  "),
                Span::styled(pane.state.label(), Style::default().fg(state_color)),
            ];
            if let Some(port) = pane.port {
                metadata.push(Span::styled(
                    format!("  ·  localhost:{port}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            frame.render_widget(
                Paragraph::new(Line::from(metadata)).style(row_style),
                Rect::new(tab_area.x, tab_area.y.saturating_add(1), tab_area.width, 1),
            );

            if pane.url.is_some() && inner.width >= OPEN_LINK_LABEL.len() as u16 {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        OPEN_LINK_LABEL,
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD),
                    ))
                    .alignment(Alignment::Right)
                    .style(row_style),
                    Rect::new(
                        inner
                            .x
                            .saturating_add(inner.width)
                            .saturating_sub(OPEN_LINK_LABEL.len() as u16),
                        tab_area.y,
                        OPEN_LINK_LABEL.len() as u16,
                        1,
                    ),
                );
            }
        }
    }

    fn render(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        workspace: &str,
        control_token_path: Option<&str>,
    ) {
        self.clear_copy_flash_if_expired();
        let root = self.root_layout(frame.area(), control_token_path);
        self.last_workspace_width = root[1].width;

        let header = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(root[0]);
        frame.render_widget(
            Paragraph::new(workspace.to_string()).style(Style::default().fg(Color::Gray)),
            header[0],
        );
        frame.render_widget(
            Paragraph::new(self.status.as_str())
                .alignment(Alignment::Right)
                .style(Style::default().fg(Color::DarkGray)),
            header[1],
        );

        let (service_list, divider, logs) = self.workspace_layout(frame.area(), control_token_path);
        if let Some(service_list) = service_list {
            self.render_service_list(frame, service_list);
        }
        if let Some(divider) = divider {
            let style = if self.divider_dragging {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            frame.render_widget(
                Paragraph::new(
                    (0..divider.height)
                        .map(|_| Line::from(Span::styled("│", style)))
                        .collect::<Vec<_>>(),
                ),
                divider,
            );
        }

        let active = self.active;
        let search_query = if self.input_mode == InputMode::Search {
            let query = self.search_input.trim();
            (!query.is_empty()).then_some(query.to_string())
        } else {
            None
        };
        let selected_hit = self.search_hits.get(self.search_cursor).copied();
        let selection = self.selection_range.and_then(|(service, start, end)| {
            (service == active).then_some((start.min(end), start.max(end)))
        });
        let flash = self.copied_flash_range.and_then(|(service, start, end)| {
            (service == active).then_some((start.min(end), start.max(end)))
        });
        let pane = &mut self.panes[active];
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(pane.color))
            .title(
                Line::from(Span::styled(
                    format!(" Logs — {} ", pane.name),
                    Style::default().fg(pane.color).add_modifier(Modifier::BOLD),
                ))
                .left_aligned(),
            );
        let inner = block.inner(logs);
        pane.last_view_height = (inner.height as usize).max(1);
        pane.last_view_width = (inner.width as usize).max(1);
        if self.wrap {
            let visual_scroll = pane.resolved_scroll_top(true);
            if pane.scroll_top != usize::MAX {
                pane.scroll_top = visual_scroll;
            }
            let (start, first_line_offset) = pane
                .line_position_at_visual_offset(visual_scroll)
                .unwrap_or((0, 0));
            let mut remaining_rows = pane.last_view_height;
            let mut rendered_rows = Vec::new();
            for (offset, line) in pane.lines.iter().skip(start).enumerate() {
                if remaining_rows == 0 {
                    break;
                }
                let index = start + offset;
                let mut rendered = highlight_line(
                    &line.text,
                    line.style,
                    search_query.as_deref(),
                    selected_hit == Some(index),
                );
                let selected = selection
                    .map(|(from, to)| (from..=to).contains(&index))
                    .unwrap_or(false);
                let flashed = flash
                    .map(|(from, to)| (from..=to).contains(&index))
                    .unwrap_or(false);
                for span in &mut rendered.spans {
                    if flashed {
                        span.style = span.style.patch(
                            Style::default()
                                .bg(Color::LightGreen)
                                .fg(Color::Black)
                                .add_modifier(Modifier::BOLD),
                        );
                    } else if selected && span.style.bg.is_none() {
                        span.style = span.style.patch(Style::default().bg(Color::DarkGray));
                    }
                }
                let rows = wrapped_line_rows(
                    &rendered,
                    pane.last_view_width,
                    if offset == 0 { first_line_offset } else { 0 },
                    remaining_rows,
                );
                remaining_rows = remaining_rows.saturating_sub(rows.len());
                rendered_rows.extend(rows);
            }
            frame.render_widget(block, logs);
            render_prewrapped_rows(frame, inner, &rendered_rows);
        } else {
            let start = pane.resolved_scroll_top(false);
            if pane.scroll_top != usize::MAX {
                pane.scroll_top = start;
            }
            let lines = pane
                .lines
                .iter()
                .skip(start)
                .take(pane.last_view_height)
                .enumerate()
                .map(|(offset, line)| {
                    let index = start + offset;
                    let mut rendered = highlight_line(
                        &line.text,
                        line.style,
                        search_query.as_deref(),
                        selected_hit == Some(index),
                    );
                    let selected = selection
                        .map(|(from, to)| (from..=to).contains(&index))
                        .unwrap_or(false);
                    let flashed = flash
                        .map(|(from, to)| (from..=to).contains(&index))
                        .unwrap_or(false);
                    for span in &mut rendered.spans {
                        if flashed {
                            span.style = span.style.patch(
                                Style::default()
                                    .bg(Color::LightGreen)
                                    .fg(Color::Black)
                                    .add_modifier(Modifier::BOLD),
                            );
                        } else if selected && span.style.bg.is_none() {
                            span.style = span.style.patch(Style::default().bg(Color::DarkGray));
                        }
                    }
                    rendered
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines).block(block), logs);
        }

        frame.render_widget(
            Paragraph::new(
                "h/l service   j/k scroll   [/] resize split   f fullscreen   w wrap   Enter bottom   m mouse   click service/[open]   drag split/select logs   y copy   Ctrl+f/b page   Ctrl+d/u half   / search   r restart   ? help   q quit",
            )
            .style(Style::default().fg(Color::DarkGray)),
            root[2],
        );

        if self.input_mode == InputMode::Search {
            let position = if self.search_hits.is_empty() {
                "0/0".to_string()
            } else {
                format!("{}/{}", self.search_cursor + 1, self.search_hits.len())
            };
            frame.render_widget(
                Paragraph::new(format!(
                    "/{}  [{}]  Enter done  Esc cancel  Ctrl+n next  Ctrl+p/Ctrl+Shift+n prev",
                    self.search_input, position
                ))
                .style(Style::default().fg(Color::White)),
                root[3],
            );
        }
        if let Some(path) = control_token_path {
            frame.render_widget(
                Paragraph::new(format!("control token: {path}"))
                    .style(Style::default().fg(Color::DarkGray)),
                root[4],
            );
        }
        if self.show_help {
            self.render_help_overlay(frame);
        }
    }

    pub fn draw(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        workspace: &str,
        control_token_path: Option<&str>,
    ) -> Result<()> {
        terminal.draw(|frame| self.render(frame, workspace, control_token_path))?;
        Ok(())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return DashboardAction::Quit;
        }
        if self.input_mode == InputMode::Search {
            return self.handle_search_key(key);
        }
        if self.show_help {
            return match key.code {
                KeyCode::Char('?') | KeyCode::Esc => {
                    self.show_help = false;
                    self.status = "controls hidden".to_string();
                    DashboardAction::Draw
                }
                KeyCode::Char('q') => DashboardAction::Quit,
                _ => DashboardAction::None,
            };
        }

        match key.code {
            KeyCode::Char('q') => DashboardAction::Quit,
            KeyCode::Char('r') => DashboardAction::Restart(self.active_name().to_string()),
            KeyCode::Char('o') => DashboardAction::Open,
            KeyCode::Char('m') => {
                self.mouse_enabled = !self.mouse_enabled;
                self.status = if self.mouse_enabled {
                    "mouse mode on: dashboard click/scroll enabled".to_string()
                } else {
                    "mouse mode off: terminal text selection enabled".to_string()
                };
                DashboardAction::ToggleMouse(self.mouse_enabled)
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                self.status = "controls shown (? / Esc to close)".to_string();
                DashboardAction::Draw
            }
            KeyCode::Char('f') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.fullscreen = !self.fullscreen;
                self.status = if self.fullscreen {
                    format!("fullscreen: {}", self.active_name())
                } else {
                    "service list restored".to_string()
                };
                DashboardAction::Draw
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let next = if self.active == 0 {
                    self.panes.len() - 1
                } else {
                    self.active - 1
                };
                self.set_active(next);
                DashboardAction::Draw
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                self.set_active((self.active + 1) % self.panes.len());
                DashboardAction::Draw
            }
            KeyCode::Char('[') => {
                self.resize_service_list(-DIVIDER_RESIZE_STEP);
                DashboardAction::Draw
            }
            KeyCode::Char(']') => {
                self.resize_service_list(DIVIDER_RESIZE_STEP);
                DashboardAction::Draw
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let wrap = self.wrap;
                self.active_pane_mut().scroll_lines(1, wrap);
                DashboardAction::Draw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let wrap = self.wrap;
                self.active_pane_mut().scroll_lines(-1, wrap);
                DashboardAction::Draw
            }
            KeyCode::PageDown | KeyCode::Char('f')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let wrap = self.wrap;
                self.active_pane_mut().scroll_page(1, false, wrap);
                DashboardAction::Draw
            }
            KeyCode::PageUp | KeyCode::Char('b')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let wrap = self.wrap;
                self.active_pane_mut().scroll_page(-1, false, wrap);
                DashboardAction::Draw
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let wrap = self.wrap;
                self.active_pane_mut().scroll_page(1, true, wrap);
                DashboardAction::Draw
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let wrap = self.wrap;
                self.active_pane_mut().scroll_page(-1, true, wrap);
                DashboardAction::Draw
            }
            KeyCode::Enter => {
                self.active_pane_mut().scroll_to_bottom();
                self.status = "scrolled to bottom".to_string();
                DashboardAction::Draw
            }
            KeyCode::Char('w') => {
                let was_wrapped = self.wrap;
                self.wrap = !self.wrap;
                for pane in &mut self.panes {
                    if pane.scroll_top == usize::MAX {
                        continue;
                    }
                    pane.scroll_top = if self.wrap && !was_wrapped {
                        pane.visual_offset_for_line(pane.scroll_top)
                    } else if !self.wrap && was_wrapped {
                        pane.line_position_at_visual_offset(pane.scroll_top)
                            .map(|(line, _)| line)
                            .unwrap_or(0)
                    } else {
                        pane.scroll_top
                    };
                    pane.scroll_top = pane.scroll_top.min(pane.max_scroll_top(self.wrap));
                }
                self.status = if self.wrap {
                    "line wrapping enabled".to_string()
                } else {
                    "line wrapping disabled".to_string()
                };
                DashboardAction::Draw
            }
            KeyCode::Char('/') => {
                self.input_mode = InputMode::Search;
                self.search_input.clear();
                self.refresh_search();
                DashboardAction::Draw
            }
            KeyCode::Char('y') => {
                if let Err(error) = self.copy_selection() {
                    self.status = format!("copy failed: {error}");
                }
                DashboardAction::Draw
            }
            KeyCode::Esc => {
                self.clear_selection();
                self.status = "selection cleared".to_string();
                DashboardAction::Draw
            }
            _ => DashboardAction::None,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> DashboardAction {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.search_input.clear();
                self.search_hits.clear();
                self.search_cursor = 0;
                self.status = "search canceled".to_string();
            }
            KeyCode::Enter => {
                let query = self.search_input.trim().to_string();
                self.status = if query.is_empty() {
                    "search cleared".to_string()
                } else {
                    format!("search done: {query} ({})", self.search_hits.len())
                };
                self.input_mode = InputMode::Normal;
                self.search_input.clear();
                self.search_hits.clear();
                self.search_cursor = 0;
            }
            KeyCode::Backspace => {
                self.search_input.pop();
                self.refresh_search();
            }
            KeyCode::Up => self.search_next(false),
            KeyCode::Down => self.search_next(true),
            KeyCode::Char(character)
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(character, 'n' | 'N' | 'p') =>
            {
                self.search_next(!matches!(character, 'N' | 'p'));
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.push(character);
                self.refresh_search();
            }
            _ => {}
        }
        DashboardAction::Draw
    }

    fn refresh_search(&mut self) {
        let query = self.search_input.trim().to_ascii_lowercase();
        self.search_hits.clear();
        self.search_cursor = 0;
        if query.is_empty() {
            return;
        }
        self.search_hits = self.panes[self.active]
            .lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                line.text
                    .to_ascii_lowercase()
                    .contains(&query)
                    .then_some(index)
            })
            .collect();
        self.jump_to_search_hit();
    }

    fn search_next(&mut self, forward: bool) {
        if self.search_hits.is_empty() {
            return;
        }
        self.search_cursor = if forward {
            (self.search_cursor + 1) % self.search_hits.len()
        } else if self.search_cursor == 0 {
            self.search_hits.len() - 1
        } else {
            self.search_cursor - 1
        };
        self.jump_to_search_hit();
    }

    fn jump_to_search_hit(&mut self) {
        if let Some(&line) = self.search_hits.get(self.search_cursor) {
            let wrap = self.wrap;
            let pane = &mut self.panes[self.active];
            pane.scroll_top = if wrap {
                pane.visual_offset_for_line(line).saturating_sub(2)
            } else {
                line.saturating_sub(2)
            };
            pane.scroll_top = pane.scroll_top.min(pane.max_scroll_top(wrap));
        }
    }

    pub fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        control_token_path: Option<&str>,
    ) -> DashboardAction {
        if !self.mouse_enabled || self.show_help {
            return DashboardAction::None;
        }
        let (service_list, divider, logs) = self.workspace_layout(area, control_token_path);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if divider.is_some_and(|rect| rect.contains((mouse.column, mouse.row).into())) {
                    self.divider_dragging = true;
                    self.clear_selection();
                    self.status = "resizing service list".to_string();
                    return DashboardAction::Draw;
                }
                if let Some(list) = service_list {
                    if let Some(index) = self.service_index_at(list, mouse.column, mouse.row) {
                        let open = self.open_link_contains(list, index, mouse.column, mouse.row);
                        self.set_active(index);
                        return if open {
                            DashboardAction::Open
                        } else {
                            DashboardAction::Draw
                        };
                    }
                }
                if logs.contains((mouse.column, mouse.row).into()) {
                    if let Some(line) = self.log_line_at(logs, mouse.row) {
                        self.selection_anchor = Some((self.active, line));
                        self.selection_range = Some((self.active, line, line));
                        self.selection_dragging = false;
                    }
                    return DashboardAction::Draw;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.divider_dragging {
                    let workspace = self.root_layout(area, control_token_path)[1];
                    self.set_service_list_width_from_column(mouse.column, workspace);
                    return DashboardAction::Draw;
                }
                if let Some((service, anchor)) = self.selection_anchor {
                    if service == self.active && logs.contains((mouse.column, mouse.row).into()) {
                        if let Some(line) = self.log_line_at(logs, mouse.row) {
                            self.selection_range = Some((service, anchor, line));
                            self.selection_dragging = true;
                        }
                    }
                    return DashboardAction::Draw;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.divider_dragging {
                    let workspace = self.root_layout(area, control_token_path)[1];
                    self.set_service_list_width_from_column(mouse.column, workspace);
                    self.divider_dragging = false;
                    return DashboardAction::Draw;
                }
                if self.selection_dragging {
                    if let Some((_, start, end)) = self.selection_range {
                        self.status = format!(
                            "selected {} line(s) in {} (press y to copy)",
                            start.max(end).saturating_sub(start.min(end)) + 1,
                            self.active_name()
                        );
                    }
                }
                self.selection_anchor = None;
                self.selection_dragging = false;
                return DashboardAction::Draw;
            }
            MouseEventKind::ScrollUp => {
                if service_list.is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
                {
                    self.scroll_service_list(-1);
                } else if logs.contains((mouse.column, mouse.row).into()) {
                    let wrap = self.wrap;
                    self.active_pane_mut().scroll_lines(-3, wrap);
                }
                return DashboardAction::Draw;
            }
            MouseEventKind::ScrollDown => {
                if service_list.is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
                {
                    self.scroll_service_list(1);
                } else if logs.contains((mouse.column, mouse.row).into()) {
                    let wrap = self.wrap;
                    self.active_pane_mut().scroll_lines(3, wrap);
                }
                return DashboardAction::Draw;
            }
            _ => {}
        }
        DashboardAction::None
    }

    fn service_index_at(&self, list: Rect, x: u16, y: u16) -> Option<usize> {
        let inner = Self::service_list_body_rect(list);
        if !inner.contains((x, y).into()) {
            return None;
        }
        let index = self
            .service_list_scroll
            .saturating_add(((y - inner.y) / SERVICE_TAB_ROWS) as usize);
        (index < self.panes.len()).then_some(index)
    }

    fn open_link_contains(&self, list: Rect, index: usize, x: u16, y: u16) -> bool {
        let inner = Self::service_list_body_rect(list);
        let slot = index.saturating_sub(self.service_list_scroll) as u16;
        let row = inner
            .y
            .saturating_add(slot.saturating_mul(SERVICE_TAB_ROWS));
        self.panes[index].url.is_some()
            && y == row
            && x >= inner
                .x
                .saturating_add(inner.width)
                .saturating_sub(OPEN_LINK_LABEL.len() as u16)
    }

    fn log_line_at(&mut self, logs: Rect, y: u16) -> Option<usize> {
        let inner = Rect::new(
            logs.x.saturating_add(1),
            logs.y.saturating_add(1),
            logs.width.saturating_sub(2),
            logs.height.saturating_sub(2),
        );
        if y < inner.y || y >= inner.y.saturating_add(inner.height) {
            return None;
        }
        let wrap = self.wrap;
        let pane = &mut self.panes[self.active];
        let offset = pane
            .resolved_scroll_top(wrap)
            .saturating_add((y - inner.y) as usize);
        let index = if wrap {
            pane.line_position_at_visual_offset(offset)
                .map(|(line, _)| line)?
        } else {
            offset
        };
        (index < pane.lines.len()).then_some(index)
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_range = None;
        self.selection_dragging = false;
    }

    fn copy_selection(&mut self) -> Result<()> {
        let Some((service, start, end)) = self.selection_range else {
            return Err(anyhow!("no selection"));
        };
        let from = start.min(end);
        let to = start.max(end);
        let text = self.panes[service]
            .lines
            .iter()
            .skip(from)
            .take(to.saturating_sub(from) + 1)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return Err(anyhow!("selection is empty"));
        }
        copy_to_clipboard(&text)?;
        let pane_name = self.panes[service].name.clone();
        self.copied_flash_range = Some((service, from, to));
        self.copied_flash_until = Some(Instant::now() + COPY_FLASH_DURATION);
        self.clear_selection();
        self.status = format!(
            "copied {} line(s) from {pane_name}",
            to.saturating_sub(from) + 1
        );
        Ok(())
    }

    fn clear_copy_flash_if_expired(&mut self) {
        if self
            .copied_flash_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.copied_flash_until = None;
            self.copied_flash_range = None;
        }
    }

    fn render_help_overlay(&self, frame: &mut ratatui::Frame<'_>) {
        let area = centered_rect(80, 80, frame.area());
        let lines = vec![
            Line::from(Span::styled(
                "Controls",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("Service focus: h/l or Left/Right"),
            Line::from("Resize service/log split: drag divider or [ / ]"),
            Line::from("Fullscreen: f toggles active logs fullscreen"),
            Line::from("Mouse mode toggle: m"),
            Line::from("Mouse mode on: click [open], wheel lists/logs, drag selections"),
            Line::from("Mouse mode off: native terminal text selection works"),
            Line::from("Log copy: drag lines, y to copy, Esc to clear"),
            Line::from("Scroll: j/k or Up/Down; Enter jumps to bottom"),
            Line::from("Page: Ctrl+f / Ctrl+b or PageDown/PageUp"),
            Line::from("Half-page: Ctrl+d / Ctrl+u"),
            Line::from("Search: /; Ctrl+n next; Ctrl+p/Ctrl+Shift+n previous"),
            Line::from("Toggle line wrapping: w"),
            Line::from("Manual restart selected service: r"),
            Line::from("Toggle controls: ?"),
            Line::from("Quit: q"),
            Line::from(""),
            Line::from("Press ? or Esc to close this panel"),
        ];
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .title("Controls")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                )
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(Color::Black).fg(Color::White)),
            area,
        );
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[derive(Clone)]
struct StyledSymbol {
    symbol: String,
    style: Style,
    width: usize,
}

fn is_wrappable_whitespace(symbol: &str) -> bool {
    symbol == "\u{200b}" || symbol.chars().all(char::is_whitespace) && symbol != "\u{00a0}"
}

fn line_to_symbols(line: &Line<'static>) -> Vec<StyledSymbol> {
    line.spans
        .iter()
        .flat_map(|span| {
            span.content
                .graphemes(true)
                .filter(|symbol| *symbol != "\n")
                .map(|symbol| StyledSymbol {
                    symbol: symbol.to_string(),
                    style: line.style.patch(span.style),
                    width: symbol.width(),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn symbols_to_line(symbols: &[StyledSymbol]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for symbol in symbols {
        if let Some(last) = spans.last_mut() {
            if last.style == symbol.style {
                last.content.to_mut().push_str(&symbol.symbol);
                continue;
            }
        }
        spans.push(Span::styled(symbol.symbol.clone(), symbol.style));
    }
    Line::from(spans)
}

fn push_wrapped_row(
    row: Vec<StyledSymbol>,
    row_index: &mut usize,
    skip: usize,
    take: usize,
    output: &mut Vec<Line<'static>>,
) -> bool {
    if *row_index >= skip && output.len() < take {
        output.push(symbols_to_line(&row));
    }
    *row_index += 1;
    output.len() >= take
}

fn wrapped_line_rows(
    line: &Line<'static>,
    width: usize,
    skip: usize,
    take: usize,
) -> Vec<Line<'static>> {
    if width == 0 || take == 0 {
        return Vec::new();
    }
    let mut output = Vec::new();
    let mut row_index = 0;
    let mut pending_line = Vec::new();
    let mut line_width = 0;
    let mut pending_word = Vec::new();
    let mut word_width = 0;
    let mut pending_whitespace = VecDeque::new();
    let mut whitespace_width = 0;
    let mut previous_was_non_whitespace = false;

    for symbol in line_to_symbols(line) {
        let whitespace = is_wrappable_whitespace(&symbol.symbol);
        let symbol_width = symbol.width;
        if symbol_width > width {
            continue;
        }
        let word_complete = previous_was_non_whitespace && whitespace;
        let initial_overflow =
            pending_line.is_empty() && word_width + whitespace_width + symbol_width > width;
        if word_complete || initial_overflow {
            pending_line.extend(pending_whitespace.drain(..));
            line_width += whitespace_width;
            whitespace_width = 0;
            pending_line.append(&mut pending_word);
            line_width += word_width;
            word_width = 0;
        }
        let overflow = symbol_width > 0 && line_width + whitespace_width + word_width >= width;
        if line_width >= width || overflow {
            let mut remaining = width.saturating_sub(line_width);
            if push_wrapped_row(
                std::mem::take(&mut pending_line),
                &mut row_index,
                skip,
                take,
                &mut output,
            ) {
                return output;
            }
            line_width = 0;
            while let Some(next_width) = pending_whitespace.front().map(|next| next.width) {
                if next_width > remaining {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(next_width);
                remaining = remaining.saturating_sub(next_width);
                pending_whitespace.pop_front();
            }
            if whitespace && pending_whitespace.is_empty() {
                previous_was_non_whitespace = false;
                continue;
            }
        }
        if whitespace {
            whitespace_width += symbol_width;
            pending_whitespace.push_back(symbol);
        } else {
            word_width += symbol_width;
            pending_word.push(symbol);
        }
        previous_was_non_whitespace = !whitespace;
    }

    if pending_line.is_empty()
        && pending_word.is_empty()
        && !pending_whitespace.is_empty()
        && push_wrapped_row(Vec::new(), &mut row_index, skip, take, &mut output)
    {
        return output;
    }
    pending_line.extend(pending_whitespace.drain(..));
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty()
        && push_wrapped_row(pending_line, &mut row_index, skip, take, &mut output)
    {
        return output;
    }
    if row_index == 0 {
        let _ = push_wrapped_row(Vec::new(), &mut row_index, skip, take, &mut output);
    }
    output
}

fn render_prewrapped_rows(frame: &mut ratatui::Frame<'_>, area: Rect, rows: &[Line<'static>]) {
    if area.is_empty() {
        return;
    }
    let buffer = frame.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            buffer[(x, y)].set_symbol(" ").set_style(Style::default());
        }
    }
    for (row_index, row) in rows.iter().take(area.height as usize).enumerate() {
        let y = area.y + row_index as u16;
        let mut x = 0;
        'spans: for span in &row.spans {
            let style = row.style.patch(span.style);
            for symbol in span
                .content
                .graphemes(true)
                .filter(|symbol| *symbol != "\n")
            {
                let width = symbol.width();
                if width == 0 {
                    continue;
                }
                if x >= area.width {
                    break 'spans;
                }
                buffer[(area.x + x, y)]
                    .set_symbol(if symbol.is_empty() { " " } else { symbol })
                    .set_style(style);
                x = x.saturating_add(width as u16);
            }
        }
    }
}

fn highlight_line(
    text: &str,
    base_style: Style,
    query: Option<&str>,
    current_match: bool,
) -> Line<'static> {
    let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) else {
        return Line::from(Span::styled(text.to_string(), base_style));
    };
    let needle = query.to_ascii_lowercase();
    let haystack = text.to_ascii_lowercase();
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut highlighted_current = false;
    while let Some(found) = haystack[cursor..].find(&needle) {
        let start = cursor + found;
        let end = start + needle.len();
        if start > cursor {
            spans.push(Span::styled(text[cursor..start].to_string(), base_style));
        }
        let style = if current_match && !highlighted_current {
            highlighted_current = true;
            base_style
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            base_style
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(text[start..end].to_string(), style));
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), base_style));
    }
    if spans.is_empty() {
        Line::from(Span::styled(text.to_string(), base_style))
    } else {
        Line::from(spans)
    }
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) = ("pbcopy", &[]);
    #[cfg(target_os = "windows")]
    let (program, args): (&str, &[&str]) = ("clip", &[]);
    #[cfg(target_os = "linux")]
    {
        for (program, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
        ] {
            if copy_with(program, args, text).is_ok() {
                return Ok(());
            }
        }
        return Err(anyhow!("clipboard helper not found (tried wl-copy, xclip)"));
    }
    #[cfg(not(target_os = "linux"))]
    copy_with(program, args, text)
}

#[cfg(not(target_os = "linux"))]
fn copy_with(program: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;
    child
        .stdin
        .as_mut()
        .context("clipboard stdin unavailable")?
        .write_all(text.as_bytes())
        .with_context(|| format!("failed writing to {program}"))?;
    let status = child
        .wait()
        .with_context(|| format!("failed waiting for {program}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("{program} failed with status {status}"))
    }
}

#[cfg(target_os = "linux")]
fn copy_with(program: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;
    child
        .stdin
        .as_mut()
        .context("clipboard stdin unavailable")?
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("{program} failed with status {status}"))
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
        if !kind.is_ascii() {
            continue;
        }
        index += 1;
        match kind {
            b'[' => {
                while let Some(&byte) = bytes.get(index) {
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
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
            b'P' | b'X' | b'^' | b'_' => {
                while let Some(&byte) = bytes.get(index) {
                    index += 1;
                    if byte == 0x1b && bytes.get(index) == Some(&b'\\') {
                        index += 1;
                        break;
                    }
                }
            }
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
    ToggleMouse(bool),
    Quit,
}

pub struct TerminalGuard {
    pub terminal: Terminal<CrosstermBackend<io::Stdout>>,
    mouse_capture: bool,
}

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        Ok(Self {
            terminal,
            mouse_capture: true,
        })
    }

    pub fn set_mouse_capture(&mut self, enabled: bool) -> Result<()> {
        if self.mouse_capture == enabled {
            return Ok(());
        }
        if enabled {
            execute!(io::stdout(), EnableMouseCapture)?;
        } else {
            execute!(io::stdout(), DisableMouseCapture)?;
        }
        self.mouse_capture = enabled;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        if self.mouse_capture {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn services(entries: &[(&str, u16)]) -> Dashboard {
        Dashboard::from_specs(
            entries
                .iter()
                .map(|(name, port)| {
                    (
                        (*name).to_string(),
                        Some(*port),
                        Some(format!("http://localhost:{port}")),
                    )
                })
                .collect(),
        )
    }

    fn terminal_text(
        terminal: &Terminal<ratatui::backend::TestBackend>,
        width: u16,
        height: u16,
    ) -> String {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_scalable_colored_service_tabs_and_focused_logs() {
        let mut dashboard = services(&[
            ("platform", 4000),
            ("developer-portal", 3300),
            ("user-portal", 3400),
        ]);
        for (name, line) in [
            ("platform", "PLATFORM_READY"),
            ("developer-portal", "DEVELOPER_READY"),
            ("user-portal", "USER_READY"),
        ] {
            dashboard.set_state(name, ServiceState::Running);
            dashboard.push_system(name, line);
        }
        let width = 100;
        let height = 18;
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| dashboard.render(frame, "/repo · branch", None))
            .unwrap();
        let first = terminal_text(&terminal, width, height);
        assert!(first.contains("platform"));
        assert!(first.contains("developer-portal"));
        assert!(first.contains("user-portal"));
        assert!(first.contains("localhost:4000"));
        assert!(first.contains("PLATFORM_READY"));
        assert!(!first.contains("DEVELOPER_READY"));
        assert_eq!(terminal.backend().buffer()[(1, 3)].fg, Color::Cyan);

        let area = Rect::new(0, 0, width, height);
        assert!(matches!(
            dashboard.handle_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 2,
                    row: 7,
                    modifiers: KeyModifiers::NONE,
                },
                area,
                None,
            ),
            DashboardAction::Draw
        ));
        assert!(matches!(
            dashboard.handle_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 38,
                    row: 6,
                    modifiers: KeyModifiers::NONE,
                },
                area,
                None,
            ),
            DashboardAction::Draw
        ));
        dashboard.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 46,
                row: 6,
                modifiers: KeyModifiers::NONE,
            },
            area,
            None,
        );
        dashboard.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 46,
                row: 6,
                modifiers: KeyModifiers::NONE,
            },
            area,
            None,
        );
        terminal
            .draw(|frame| dashboard.render(frame, "/repo · branch", None))
            .unwrap();
        let second = terminal_text(&terminal, width, height);
        assert_eq!(dashboard.active, 1);
        assert_eq!(dashboard.service_list_width, 46);
        assert!(second.contains("DEVELOPER_READY"));
        assert!(!second.contains("PLATFORM_READY"));
    }

    #[test]
    fn service_list_scroll_and_divider_resize_keep_large_stacks_navigable() {
        let specs = (0..8)
            .map(|index| {
                (
                    format!("service-{index}"),
                    Some(4100 + index),
                    Some(format!("http://localhost:{}", 4100 + index)),
                )
            })
            .collect::<Vec<_>>();
        let mut dashboard = Dashboard::from_specs(specs);
        dashboard.set_active(7);
        let width = 80;
        let height = 12;
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| dashboard.render(frame, "/repo", None))
            .unwrap();
        assert!(dashboard.service_list_scroll > 0);
        let original = dashboard.service_list_width;
        assert!(matches!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
            DashboardAction::Draw
        ));
        assert!(dashboard.service_list_width > original);
    }

    #[test]
    fn search_navigation_selects_matching_log_lines() {
        let mut dashboard = services(&[("platform", 4000)]);
        dashboard.push_system("platform", "first needle");
        dashboard.push_system("platform", "other");
        dashboard.push_system("platform", "second needle");
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "needle".chars() {
            dashboard.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(dashboard.search_hits, vec![0, 2]);
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(dashboard.search_cursor, 1);
    }

    #[test]
    fn wrapping_uses_visual_rows_and_preserves_unicode_graphemes() {
        let line = Line::from(vec![
            Span::styled("aaaaa ", Style::default().fg(Color::Cyan)),
            Span::styled("bbbbb 🦀 ccccc", Style::default().fg(Color::Yellow)),
        ]);
        let rows = wrapped_line_rows(&line, 10, 0, usize::MAX);
        assert_eq!(rows.len(), 3);
        let text = rows
            .iter()
            .flat_map(|row| row.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains('🦀'));
        assert!(rows
            .iter()
            .flat_map(|row| &row.spans)
            .any(|span| span.style.fg == Some(Color::Yellow)));
    }

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
