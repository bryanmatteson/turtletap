//! A surface bound to one resident session.

use std::{
    collections::HashSet,
    io::{self},
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use turtletap::{
    Frame, InputPolicy, KeyCode, KeyModifiers, MouseEventKind, Rect, ShellBindings, Shortcut,
    Surface, SurfaceAction, SurfaceEvent, SurfaceStatus,
    resident::{
        AttachmentMode, ClientInstanceId, ClientRequest, ControlResult, DriverLease, LeaseEpoch,
        ServerMessage, SessionId, ShutdownReason,
    },
    tui::{
        layout::{Constraint, Direction, Layout, Position},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::Paragraph,
    },
};

use crate::app::{SessionSnapshot, ShellCommand, ShellEvent, transcript_append_count};
use crate::async_client::{ClientEvent, ConnectionState, SessionHandle};
use crate::command::{
    Scrollback, TranscriptEntry, TranscriptKind, binding_labels, char_slice_width,
    leader_binding_label, matches_binding, session_cursor_target, session_delete_start,
};
use crate::commands::is_detach_command;

pub(crate) type OpenSessions = Arc<Mutex<HashSet<SessionId>>>;

pub(crate) fn open_sessions() -> OpenSessions {
    Arc::new(Mutex::new(HashSet::new()))
}

pub(crate) fn session_surface_key(session: SessionId) -> String {
    format!("session:{session}")
}

pub(crate) struct RemoteUi {
    pub(crate) bindings: ShellBindings,
    pub(crate) open_sessions: OpenSessions,
}

#[derive(Default)]
pub(crate) struct ScreenActivity {
    pub(crate) focused: bool,
    pub(crate) unread_lines: usize,
}

impl ScreenActivity {
    pub(crate) fn appended(&mut self, lines: usize) {
        if !self.focused {
            self.unread_lines = self.unread_lines.saturating_add(lines);
        }
    }

    pub(crate) fn focus(&mut self) {
        self.focused = true;
        self.unread_lines = 0;
    }

    pub(crate) fn blur(&mut self) {
        self.focused = false;
    }
}

pub(crate) fn driver_role(
    lease: Option<DriverLease>,
    client: ClientInstanceId,
) -> (Option<LeaseEpoch>, AttachmentMode) {
    let owned = lease
        .filter(|lease| lease.owner == client)
        .map(|lease| lease.epoch);
    let mode = if owned.is_some() {
        AttachmentMode::Drive
    } else {
        AttachmentMode::View
    };
    (owned, mode)
}

pub(crate) struct RemoteSurface {
    client: SessionHandle,
    client_instance: ClientInstanceId,
    lease: Option<LeaseEpoch>,
    pub(crate) session_id: SessionId,
    pub(crate) name: String,
    pub(crate) mode: AttachmentMode,
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) input: Vec<char>,
    pub(crate) cursor: usize,
    pub(crate) history_cursor: Option<usize>,
    pub(crate) history_draft: String,
    pub(crate) connection_error: Option<String>,
    pub(crate) planned_shutdown: Option<ShutdownReason>,
    pub(crate) banner: Option<String>,
    pub(crate) scrollback: Scrollback,
    pub(crate) activity: ScreenActivity,
    pub(crate) metadata_elapsed: Duration,
    pub(crate) last_event_at: Option<u64>,
    pub(crate) open_sessions: OpenSessions,
    pub(crate) bindings: ShellBindings,
    multiline_confirmation: bool,
    take_confirmation: bool,
}

