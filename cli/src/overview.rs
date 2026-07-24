//! The session overview surface.

use std::{
    collections::{HashMap, HashSet},
    io::{self},
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use turtletap::{
    Frame, KeyCode, KeyModifiers, MouseEventKind, Rect, ShellBindings, Shortcut, Surface,
    SurfaceAction, SurfaceEvent, SurfaceStatus, Theme,
    resident::{
        AttachmentMode, ClientRequest, ControlResult, ServerMessage, SessionId, SessionSelector,
        SessionSummary,
    },
    tui::{
        layout::{Constraint, Direction, Layout},
        style::{Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Paragraph},
    },
};

use crate::async_client::{self, ClientEvent, ConnectionState, SessionHandle};
use crate::command::{binding_labels, matches_binding};
use crate::keybindings::KeybindingEditor;
use crate::remote::{OpenSessions, RemoteSurface, session_surface_key};

#[derive(Clone, Copy)]
pub(crate) enum DashboardMode {
    Browse,
    Search,
    Create,
    Rename(SessionId),
    ConfirmTake(SessionId),
    ConfirmDelete(SessionId),
    ConfirmStopLeader,
}

pub(crate) struct SessionDashboard {
    pub(crate) path: PathBuf,
    client: SessionHandle,
    pending: HashMap<u64, DashboardOperation>,
    opening: HashSet<SessionId>,
    opened_tx: tokio::sync::mpsc::Sender<OpenResult>,
    opened_rx: tokio::sync::mpsc::Receiver<OpenResult>,
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) selected: usize,
    pub(crate) query: String,
    query_cursor: usize,
    grid_columns: usize,
    tile_hits: Vec<(Rect, usize)>,
    pub(crate) mode: DashboardMode,
    pub(crate) notice: Option<String>,
    pub(crate) refresh_elapsed: Duration,
    pub(crate) theme: Theme,
    pub(crate) bindings: ShellBindings,
    pub(crate) open_sessions: OpenSessions,
    no_color: bool,
    config_stamp: Option<(PathBuf, std::time::SystemTime)>,
}

#[derive(Clone, Debug)]
enum DashboardOperation {
    Refresh,
    Create(String),
    Rename(String),
    Delete,
    StopLeader,
}

type OpenResult = (SessionId, Result<RemoteSurface, String>);

/// The application-defined digest a `CommandSurface` publishes, mirrored here so
/// the overview can render previews without attaching to each session.
#[derive(Debug, Default, serde::Deserialize)]
struct SessionDigest {
    #[serde(default)]
    preview: Vec<String>,
    #[serde(default)]
    status: DigestStatus,
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum DigestStatus {
    #[default]
    Ready,
    Working,
    Attention,
    Failed,
    Complete,
}

impl DigestStatus {
    const fn surface_status(self) -> SurfaceStatus {
        match self {
            Self::Ready => SurfaceStatus::Ready,
            Self::Working => SurfaceStatus::Working,
            Self::Attention => SurfaceStatus::Attention,
            Self::Failed => SurfaceStatus::Failed,
            Self::Complete => SurfaceStatus::Complete,
        }
    }
}

impl SessionDashboard {
    pub(crate) fn connect_async(
        path: &Path,
        theme: Theme,
        bindings: ShellBindings,
        open_sessions: OpenSessions,
        sessions: Vec<SessionSummary>,
        client: SessionHandle,
        no_color: bool,
    ) -> Self {
        let (opened_tx, opened_rx) = tokio::sync::mpsc::channel(16);
        Self {
            path: path.to_owned(),
            client,
            pending: HashMap::new(),
            opening: HashSet::new(),
            opened_tx,
            opened_rx,
            sessions,
            selected: 0,
            query: String::new(),
            query_cursor: 0,
            grid_columns: 1,
            tile_hits: Vec::new(),
            mode: DashboardMode::Browse,
            notice: None,
            refresh_elapsed: Duration::ZERO,
            theme,
            bindings,
            open_sessions,
            no_color,
            config_stamp: config_stamp(),
        }
    }