impl RemoteSurface {
    pub(crate) fn attach_async(
        session: turtletap::resident::SessionSummary,
        client_instance: ClientInstanceId,
        lease: Option<LeaseEpoch>,
        snapshot: SessionSnapshot,
        handle: SessionHandle,
        mode: AttachmentMode,
        ui: RemoteUi,
    ) -> io::Result<Self> {
        let inserted = ui
            .open_sessions
            .lock()
            .map_err(|_| io::Error::other("open-session registry is poisoned"))?
            .insert(session.id);
        if !inserted {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("session '{}' is already open", session.name),
            ));
        }
        Ok(Self {
            client: handle,
            client_instance,
            lease,
            session_id: session.id,
            name: session.name,
            mode,
            snapshot,
            input: Vec::new(),
            cursor: 0,
            history_cursor: None,
            history_draft: String::new(),
            connection_error: None,
            planned_shutdown: None,
            banner: None,
            scrollback: Scrollback::default(),
            activity: ScreenActivity::default(),
            metadata_elapsed: Duration::ZERO,
            last_event_at: session.last_event_at,
            open_sessions: ui.open_sessions,
            bindings: ui.bindings,
            multiline_confirmation: false,
            take_confirmation: false,
        })
    }

    fn enqueue(&mut self, request: ClientRequest) -> Result<(), io::Error> {
        let _ = self.client.try_request(request)?;
        Ok(())
    }

    pub(crate) fn refresh_metadata(&mut self) -> SurfaceAction {
        self.metadata_elapsed = Duration::ZERO;
        match self.enqueue(ClientRequest::ListSessions) {
            Ok(()) => SurfaceAction::Ignored,
            Err(error) => self.fail(error),
        }
    }

    pub(crate) fn apply_driver(&mut self, lease: Option<DriverLease>) -> bool {
        let (owned, mode) = driver_role(lease, self.client_instance);
        let changed = self.lease != owned || self.mode != mode;
        self.lease = owned;
        self.mode = mode;
        if changed {
            self.take_confirmation = false;
            self.banner = Some(if mode == AttachmentMode::Drive {
                "Driver acquired · input is live.".to_owned()
            } else {
                format!(
                    "Driver changed · viewing only. Press {} to take over.",
                    binding_labels(&self.bindings.session_take_driver)
                )
            });
        }
        changed
    }

    pub(crate) fn take_driver(&mut self) -> SurfaceAction {
        if self.mode == AttachmentMode::Drive {
            self.banner = Some("This terminal already has control.".to_owned());
            return SurfaceAction::Consumed;
        }
        self.take_confirmation = true;
        self.banner = Some("Take control? The other terminal will become view-only. [y/N]".into());
        SurfaceAction::Consumed
    }

    fn confirm_take_driver(&mut self) -> SurfaceAction {
        self.take_confirmation = false;
        match self.enqueue(ClientRequest::AcquireDriver {
            session: self.session_id,
            force: true,
        }) {
            Ok(()) => SurfaceAction::Consumed,
            Err(error) => self.fail(error),
        }
    }

    pub(crate) fn release_driver(&mut self) -> SurfaceAction {
        match self.enqueue(ClientRequest::ReleaseDriver {
            session: self.session_id,
        }) {
            Ok(()) => SurfaceAction::Consumed,
            Err(error) => self.fail(error),
        }
    }

    pub(crate) fn command(&mut self, command: ShellCommand) -> SurfaceAction {
        let lease = match self.lease {
            Some(lease) => lease,
            None => {
                return self.fail(
                    "this terminal is viewing the session; use 'turtletap take' to drive it",
                );
            }
        };
        let command = match serde_json::to_value(command) {
            Ok(command) => command,
            Err(error) => return self.fail(error),
        };
        match self.enqueue(ClientRequest::Command {
            session: self.session_id,
            lease,
            command,
        }) {
            Ok(()) => {
                self.connection_error = None;
                SurfaceAction::Consumed
            }
            Err(error) => self.fail(error),
        }
    }

    fn apply_messages(
        &mut self,
        messages: impl IntoIterator<Item = ServerMessage>,
    ) -> SurfaceAction {
        let mut changed = false;
        for message in messages {
            match message {
                ServerMessage::Snapshot {
                    session,
                    sequence: _,
                    state,
                } if self.session_id == session => {
                    match serde_json::from_value::<SessionSnapshot>(state) {
                        Ok(snapshot) => {
                            let appended = transcript_append_count(
                                &self.snapshot.transcript,
                                &snapshot.transcript,
                            );
                            self.snapshot = snapshot;
                            self.scrollback
                                .appended(appended, self.snapshot.transcript.len());
                            self.activity.appended(appended);
                            changed = true;
                        }
                        Err(error) => return self.fail(error),
                    }
                }
                ServerMessage::Event {
                    session,
                    sequence: _,
                    event,
                } if self.session_id == session => {
                    match serde_json::from_value::<ShellEvent>(event) {
                        Ok(event) => {
                            let appended = event.appended_count(&self.snapshot);
                            event.apply(&mut self.snapshot);
                            self.scrollback
                                .appended(appended, self.snapshot.transcript.len());
                            self.activity.appended(appended);
                            changed = true;
                        }
                        Err(error) => return self.fail(error),
                    }
                }
                ServerMessage::DriverChanged { session, lease } if self.session_id == session => {
                    changed |= self.apply_driver(lease);
                }
                ServerMessage::ResyncRequired { .. } => {
                    self.banner = Some("Session state is resynchronizing…".to_owned());
                    changed = true;
                }
                ServerMessage::ShuttingDown { reason } => {
                    self.planned_shutdown = Some(reason);
                    self.banner = Some(format!("Leader {reason:?} · reconnecting…"));
                    self.snapshot.transcript.push(TranscriptEntry {
                        kind: TranscriptKind::System,
                        text: format!("Resident is shutting down: {reason:?}"),
                    });
                    self.activity.appended(1);
                    changed = true;
                }
                ServerMessage::Shutdown { reason } => {
                    if reason == ShutdownReason::Upgrade {
                        self.planned_shutdown = None;
                        self.banner = Some("Connecting to the replacement leader…".to_owned());
                        changed = true;
                    } else {
                        return SurfaceAction::Detach;
                    }
                }
                ServerMessage::Response { .. } => {}
                ServerMessage::Snapshot { .. }
                | ServerMessage::Event { .. }
                | ServerMessage::DriverChanged { .. } => {}
            }
        }
        if changed {
            SurfaceAction::Consumed
        } else {
            SurfaceAction::Ignored
        }
    }

    fn apply_result(&mut self, result: ControlResult) -> SurfaceAction {
        match result {
            ControlResult::Accepted { .. } => {
                self.connection_error = None;
                SurfaceAction::Consumed
            }
            ControlResult::Driver { lease, .. } => {
                let _ = self.apply_driver(lease);
                SurfaceAction::Consumed
            }
            ControlResult::Sessions { sessions } => {
                let Some(summary) = sessions
                    .into_iter()
                    .find(|summary| summary.id == self.session_id)
                else {
                    return SurfaceAction::Close;
                };
                let mut changed = false;
                if self.name != summary.name {
                    let previous = std::mem::replace(&mut self.name, summary.name);
                    self.banner = Some(format!("Session renamed from '{previous}'."));
                    changed = true;
                }
                if self.last_event_at != summary.last_event_at {
                    self.last_event_at = summary.last_event_at;
                    changed = true;
                }
                changed |= self.apply_driver(summary.driver);
                if changed {
                    SurfaceAction::Consumed
                } else {
                    SurfaceAction::Ignored
                }
            }
            ControlResult::Detached { .. } => SurfaceAction::Detach,
            _ => self.fail("resident returned an unexpected response"),
        }
    }

    fn apply_client_event(&mut self, event: ClientEvent) -> SurfaceAction {
        match event {
            ClientEvent::Message(message) => self.apply_messages([message]),
            ClientEvent::Completed { id: _, result } => match result {
                Ok(result) => self.apply_result(result),
                Err(error) => self.fail(error),
            },
            ClientEvent::Connection(ConnectionState::Reconnecting { attempt }) => {
                self.banner = Some(format!("Disconnected · reconnecting ({attempt})…"));
                SurfaceAction::Consumed
            }
            ClientEvent::Connection(ConnectionState::Reconnected) => {
                self.connection_error = None;
                self.banner = Some("Reconnected · session state resumed.".to_owned());
                SurfaceAction::Consumed
            }
            ClientEvent::Connection(ConnectionState::Failed(error)) => self.fail(error),
        }
    }

    pub(crate) fn fail(&mut self, error: impl std::fmt::Display) -> SurfaceAction {
        let message = error.to_string();
        if self.connection_error.as_deref() == Some(&message) {
            return SurfaceAction::Ignored;
        }
        self.snapshot.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Error,
            text: message.clone(),
        });
        self.snapshot.last_failed = true;
        self.connection_error = Some(message);
        self.activity.appended(1);
        SurfaceAction::Consumed
    }

    pub(crate) fn submit(&mut self) -> SurfaceAction {
        if self.mode == AttachmentMode::View {
            self.banner = Some(format!(
                "View only · press {} to take control.",
                binding_labels(&self.bindings.session_take_driver)
            ));
            return SurfaceAction::Consumed;
        }
        let line: String = self.input.iter().collect();
        let line = line.trim().to_owned();
        if line.is_empty() {
            return SurfaceAction::Ignored;
        }
        if self.multiline_confirmation {
            self.multiline_confirmation = false;
            self.banner = Some(
                "Multiline command ready · press Enter again to run or Esc to cancel.".to_owned(),
            );
            return SurfaceAction::Consumed;
        }
        if is_detach_command(&line) {
            let _ = self.enqueue(ClientRequest::Detach {
                session: self.session_id,
            });
            return SurfaceAction::Detach;
        }
        self.clear_input();
        let (name, arguments) = crate::command::split_command(&line);
        if name == ":queue" {
            return self.command(ShellCommand::QueueStatus);
        }
        if name == ":cancel" {
            return self.command(ShellCommand::CancelQueued {
                target: arguments.to_owned(),
            });
        }
        self.command(ShellCommand::Submit { line })
    }

    pub(crate) fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_cursor = None;
        self.history_draft.clear();
        self.multiline_confirmation = false;
    }

    fn insert_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.contains('\n') {
            self.multiline_confirmation = true;
        }
        let inserted: Vec<_> = normalized.chars().collect();
        let count = inserted.len();
        self.input.splice(self.cursor..self.cursor, inserted);
        self.cursor += count;
    }

    pub(crate) fn history_previous(&mut self) {
        if self.snapshot.history.is_empty() {
            return;
        }
        let index = match self.history_cursor {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft = self.input.iter().collect();
                self.snapshot.history.len() - 1
            }
        };
        self.history_cursor = Some(index);
        self.input = self.snapshot.history[index].chars().collect();
        self.cursor = self.input.len();
    }

    pub(crate) fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.snapshot.history.len() {
            let next = index + 1;
            self.history_cursor = Some(next);
            self.input = self.snapshot.history[next].chars().collect();
        } else {
            self.history_cursor = None;
            self.input = self.history_draft.chars().collect();
            self.history_draft.clear();
        }
        self.cursor = self.input.len();
    }

    pub(crate) fn complete(&mut self) {
        if self.cursor != self.input.len() {
            return;
        }
        let input: String = self.input.iter().collect();
        if input.chars().any(char::is_whitespace) {
            return;
        }
        let mut candidates: Vec<_> = [
            ":add",
            ":commands",
            ":remove",
            ":cd",
            ":history",
            ":queue",
            ":cancel",
            ":clear",
            ":help",
            ":quit",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(self.snapshot.commands.iter().cloned())
        .filter(|candidate| candidate.starts_with(&input))
        .collect();
        candidates.sort();
        candidates.dedup();
        if let [candidate] = candidates.as_slice() {
            self.input = candidate.chars().collect();
            self.cursor = self.input.len();
        }
    }

    pub(crate) fn render_prompt(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let role = if self.lease.is_some() { "" } else { " [view]" };
        let label = format!("{}{} ❯ ", self.snapshot.cwd_label, role);
        let label_width = Line::from(label.as_str()).width();
        let available = usize::from(area.width).saturating_sub(label_width).max(1);
        let mut start = 0;
        while start < self.cursor && char_slice_width(&self.input[start..self.cursor]) >= available
        {
            start += 1;
        }
        let mut end = start;
        while end < self.input.len() && char_slice_width(&self.input[start..=end]) <= available {
            end += 1;
        }
        let visible: String = self.input[start..end]
            .iter()
            .map(|character| {
                if *character == '\n' {
                    '↵'
                } else {
                    *character
                }
            })
            .collect();
        let before_cursor = char_slice_width(&self.input[start..self.cursor]);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(label, Style::default().fg(Color::Cyan)),
                Span::raw(visible),
            ])),
            area,
        );
        let x = area
            .x
            .saturating_add((label_width + before_cursor).min(usize::from(u16::MAX)) as u16)
            .min(area.right().saturating_sub(1));
        if self.mode == AttachmentMode::Drive {
            frame.set_cursor_position(Position::new(x, area.y));
        }
    }
}

impl Drop for RemoteSurface {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.open_sessions.lock() {
            sessions.remove(&self.session_id);
        }
    }
}

impl Surface for RemoteSurface {
    fn title(&self) -> std::borrow::Cow<'_, str> {
        self.name.as_str().into()
    }

    fn key(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(session_surface_key(self.session_id).into())
    }

    fn badge(&self) -> Option<std::borrow::Cow<'_, str>> {
        let role = if self.connection_error.is_some() {
            "○"
        } else if self.lease.is_some() {
            "▶"
        } else {
            "·"
        };
        if self.activity.unread_lines == 0 {
            Some(role.into())
        } else {
            Some(format!("{role} +{}", self.activity.unread_lines).into())
        }
    }

    fn wide_badge(&self) -> Option<std::borrow::Cow<'_, str>> {
        let badge = self.badge().unwrap_or_default();
        Some(format!("{badge} {}", compact_elapsed(self.last_event_at)).into())
    }

    fn status(&self) -> SurfaceStatus {
        if self.connection_error.is_some() || self.snapshot.last_failed {
            SurfaceStatus::Failed
        } else if self.activity.unread_lines > 0 && !self.snapshot.running {
            SurfaceStatus::Attention
        } else if self.snapshot.running {
            SurfaceStatus::Working
        } else {
            SurfaceStatus::Ready
        }
    }

    fn input_policy(&self) -> InputPolicy {
        InputPolicy::Captured
    }

    fn reconfigure(&mut self, config: &turtletap::ShellConfig) {
        self.bindings.clone_from(&config.bindings);
    }

    fn opens_action_bar_on_escape(&self) -> bool {
        self.input.is_empty()
    }

    fn poll_interval(&self) -> Option<Duration> {
        None
    }

    fn poll_background(&mut self, context: &mut Context<'_>) -> Poll<SurfaceAction> {
        let event = match self.client.poll_event(context) {
            Poll::Ready(Some(event)) => event,
            Poll::Ready(None) => {
                return Poll::Ready(self.fail("resident connection pump stopped"));
            }
            Poll::Pending => return Poll::Pending,
        };
        Poll::Ready(self.apply_client_event(event))
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let constraints = if self.banner.is_some() {
            vec![
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ]
        } else {
            vec![
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ]
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        let (transcript_area, scroll_area, prompt_area) = if let Some(banner) = &self.banner {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    banner.as_str(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                sections[0],
            );
            (sections[1], sections[2], sections[3])
        } else {
            (sections[0], sections[1], sections[2])
        };
        let visible_lines = usize::from(transcript_area.height);
        let (start, end) = self
            .scrollback
            .window(self.snapshot.transcript.len(), visible_lines);
        let lines: Vec<Line<'_>> = self.snapshot.transcript[start..end]
            .iter()
            .map(|entry| {
                let style = match entry.kind {
                    TranscriptKind::System => Style::default().fg(Color::DarkGray),
                    TranscriptKind::Command => Style::default().fg(Color::Cyan),
                    TranscriptKind::Stdout => Style::default(),
                    TranscriptKind::Stderr => Style::default().fg(Color::Red),
                    TranscriptKind::Error => Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                };
                Line::styled(entry.text.as_str(), style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), transcript_area);
        let mut status = self.scrollback.status_line(
            leader_binding_label(&self.bindings.leaders, &self.bindings.leader_scroll_up),
            leader_binding_label(&self.bindings.leaders, &self.bindings.leader_scroll_down),
        );
        if self.snapshot.queued > 0 {
            status.push_span(Span::styled(
                format!(" · {} queued", self.snapshot.queued),
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(Paragraph::new(status), scroll_area);
        self.render_prompt(frame, prompt_area);
    }

    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
        match event {
            SurfaceEvent::Tick(elapsed) => {
                self.metadata_elapsed += elapsed;
                if self.metadata_elapsed < Duration::from_secs(2) {
                    return SurfaceAction::Ignored;
                }
                self.refresh_metadata()
            }
            SurfaceEvent::Paste(text) if self.mode == AttachmentMode::Drive => {
                self.insert_text(&text);
                SurfaceAction::Consumed
            }
            SurfaceEvent::Paste(_) => SurfaceAction::Ignored,
            SurfaceEvent::ScrollPageUp => {
                let page = self.scrollback.page_size();
                self.scrollback
                    .scroll_up(self.snapshot.transcript.len(), page);
                SurfaceAction::Consumed
            }
            SurfaceEvent::ScrollPageDown => {
                let page = self.scrollback.page_size();
                self.scrollback.scroll_down(page);
                SurfaceAction::Consumed
            }
            SurfaceEvent::Key(key) => {
                if self.take_confirmation {
                    return match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_take_driver(),
                        KeyCode::Enter | KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                            self.banner = Some("Take control cancelled.".to_owned());
                            self.take_confirmation = false;
                            SurfaceAction::Consumed
                        }
                        _ => SurfaceAction::Consumed,
                    };
                }
                if matches_binding(&self.bindings.session_release_driver, key) {
                    return self.release_driver();
                }
                if matches_binding(&self.bindings.session_take_driver, key) {
                    return self.take_driver();
                }
                if matches_binding(&self.bindings.session_clear, key) {
                    return self.command(ShellCommand::Clear);
                }
                if matches_binding(&self.bindings.session_scroll_up, key) {
                    let page = self.scrollback.page_size();
                    self.scrollback
                        .scroll_up(self.snapshot.transcript.len(), page);
                    return SurfaceAction::Consumed;
                }
                if matches_binding(&self.bindings.session_scroll_down, key) {
                    let page = self.scrollback.page_size();
                    self.scrollback.scroll_down(page);
                    return SurfaceAction::Consumed;
                }
                if matches_binding(&self.bindings.session_interrupt, key) {
                    return if self.snapshot.running {
                        self.command(ShellCommand::Interrupt)
                    } else {
                        self.clear_input();
                        SurfaceAction::Consumed
                    };
                }
                if self.input.is_empty() && matches_binding(&self.bindings.session_detach, key) {
                    return SurfaceAction::Detach;
                }
                if matches_binding(&self.bindings.session_scroll_top, key) {
                    self.scrollback.top(self.snapshot.transcript.len());
                    return SurfaceAction::Consumed;
                }
                if matches_binding(&self.bindings.session_scroll_bottom, key) {
                    self.scrollback.follow();
                    return SurfaceAction::Consumed;
                }
                if self.mode == AttachmentMode::View {
                    if matches!(key.code, KeyCode::Enter | KeyCode::Char(_)) {
                        self.banner = Some(format!(
                            "View only · press {} to take control.",
                            binding_labels(&self.bindings.session_take_driver)
                        ));
                        return SurfaceAction::Consumed;
                    }
                    return SurfaceAction::Ignored;
                }
                if self.mode == AttachmentMode::Drive
                    && let Some(start) =
                        session_delete_start(&self.bindings, &self.input, self.cursor, key)
                {
                    self.input.drain(start..self.cursor);
                    self.cursor = start;
                    return SurfaceAction::Consumed;
                }
                if let Some(cursor) =
                    session_cursor_target(&self.bindings, &self.input, self.cursor, key)
                {
                    self.cursor = cursor;
                    return SurfaceAction::Consumed;
                }
                if matches_binding(&self.bindings.session_complete, key) {
                    self.complete();
                    return SurfaceAction::Consumed;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return match key.code {
                        KeyCode::Char('d') => {
                            if self.cursor < self.input.len() {
                                self.input.remove(self.cursor);
                            }
                            SurfaceAction::Consumed
                        }
                        _ => SurfaceAction::Ignored,
                    };
                }
                match key.code {
                    KeyCode::Enter => self.submit(),
                    KeyCode::Char(character) if self.mode == AttachmentMode::Drive => {
                        self.input.insert(self.cursor, character);
                        self.cursor += 1;
                        SurfaceAction::Consumed
                    }
                    KeyCode::Backspace => {
                        if self.cursor > 0 {
                            self.cursor -= 1;
                            self.input.remove(self.cursor);
                        }
                        SurfaceAction::Consumed
                    }
                    KeyCode::Delete => {
                        if self.cursor < self.input.len() {
                            self.input.remove(self.cursor);
                        }
                        SurfaceAction::Consumed
                    }
                    KeyCode::Left => {
                        self.cursor = self.cursor.saturating_sub(1);
                        SurfaceAction::Consumed
                    }
                    KeyCode::Right => {
                        self.cursor = (self.cursor + 1).min(self.input.len());
                        SurfaceAction::Consumed
                    }
                    KeyCode::Home => {
                        self.cursor = 0;
                        SurfaceAction::Consumed
                    }
                    KeyCode::End => {
                        self.cursor = self.input.len();
                        SurfaceAction::Consumed
                    }
                    KeyCode::Up => {
                        self.history_previous();
                        SurfaceAction::Consumed
                    }
                    KeyCode::Down => {
                        self.history_next();
                        SurfaceAction::Consumed
                    }
                    KeyCode::Esc => {
                        self.clear_input();
                        SurfaceAction::Consumed
                    }
                    _ => SurfaceAction::Ignored,
                }
            }
            SurfaceEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.scrollback.scroll_up(self.snapshot.transcript.len(), 3);
                    SurfaceAction::Consumed
                }
                MouseEventKind::ScrollDown => {
                    self.scrollback.scroll_down(3);
                    SurfaceAction::Consumed
                }
                _ => SurfaceAction::Ignored,
            },
            SurfaceEvent::Resize { .. } => SurfaceAction::Ignored,
        }
    }

    fn shortcuts(&self) -> Vec<Shortcut> {
        vec![
            Shortcut::new("Enter", "Run command"),
            Shortcut::new("↑ / ↓", "Command history"),
            Shortcut::new(
                format!(
                    "{} / {}",
                    binding_labels(&self.bindings.session_word_left),
                    binding_labels(&self.bindings.session_word_right)
                ),
                "Move by word",
            ),
            Shortcut::new(
                format!(
                    "{} / {}",
                    binding_labels(&self.bindings.session_line_start),
                    binding_labels(&self.bindings.session_line_end)
                ),
                "Beginning or end of line",
            ),
            Shortcut::new(
                binding_labels(&self.bindings.session_delete_word),
                "Delete previous word",
            ),
            Shortcut::new(
                binding_labels(&self.bindings.session_delete_to_start),
                "Delete to beginning of line",
            ),
            Shortcut::new(
                binding_labels(&self.bindings.session_clear),
                "Clear transcript",
            ),
            Shortcut::new(
                binding_labels(&self.bindings.session_complete),
                "Complete added command",
            ),
            Shortcut::new(
                format!(
                    "{} / {}",
                    leader_binding_label(&self.bindings.leaders, &self.bindings.leader_scroll_up),
                    leader_binding_label(&self.bindings.leaders, &self.bindings.leader_scroll_down)
                ),
                "Scroll transcript history",
            ),
            Shortcut::new(
                format!(
                    "{} / {}",
                    binding_labels(&self.bindings.session_scroll_top),
                    binding_labels(&self.bindings.session_scroll_bottom)
                ),
                "Oldest output or live tail",
            ),
            Shortcut::new(
                binding_labels(&self.bindings.session_interrupt),
                "Interrupt command or clear input",
            ),
            Shortcut::new("Esc", "Open the action bar when input is empty"),
            Shortcut::new(
                binding_labels(&self.bindings.session_detach),
                "Detach; the session keeps running",
            ),
            Shortcut::new(
                format!(
                    "{} / {}",
                    binding_labels(&self.bindings.session_release_driver),
                    binding_labels(&self.bindings.session_take_driver)
                ),
                "Release or take driver",
            ),
        ]
    }

    fn focus(&mut self) {
        self.activity.focus();
    }

    fn blur(&mut self) {
        self.activity.blur();
    }
}

fn compact_elapsed(last_event_at: Option<u64>) -> String {
    let Some(then) = last_event_at else {
        return "new".to_owned();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let seconds = now.saturating_sub(then) / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_activity_counts_only_while_unfocused() {
        let mut activity = ScreenActivity::default();

        activity.focus();
        activity.appended(2);
        assert_eq!(activity.unread_lines, 0);

        activity.blur();
        activity.appended(3);
        assert_eq!(activity.unread_lines, 3);

        activity.focus();
        assert_eq!(activity.unread_lines, 0);
    }

    #[test]
    fn driver_role_tracks_forced_takeover_immediately() {
        let current = ClientInstanceId::new();
        let other = ClientInstanceId::new();
        let epoch = LeaseEpoch(7);

        assert_eq!(
            driver_role(
                Some(DriverLease {
                    owner: current,
                    epoch,
                }),
                current,
            ),
            (Some(epoch), AttachmentMode::Drive)
        );
        assert_eq!(
            driver_role(
                Some(DriverLease {
                    owner: other,
                    epoch: LeaseEpoch(8),
                }),
                current,
            ),
            (None, AttachmentMode::View)
        );
    }
}