    pub(crate) fn refresh(&mut self) -> SurfaceAction {
        self.refresh_elapsed = Duration::ZERO;
        self.request(ClientRequest::ListSessions, DashboardOperation::Refresh)
    }

    fn replace_sessions(&mut self, sessions: Vec<SessionSummary>) -> SurfaceAction {
        let previous_selected = self.selected;
        let mut changed = self.sessions != sessions;
        self.sessions = sessions;
        self.selected = previous_selected.min(self.filtered_indices().len().saturating_sub(1));
        changed |= self.selected != previous_selected;
        if self
            .notice
            .as_deref()
            .is_some_and(|notice| notice.starts_with("Disconnected · retrying:"))
        {
            self.notice = Some("Reconnected to the resident.".to_owned());
            changed = true;
        }
        if changed {
            SurfaceAction::Consumed
        } else {
            SurfaceAction::Ignored
        }
    }

    fn request(&mut self, request: ClientRequest, operation: DashboardOperation) -> SurfaceAction {
        match self.client.try_request(request) {
            Ok(id) => {
                self.pending.insert(id, operation);
                SurfaceAction::Consumed
            }
            Err(error) => self.fail(error),
        }
    }

    pub(crate) fn filtered_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                (query.is_empty() || session.name.to_lowercase().contains(&query)).then_some(index)
            })
            .collect()
    }

    pub(crate) fn selected_session(&self) -> Option<SessionSummary> {
        let indices = self.filtered_indices();
        indices
            .get(self.selected)
            .and_then(|index| self.sessions.get(*index))
            .cloned()
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let length = self.filtered_indices().len();
        if length == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(length - 1)
        };
    }

    pub(crate) fn open_selected(&mut self, mode: AttachmentMode, force: bool) -> SurfaceAction {
        let Some(session) = self.selected_session() else {
            self.notice = Some("No matching session to open.".to_owned());
            return SurfaceAction::Consumed;
        };
        let already_open = self
            .open_sessions
            .lock()
            .ok()
            .map(|sessions| sessions.contains(&session.id));
        match already_open {
            Some(true) => {
                return SurfaceAction::focus_key(session_surface_key(session.id));
            }
            Some(false) => {}
            None => return self.fail("open-session registry is poisoned"),
        }
        self.open_session(session, mode, force)
    }

    fn open_session(
        &mut self,
        session: SessionSummary,
        mode: AttachmentMode,
        force: bool,
    ) -> SurfaceAction {
        if !self.opening.insert(session.id) {
            return SurfaceAction::Ignored;
        }
        let path = self.path.clone();
        let open_sessions = self.open_sessions.clone();
        let bindings = self.bindings.clone();
        let opened = self.opened_tx.clone();
        tokio::spawn(async move {
            let id = session.id;
            let result = async {
                let (summary, instance, lease, snapshot, handle) =
                    async_client::connect_and_attach(&path, SessionSelector::Id(id), mode, force)
                        .await?;
                RemoteSurface::attach_async(
                    summary,
                    instance,
                    lease,
                    snapshot,
                    handle,
                    mode,
                    crate::remote::RemoteUi {
                        bindings,
                        open_sessions,
                    },
                )
            }
            .await
            .map_err(|error: io::Error| error.to_string());
            let _ = opened.send((id, result)).await;
        });
        self.notice = Some(format!("Opening '{}'…", session.name));
        SurfaceAction::Consumed
    }

    pub(crate) fn submit_text(&mut self) -> SurfaceAction {
        let value = self.query.trim().to_owned();
        if value.is_empty() {
            self.notice = Some("A session name cannot be empty.".to_owned());
            return SurfaceAction::Consumed;
        }
        let mode = std::mem::replace(&mut self.mode, DashboardMode::Browse);
        if matches!(mode, DashboardMode::Create) {
            return self.request(
                ClientRequest::CreateSession {
                    name: value.clone(),
                },
                DashboardOperation::Create(value),
            );
        }
        match mode {
            DashboardMode::Rename(session) => self.request(
                ClientRequest::RenameSession {
                    session,
                    name: value.clone(),
                },
                DashboardOperation::Rename(value),
            ),
            _ => SurfaceAction::Ignored,
        }
    }

    pub(crate) fn confirm_delete(&mut self, session: SessionId) -> SurfaceAction {
        self.request(
            ClientRequest::StopSession { session },
            DashboardOperation::Delete,
        )
    }

    pub(crate) fn fail(&mut self, error: impl std::fmt::Display) -> SurfaceAction {
        let message = error.to_string();
        if self.notice.as_deref() == Some(&message) {
            SurfaceAction::Ignored
        } else {
            self.notice = Some(message);
            SurfaceAction::Consumed
        }
    }

    fn complete_operation(
        &mut self,
        id: u64,
        result: Result<ControlResult, String>,
    ) -> SurfaceAction {
        let Some(operation) = self.pending.remove(&id) else {
            return SurfaceAction::Ignored;
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => return self.fail(error),
        };
        match (operation, result) {
            (DashboardOperation::Refresh, ControlResult::Sessions { sessions }) => {
                self.replace_sessions(sessions)
            }
            (DashboardOperation::Create(name), ControlResult::Created { session }) => {
                self.query.clear();
                self.query_cursor = 0;
                self.notice = Some(format!("Created session '{name}'."));
                let _ = self.refresh();
                self.open_session(session, AttachmentMode::Drive, false)
            }
            (DashboardOperation::Rename(name), ControlResult::Renamed { .. }) => {
                self.query.clear();
                self.query_cursor = 0;
                self.notice = Some(format!("Saved session '{name}'."));
                self.refresh()
            }
            (DashboardOperation::Delete, ControlResult::Stopping) => {
                self.mode = DashboardMode::Browse;
                self.notice = Some("Session stopped and its durable state deleted.".to_owned());
                self.refresh()
            }
            (DashboardOperation::StopLeader, ControlResult::Stopping) => SurfaceAction::Detach,
            _ => self.fail("resident returned an unexpected dashboard response"),
        }
    }

    fn apply_client_event(&mut self, event: ClientEvent) -> SurfaceAction {
        match event {
            ClientEvent::Completed { id, result } => self.complete_operation(id, result),
            ClientEvent::Connection(ConnectionState::Reconnecting { attempt }) => {
                self.notice = Some(format!("Disconnected · reconnecting ({attempt})…"));
                SurfaceAction::Consumed
            }
            ClientEvent::Connection(ConnectionState::Reconnected) => {
                self.notice = Some("Reconnected to the resident.".to_owned());
                let _ = self.refresh();
                SurfaceAction::Consumed
            }
            ClientEvent::Connection(ConnectionState::Failed(error)) => self.fail(error),
            ClientEvent::Message(ServerMessage::ShuttingDown { reason }) => {
                self.notice = Some(format!("Leader {reason:?} · reconnecting…"));
                SurfaceAction::Consumed
            }
            ClientEvent::Message(ServerMessage::Shutdown { reason })
                if reason != turtletap::resident::ShutdownReason::Upgrade =>
            {
                SurfaceAction::Detach
            }
            ClientEvent::Message(_) => SurfaceAction::Ignored,
        }
    }

    pub(crate) fn render_input<'a>(&'a self) -> Option<Line<'a>> {
        let label = match self.mode {
            DashboardMode::Search => "filter / ",
            DashboardMode::Create => "new session / ",
            DashboardMode::Rename(_) => "rename / ",
            _ => return None,
        };
        let before = &self.query[..self.query_cursor];
        let after = &self.query[self.query_cursor..];
        Some(Line::from(vec![
            Span::styled(label, self.theme.accent),
            Span::raw(before),
            Span::styled("_", Style::default().add_modifier(Modifier::REVERSED)),
            Span::raw(after),
        ]))
    }

    fn dashboard_hint(&self) -> String {
        format!(
            "Enter open · {} search · {} new · {} keys · ? help",
            binding_labels(&self.bindings.dashboard_search),
            binding_labels(&self.bindings.dashboard_new),
            binding_labels(&self.bindings.dashboard_keybindings),
        )
    }

    fn open_keybindings(&mut self) -> SurfaceAction {
        match crate::settings::shell_config("TurtleTap") {
            Ok(mut config) => {
                if self.no_color {
                    config.theme = config.theme.without_color();
                }
                self.notice = None;
                SurfaceAction::open(KeybindingEditor::new(config, self.no_color))
            }
            Err(error) => {
                self.notice = Some(format!("Could not open keybindings: {error}"));
                SurfaceAction::Consumed
            }
        }
    }
}

impl Surface for SessionDashboard {
    fn title(&self) -> std::borrow::Cow<'_, str> {
        "sessions".into()
    }

    fn reconfigure(&mut self, config: &turtletap::ShellConfig) {
        self.theme.clone_from(&config.theme);
        self.bindings.clone_from(&config.bindings);
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let theme = &self.theme;
        let now = unix_millis();
        let driven = self
            .sessions
            .iter()
            .filter(|session| session.driver.is_some())
            .count();
        let attached: usize = self
            .sessions
            .iter()
            .map(|session| session.attached_clients)
            .sum();
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(area);
        self.tile_hits.clear();
        let header = vec![
            Line::styled(
                "Resident sessions",
                theme.chrome.add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!(
                    "{} session{} · {driven} driven · {attached} attached",
                    self.sessions.len(),
                    if self.sessions.len() == 1 { "" } else { "s" }
                ),
                theme.muted,
            ),
            Line::styled(self.dashboard_hint(), theme.muted),
        ];
        frame.render_widget(Paragraph::new(header), sections[0]);

        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    if self.sessions.is_empty() {
                        format!(
                            "No sessions yet. Press {} to create one.",
                            binding_labels(&self.bindings.dashboard_new)
                        )
                    } else {
                        "No sessions match this filter.".to_owned()
                    },
                    theme.attention,
                )),
                sections[1],
            );
        } else {
            const TILE_HEIGHT: u16 = 6;
            let grid = sections[1];
            let columns = if grid.width >= 72 { 2_usize } else { 1 };
            self.grid_columns = columns;
            let visible_rows = usize::from(grid.height / TILE_HEIGHT).max(1);
            let selected_row = self.selected / columns;
            let first_row = selected_row.saturating_add(1).saturating_sub(visible_rows);
            let first = first_row.saturating_mul(columns);
            let capacity = visible_rows.saturating_mul(columns);
            let column_width = grid.width / u16::try_from(columns).unwrap_or(1);

            for (slot, index) in filtered.iter().skip(first).take(capacity).enumerate() {
                let row = slot / columns;
                let column = slot % columns;
                let x = grid.x.saturating_add(
                    u16::try_from(column)
                        .unwrap_or(u16::MAX)
                        .saturating_mul(column_width),
                );
                let width = if column + 1 == columns {
                    grid.right().saturating_sub(x)
                } else {
                    column_width
                };
                let tile = Rect::new(
                    x,
                    grid.y.saturating_add(
                        u16::try_from(row)
                            .unwrap_or(u16::MAX)
                            .saturating_mul(TILE_HEIGHT),
                    ),
                    width,
                    TILE_HEIGHT.min(grid.height),
                );
                let session = &self.sessions[*index];
                let visible = first.saturating_add(slot);
                self.tile_hits.push((tile, visible));
                let selected = visible == self.selected;
                let digest = session
                    .digest
                    .as_ref()
                    .and_then(|value| serde_json::from_value::<SessionDigest>(value.clone()).ok())
                    .unwrap_or_default();
                let status = digest.status.surface_status();
                let title_style = if selected {
                    theme.selected.add_modifier(Modifier::BOLD)
                } else {
                    status_style(theme, status).add_modifier(Modifier::BOLD)
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(if selected {
                        theme.selected
                    } else {
                        theme.muted
                    })
                    .title(Line::from(vec![
                        Span::styled(
                            format!(" {} ", status.marker()),
                            status_style(theme, status),
                        ),
                        Span::styled(session.name.as_str(), title_style),
                        Span::raw(" "),
                    ]));
                let inner = block.inner(tile);
                frame.render_widget(block, tile);
                let role = if session.driver.is_some() {
                    "driven"
                } else {
                    "idle"
                };
                let mut lines = vec![Line::styled(
                    format!(
                        "{role} · {} viewer{} · {}",
                        session.attached_clients,
                        if session.attached_clients == 1 {
                            ""
                        } else {
                            "s"
                        },
                        relative_time(now, session.last_event_at),
                    ),
                    theme.muted,
                )];
                lines.extend(
                    digest
                        .preview
                        .iter()
                        .take(3)
                        .map(|preview| Line::styled(preview.as_str(), theme.chrome)),
                );
                frame.render_widget(Paragraph::new(lines), inner);
            }
        }

        let footer = match self.mode {
            DashboardMode::ConfirmTake(_)
            | DashboardMode::ConfirmDelete(_)
            | DashboardMode::ConfirmStopLeader => Line::styled(
                if matches!(self.mode, DashboardMode::ConfirmTake(_)) {
                    "Replace the current driver? The other terminal becomes view-only. [y/N]"
                } else if matches!(self.mode, DashboardMode::ConfirmStopLeader) {
                    "Stop the resident leader? Sessions remain durable. [y/N]"
                } else {
                    "Delete this session and its durable state? [y/N]"
                },
                theme.attention.add_modifier(Modifier::BOLD),
            ),
            _ => self.render_input().unwrap_or_else(|| {
                self.notice.as_ref().map_or_else(
                    || Line::raw(""),
                    |notice| Line::styled(notice.as_str(), theme.attention),
                )
            }),
        };
        frame.render_widget(Paragraph::new(footer), sections[2]);
    }

    fn poll_background(&mut self, context: &mut Context<'_>) -> Poll<SurfaceAction> {
        match Pin::new(&mut self.opened_rx).poll_recv(context) {
            Poll::Ready(Some((session, result))) => {
                self.opening.remove(&session);
                return Poll::Ready(match result {
                    Ok(surface) => {
                        self.notice = None;
                        SurfaceAction::open(surface)
                    }
                    Err(error) => self.fail(error),
                });
            }
            Poll::Ready(None) | Poll::Pending => {}
        }
        match self.client.poll_event(context) {
            Poll::Ready(Some(event)) => Poll::Ready(self.apply_client_event(event)),
            Poll::Ready(None) => {
                Poll::Ready(self.fail("resident dashboard connection pump stopped"))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
        match event {
            SurfaceEvent::Tick(elapsed) => {
                self.refresh_elapsed += elapsed;
                let stamp = config_stamp();
                if stamp != self.config_stamp {
                    self.config_stamp = stamp;
                    match crate::settings::shell_config("TurtleTap") {
                        Ok(mut config) => {
                            if self.no_color {
                                config.theme = config.theme.without_color();
                            }
                            self.notice = Some("Configuration reloaded.".to_owned());
                            return SurfaceAction::Reconfigure(Box::new(config));
                        }
                        Err(error) => {
                            self.notice = Some(format!(
                                "Configuration reload failed; keeping current settings: {error}"
                            ));
                            return SurfaceAction::Consumed;
                        }
                    }
                }
                if self.refresh_elapsed >= Duration::from_secs(2) {
                    return self.refresh();
                }
                SurfaceAction::Ignored
            }
            SurfaceEvent::Key(key) => match self.mode {
                DashboardMode::Browse => {
                    if key.code == KeyCode::Left {
                        self.move_selection(-1);
                        SurfaceAction::Consumed
                    } else if key.code == KeyCode::Right {
                        self.move_selection(1);
                        SurfaceAction::Consumed
                    } else if key.code == KeyCode::Up
                        || matches_binding(&self.bindings.dashboard_up, key)
                    {
                        self.move_selection(-(self.grid_columns as isize));
                        SurfaceAction::Consumed
                    } else if key.code == KeyCode::Down
                        || matches_binding(&self.bindings.dashboard_down, key)
                    {
                        self.move_selection(self.grid_columns as isize);
                        SurfaceAction::Consumed
                    } else if key.code == KeyCode::Enter {
                        let mode = if self
                            .selected_session()
                            .is_some_and(|session| session.driver.is_some())
                        {
                            AttachmentMode::View
                        } else {
                            AttachmentMode::Drive
                        };
                        self.open_selected(mode, false)
                    } else if matches_binding(&self.bindings.dashboard_view, key) {
                        self.open_selected(AttachmentMode::View, false)
                    } else if matches_binding(&self.bindings.dashboard_take, key) {
                        if let Some(session) = self.selected_session() {
                            if session.driver.is_some() {
                                self.mode = DashboardMode::ConfirmTake(session.id);
                                SurfaceAction::Consumed
                            } else {
                                self.open_selected(AttachmentMode::Drive, false)
                            }
                        } else {
                            SurfaceAction::Ignored
                        }
                    } else if matches_binding(&self.bindings.dashboard_search, key) {
                        self.mode = DashboardMode::Search;
                        self.query.clear();
                        self.query_cursor = 0;
                        SurfaceAction::Consumed
                    } else if matches_binding(&self.bindings.dashboard_new, key) {
                        self.mode = DashboardMode::Create;
                        self.query.clear();
                        self.query_cursor = 0;
                        SurfaceAction::Consumed
                    } else if matches_binding(&self.bindings.dashboard_rename, key) {
                        if let Some(session) = self.selected_session() {
                            self.query.clone_from(&session.name);
                            self.query_cursor = self.query.len();
                            self.mode = DashboardMode::Rename(session.id);
                        }
                        SurfaceAction::Consumed
                    } else if matches_binding(&self.bindings.dashboard_delete, key) {
                        if let Some(session) = self.selected_session() {
                            self.mode = DashboardMode::ConfirmDelete(session.id);
                        }
                        SurfaceAction::Consumed
                    } else if matches_binding(&self.bindings.dashboard_stop, key) {
                        self.mode = DashboardMode::ConfirmStopLeader;
                        SurfaceAction::Consumed
                    } else if matches_binding(&self.bindings.dashboard_keybindings, key) {
                        self.open_keybindings()
                    } else if matches_binding(&self.bindings.dashboard_close, key) {
                        SurfaceAction::Close
                    } else {
                        SurfaceAction::Ignored
                    }
                }
                DashboardMode::Search | DashboardMode::Create | DashboardMode::Rename(_) => {
                    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Left {
                        self.query_cursor = previous_word_boundary(&self.query, self.query_cursor);
                        return SurfaceAction::Consumed;
                    }
                    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Right {
                        self.query_cursor = next_word_boundary(&self.query, self.query_cursor);
                        return SurfaceAction::Consumed;
                    }
                    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Backspace {
                        let previous = previous_word_boundary(&self.query, self.query_cursor);
                        self.query.drain(previous..self.query_cursor);
                        self.query_cursor = previous;
                        self.selected = 0;
                        return SurfaceAction::Consumed;
                    }
                    match key.code {
                        KeyCode::Esc => {
                            self.mode = DashboardMode::Browse;
                            self.query.clear();
                            self.query_cursor = 0;
                            SurfaceAction::Consumed
                        }
                        KeyCode::Enter if matches!(self.mode, DashboardMode::Search) => {
                            self.mode = DashboardMode::Browse;
                            SurfaceAction::Consumed
                        }
                        KeyCode::Enter => self.submit_text(),
                        KeyCode::Backspace => {
                            if self.query_cursor > 0 {
                                let previous = previous_boundary(&self.query, self.query_cursor);
                                self.query.drain(previous..self.query_cursor);
                                self.query_cursor = previous;
                            }
                            self.selected = 0;
                            SurfaceAction::Consumed
                        }
                        KeyCode::Delete => {
                            if self.query_cursor < self.query.len() {
                                let next = next_boundary(&self.query, self.query_cursor);
                                self.query.drain(self.query_cursor..next);
                            }
                            self.selected = 0;
                            SurfaceAction::Consumed
                        }
                        KeyCode::Left => {
                            self.query_cursor = previous_boundary(&self.query, self.query_cursor);
                            SurfaceAction::Consumed
                        }
                        KeyCode::Right => {
                            self.query_cursor = next_boundary(&self.query, self.query_cursor);
                            SurfaceAction::Consumed
                        }
                        KeyCode::Home => {
                            self.query_cursor = 0;
                            SurfaceAction::Consumed
                        }
                        KeyCode::End => {
                            self.query_cursor = self.query.len();
                            SurfaceAction::Consumed
                        }
                        KeyCode::Char(character) => {
                            if !character.is_control() {
                                self.query.insert(self.query_cursor, character);
                                self.query_cursor += character.len_utf8();
                            }
                            self.selected = 0;
                            SurfaceAction::Consumed
                        }
                        _ => SurfaceAction::Ignored,
                    }
                }
                DashboardMode::ConfirmTake(_)
                | DashboardMode::ConfirmDelete(_)
                | DashboardMode::ConfirmStopLeader
                    if key.code == KeyCode::Enter =>
                {
                    self.mode = DashboardMode::Browse;
                    self.notice = Some("Action cancelled.".to_owned());
                    SurfaceAction::Consumed
                }
                DashboardMode::ConfirmTake(session) => match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.mode = DashboardMode::Browse;
                        if self
                            .selected_session()
                            .is_some_and(|selected| selected.id == session)
                        {
                            self.open_selected(AttachmentMode::Drive, true)
                        } else {
                            self.fail("the selected session changed; takeover was cancelled")
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.mode = DashboardMode::Browse;
                        self.notice = Some("Takeover cancelled.".to_owned());
                        SurfaceAction::Consumed
                    }
                    _ => SurfaceAction::Ignored,
                },
                DashboardMode::ConfirmDelete(session) => match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_delete(session),
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.mode = DashboardMode::Browse;
                        self.notice = Some("Delete cancelled.".to_owned());
                        SurfaceAction::Consumed
                    }
                    _ => SurfaceAction::Ignored,
                },
                DashboardMode::ConfirmStopLeader => match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.request(ClientRequest::StopLeader, DashboardOperation::StopLeader)
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.mode = DashboardMode::Browse;
                        self.notice = Some("Leader stop cancelled.".to_owned());
                        SurfaceAction::Consumed
                    }
                    _ => SurfaceAction::Ignored,
                },
            },
            SurfaceEvent::Paste(text)
                if matches!(
                    self.mode,
                    DashboardMode::Search | DashboardMode::Create | DashboardMode::Rename(_)
                ) =>
            {
                if text.contains(['\r', '\n']) || text.chars().any(char::is_control) {
                    self.notice =
                        Some("Session names and filters cannot contain line breaks.".to_owned());
                    return SurfaceAction::Consumed;
                }
                self.query.insert_str(self.query_cursor, &text);
                self.query_cursor += text.len();
                self.selected = 0;
                SurfaceAction::Consumed
            }
            SurfaceEvent::Mouse(mouse)
                if mouse.kind == MouseEventKind::Down(turtletap::MouseButton::Left) =>
            {
                let position = self
                    .tile_hits
                    .iter()
                    .find(|(area, _)| {
                        mouse.column >= area.x
                            && mouse.column < area.right()
                            && mouse.row >= area.y
                            && mouse.row < area.bottom()
                    })
                    .map(|(_, position)| *position);
                if let Some(position) = position {
                    self.selected = position;
                    SurfaceAction::Consumed
                } else {
                    SurfaceAction::Ignored
                }
            }
            SurfaceEvent::Paste(_)
            | SurfaceEvent::Mouse(_)
            | SurfaceEvent::Resize { .. }
            | SurfaceEvent::ScrollPageUp
            | SurfaceEvent::ScrollPageDown => SurfaceAction::Ignored,
        }
    }

    fn shortcuts(&self) -> Vec<Shortcut> {
        vec![
            Shortcut::new("Enter", "Open selected session"),
            Shortcut::new(
                binding_labels(&self.bindings.dashboard_search),
                "Search sessions",
            ),
            Shortcut::new(
                format!(
                    "{} / {} / {}",
                    binding_labels(&self.bindings.dashboard_new),
                    binding_labels(&self.bindings.dashboard_rename),
                    binding_labels(&self.bindings.dashboard_delete)
                ),
                "New, rename, or delete",
            ),
            Shortcut::new(
                format!(
                    "{} / {}",
                    binding_labels(&self.bindings.dashboard_view),
                    binding_labels(&self.bindings.dashboard_take)
                ),
                "View or take driver",
            ),
            Shortcut::new(
                binding_labels(&self.bindings.dashboard_stop),
                "Stop resident leader",
            ),
            Shortcut::new(
                binding_labels(&self.bindings.dashboard_keybindings),
                "Edit keybindings",
            ),
        ]
    }
}

fn config_stamp() -> Option<(PathBuf, std::time::SystemTime)> {
    let path = crate::settings::active_path().ok()?;
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    Some((path, modified))
}

fn previous_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(index, _)| cursor + index)
}

fn previous_word_boundary(value: &str, cursor: usize) -> usize {
    let characters: Vec<_> = value[..cursor].char_indices().collect();
    let mut index = characters.len();
    while index > 0 && characters[index - 1].1.is_whitespace() {
        index -= 1;
    }
    while index > 0 && !characters[index - 1].1.is_whitespace() {
        index -= 1;
    }
    characters.get(index).map_or(0, |(offset, _)| *offset)
}

fn next_word_boundary(value: &str, cursor: usize) -> usize {
    let mut offset = cursor;
    let mut seen_word = false;
    for character in value[cursor..].chars() {
        if seen_word && character.is_whitespace() {
            break;
        }
        seen_word |= !character.is_whitespace();
        offset += character.len_utf8();
    }
    while offset < value.len() {
        let character = value[offset..].chars().next().unwrap_or_default();
        if !character.is_whitespace() {
            break;
        }
        offset += character.len_utf8();
    }
    offset
}

/// Maps a surface status to its themed color.
fn status_style(theme: &Theme, status: SurfaceStatus) -> Style {
    match status {
        SurfaceStatus::Ready => theme.muted,
        SurfaceStatus::Working => theme.working,
        SurfaceStatus::Attention => theme.attention,
        SurfaceStatus::Failed => theme.failed,
        SurfaceStatus::Complete => theme.complete,
    }
}

/// A short "idle 3m" style label from a millisecond timestamp, or "new" when a
/// session has committed nothing yet.
fn relative_time(now: u64, last_event_at: Option<u64>) -> String {
    let Some(then) = last_event_at else {
        return "new".to_owned();
    };
    let seconds = now.saturating_sub(then) / 1000;
    if seconds < 5 {
        "active".to_owned()
    } else if seconds < 60 {
        format!("idle {seconds}s")
    } else if seconds < 3600 {
        format!("idle {}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("idle {}h", seconds / 3600)
    } else {
        format!("idle {}d", seconds / 86_400)
    }
}

/// Wall-clock milliseconds since the Unix epoch for relative-time display.
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
