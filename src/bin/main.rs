//! Installable TurtleTap command shell.

use std::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
    env,
    ffi::{OsStr, OsString},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
};

use serde::{Deserialize, Serialize};
use turtletap::{
    Frame, InputPolicy, KeyCode, KeyModifiers, Rect, Shortcut, Surface, SurfaceAction,
    SurfaceEvent, SurfaceStatus,
    tui::{
        layout::{Constraint, Direction, Layout, Position},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::Paragraph,
    },
};

mod resident;

const MAX_TRANSCRIPT_LINES: usize = 5_000;
const MAX_HISTORY_LINES: usize = 1_000;

fn main() -> ExitCode {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    match arguments.as_slice() {
        [] => finish(resident::open()),
        [argument] if argument == OsStr::new("attach") => finish(resident::attach()),
        [command, name] if command == OsStr::new("attach") => {
            finish(resident::attach_named(&name.to_string_lossy()))
        }
        [command, name] if command == OsStr::new("view") => {
            finish(resident::view(&name.to_string_lossy()))
        }
        [command, name] if command == OsStr::new("take") => {
            finish(resident::take(&name.to_string_lossy()))
        }
        [command, name] if command == OsStr::new("new") => {
            finish(resident::create(&name.to_string_lossy()))
        }
        [command, old, new] if command == OsStr::new("rename") => finish(resident::rename(
            &old.to_string_lossy(),
            &new.to_string_lossy(),
        )),
        [argument] if argument == OsStr::new("list") => finish(resident::list()),
        [argument] if argument == OsStr::new("start") => finish(resident::start()),
        [argument] if argument == OsStr::new("status") => finish(resident::status()),
        [argument] if argument == OsStr::new("stop") => finish(resident::stop()),
        [command, name] if command == OsStr::new("stop") => {
            finish(resident::stop_session(&name.to_string_lossy()))
        }
        [argument] if argument == OsStr::new("-h") || argument == OsStr::new("--help") => {
            print_help();
            ExitCode::SUCCESS
        }
        [argument] if argument == OsStr::new("-V") || argument == OsStr::new("--version") => {
            println!("turtletap {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [command, socket] if command == OsStr::new("__serve") => {
            finish(resident::serve(PathBuf::from(socket)))
        }
        _ => {
            let rendered = arguments
                .iter()
                .map(|argument| argument.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("turtletap: unexpected arguments: {rendered}");
            eprintln!("Try 'turtletap --help'.");
            ExitCode::from(2)
        }
    }
}

fn finish(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("turtletap: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "TurtleTap command shell\n\n\
         Usage:\n  turtletap [command]\n\n\
         Commands:\n  attach [name]   Attach as the session driver\n  view <name>     Attach without mutation authority\n  take <name>     Attach and take the driver lease\n  new <name>      Create and attach to a named session\n  rename <old> <new>\n                   Rename a durable session\n  list            List resident sessions\n  start           Start the resident without attaching\n  status          Show resident and session status\n  stop [name]     Stop one session, or the resident when omitted\n\n\
         Options:\n  -h, --help       Show this help\n  -V, --version    Show the version\n\n\
         Shell commands:\n  :add <name> <command>    Add a session-local command\n  :commands                List added commands\n  :remove <name>           Remove an added command\n  :cd [path]               Change working directory\n  :history                 Show command history\n  :clear                   Clear the transcript\n  :help                    Show in-shell help\n  :quit                    Detach\n\n\
         Running turtletap without a command opens the resident session dashboard.\n\
         Type any other line inside the session to execute it through your login shell."
    );
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum TranscriptKind {
    System,
    Command,
    Stdout,
    Stderr,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TranscriptEntry {
    kind: TranscriptKind,
    text: String,
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

struct OutputLine {
    stream: OutputStream,
    text: String,
}

struct RunningCommand {
    child: Child,
    output: Receiver<OutputLine>,
    completion: Option<CommandCompletion>,
    output_disconnected: bool,
}

enum CommandCompletion {
    Exited(ExitStatus),
    WaitFailed(String),
}

pub(crate) struct CommandSurface {
    input: Vec<char>,
    cursor: usize,
    transcript: Vec<TranscriptEntry>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    commands: BTreeMap<String, String>,
    pending: VecDeque<String>,
    started_command: Option<String>,
    cwd: PathBuf,
    running: Option<RunningCommand>,
    last_failed: bool,
    revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PersistedCommandSurface {
    transcript: Vec<TranscriptEntry>,
    history: Vec<String>,
    commands: BTreeMap<String, String>,
    pending: VecDeque<String>,
    cwd: PathBuf,
    running_command: Option<String>,
    last_failed: bool,
    revision: u64,
}

impl CommandSurface {
    fn new() -> io::Result<Self> {
        let cwd = env::current_dir()?;
        let mut surface = Self {
            input: Vec::new(),
            cursor: 0,
            transcript: Vec::new(),
            history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            commands: BTreeMap::new(),
            pending: VecDeque::new(),
            started_command: None,
            cwd,
            running: None,
            last_failed: false,
            revision: 0,
        };
        surface.push(TranscriptKind::System, "TurtleTap command shell");
        surface.push(
            TranscriptKind::System,
            "Type :help for commands · Ctrl-D on an empty prompt detaches",
        );
        Ok(surface)
    }

    fn push(&mut self, kind: TranscriptKind, text: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            kind,
            text: text.into(),
        });
        let excess = self.transcript.len().saturating_sub(MAX_TRANSCRIPT_LINES);
        if excess > 0 {
            self.transcript.drain(..excess);
        }
        self.touch();
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn submit(&mut self) -> SurfaceAction {
        let line: String = self.input.iter().collect();
        let line = line.trim().to_owned();
        self.clear_input();
        let action = self.accept_line(line);
        if matches!(action, SurfaceAction::Ignored) {
            return action;
        }
        self.run_pending()
    }

    fn accept_line(&mut self, line: String) -> SurfaceAction {
        if line.is_empty() {
            return SurfaceAction::Ignored;
        }

        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
            let excess = self.history.len().saturating_sub(MAX_HISTORY_LINES);
            if excess > 0 {
                self.history.drain(..excess);
            }
        }
        self.history_cursor = None;
        self.history_draft.clear();
        self.push(TranscriptKind::Command, format!("$ {line}"));
        self.pending.push_back(line);
        SurfaceAction::Consumed
    }

    fn persisted(&self) -> PersistedCommandSurface {
        PersistedCommandSurface {
            transcript: self.transcript.clone(),
            history: self.history.clone(),
            commands: self.commands.clone(),
            pending: self.pending.clone(),
            cwd: self.cwd.clone(),
            running_command: self.started_command.clone(),
            last_failed: self.last_failed,
            revision: self.revision,
        }
    }

    fn restore(state: PersistedCommandSurface) -> Self {
        let interrupted = state.running_command;
        let mut pending = state.pending;
        if interrupted
            .as_ref()
            .is_some_and(|command| pending.front() == Some(command))
        {
            pending.pop_front();
        }
        let mut surface = Self {
            input: Vec::new(),
            cursor: 0,
            transcript: state.transcript,
            history: state.history,
            history_cursor: None,
            history_draft: String::new(),
            commands: state.commands,
            pending,
            started_command: None,
            cwd: state.cwd,
            running: None,
            last_failed: state.last_failed,
            revision: state.revision,
        };
        if let Some(command) = interrupted {
            surface.last_failed = true;
            surface.push(
                TranscriptKind::Error,
                format!("Resident restarted while this command was running: {command}"),
            );
        }
        surface
    }

    fn execute(&mut self, line: &str) -> SurfaceAction {
        if self.started_command.is_none() {
            self.started_command = Some(line.to_owned());
            self.touch();
        }
        if let Some(action) = self.run_builtin(line) {
            self.started_command = None;
            self.touch();
            return action;
        }

        let command = self.expand_command(line);
        match spawn_command(&command, &self.cwd) {
            Ok(running) => {
                self.running = Some(running);
                self.last_failed = false;
            }
            Err(error) => {
                self.started_command = None;
                self.last_failed = true;
                self.push(
                    TranscriptKind::Error,
                    format!("Could not start command: {error}"),
                );
            }
        }
        SurfaceAction::Consumed
    }

    fn run_pending(&mut self) -> SurfaceAction {
        while self.running.is_none() {
            let Some(line) = self.pending.pop_front() else {
                return SurfaceAction::Consumed;
            };
            let action = self.execute(&line);
            if matches!(action, SurfaceAction::Detach | SurfaceAction::Close) {
                return action;
            }
        }
        SurfaceAction::Consumed
    }

    #[cfg(test)]
    fn mark_next_started(&mut self) -> bool {
        if self.running.is_some() || self.started_command.is_some() {
            return false;
        }
        let Some(line) = self.pending.front().cloned() else {
            return false;
        };
        self.started_command = Some(line);
        self.touch();
        true
    }

    fn run_builtin(&mut self, line: &str) -> Option<SurfaceAction> {
        let (name, arguments) = split_command(line);
        match name {
            ":quit" | ":detach" | "exit" => Some(SurfaceAction::Detach),
            ":clear" | "clear" => {
                self.transcript.clear();
                self.touch();
                Some(SurfaceAction::Consumed)
            }
            ":help" => {
                self.show_help();
                Some(SurfaceAction::Consumed)
            }
            ":commands" => {
                self.show_commands();
                Some(SurfaceAction::Consumed)
            }
            ":history" | "history" => {
                let entries: Vec<String> = self
                    .history
                    .iter()
                    .enumerate()
                    .map(|(index, command)| format!("{:>4}  {command}", index + 1))
                    .collect();
                for entry in entries {
                    self.push(TranscriptKind::Stdout, entry);
                }
                Some(SurfaceAction::Consumed)
            }
            ":add" => {
                self.add_command(arguments);
                Some(SurfaceAction::Consumed)
            }
            ":remove" => {
                self.remove_command(arguments);
                Some(SurfaceAction::Consumed)
            }
            ":cd" | "cd" => {
                self.change_directory(arguments);
                Some(SurfaceAction::Consumed)
            }
            _ => None,
        }
    }

    fn show_help(&mut self) {
        for line in [
            ":add <name> <command>  Add a session-local command",
            ":commands              List added commands",
            ":remove <name>         Remove an added command",
            ":cd [path]             Change working directory",
            ":history               Show command history",
            ":clear                 Clear the transcript",
            ":quit                  Detach",
            "",
            "Added commands can be invoked by name with optional arguments.",
            "Unknown input runs through your login shell.",
        ] {
            self.push(TranscriptKind::System, line);
        }
    }

    fn add_command(&mut self, arguments: &str) {
        let (name, command) = split_command(arguments.trim());
        if name.is_empty() || command.is_empty() {
            self.push(TranscriptKind::Error, "Usage: :add <name> <shell command>");
            return;
        }
        if !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        {
            self.push(
                TranscriptKind::Error,
                "Command names may contain only letters, numbers, '-' and '_'.",
            );
            return;
        }

        let replaced = self.commands.insert(name.to_owned(), command.to_owned());
        let verb = if replaced.is_some() {
            "Updated"
        } else {
            "Added"
        };
        self.push(
            TranscriptKind::System,
            format!("{verb} command '{name}' → {command}"),
        );
    }

    fn remove_command(&mut self, arguments: &str) {
        let name = arguments.trim();
        if name.is_empty() {
            self.push(TranscriptKind::Error, "Usage: :remove <name>");
        } else if self.commands.remove(name).is_some() {
            self.push(TranscriptKind::System, format!("Removed command '{name}'."));
        } else {
            self.push(TranscriptKind::Error, format!("No command named '{name}'."));
        }
    }

    fn show_commands(&mut self) {
        if self.commands.is_empty() {
            self.push(TranscriptKind::System, "No commands added this session.");
            return;
        }
        let commands: Vec<String> = self
            .commands
            .iter()
            .map(|(name, command)| format!("{name:<16} {command}"))
            .collect();
        for command in commands {
            self.push(TranscriptKind::Stdout, command);
        }
    }

    fn expand_command(&self, line: &str) -> String {
        let (name, arguments) = split_command(line);
        let Some(command) = self.commands.get(name) else {
            return line.to_owned();
        };
        if arguments.is_empty() {
            command.clone()
        } else {
            format!("{command} {arguments}")
        }
    }

    fn change_directory(&mut self, arguments: &str) {
        let requested = if arguments.trim().is_empty() {
            home_directory()
        } else {
            Some(expand_home(arguments.trim()))
        };
        let Some(requested) = requested else {
            self.push(TranscriptKind::Error, "Home directory is not available.");
            return;
        };
        let path = if requested.is_absolute() {
            requested
        } else {
            self.cwd.join(requested)
        };
        match path.canonicalize() {
            Ok(path) if path.is_dir() => {
                self.cwd = path;
                self.last_failed = false;
                self.touch();
            }
            Ok(_) => {
                self.last_failed = true;
                self.push(
                    TranscriptKind::Error,
                    "The requested path is not a directory.",
                );
            }
            Err(error) => {
                self.last_failed = true;
                self.push(
                    TranscriptKind::Error,
                    format!("Could not change directory: {error}"),
                );
            }
        }
    }

    fn poll_command(&mut self) -> SurfaceAction {
        self.poll_command_inner(true)
    }

    fn poll_command_deferred(&mut self) -> SurfaceAction {
        self.poll_command_inner(false)
    }

    fn poll_command_inner(&mut self, start_next: bool) -> SurfaceAction {
        let Some(running) = self.running.as_mut() else {
            return SurfaceAction::Ignored;
        };
        let mut lines = Vec::new();

        loop {
            match running.output.try_recv() {
                Ok(line) => lines.push(line),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    running.output_disconnected = true;
                    break;
                }
            }
        }

        if running.completion.is_none() {
            running.completion = match running.child.try_wait() {
                Ok(Some(status)) => Some(CommandCompletion::Exited(status)),
                Ok(None) => None,
                Err(error) => Some(CommandCompletion::WaitFailed(error.to_string())),
            };
        }

        let changed = !lines.is_empty();
        for line in lines {
            let kind = match line.stream {
                OutputStream::Stdout => TranscriptKind::Stdout,
                OutputStream::Stderr => TranscriptKind::Stderr,
            };
            self.push(kind, line.text);
        }

        let finished = self
            .running
            .as_ref()
            .is_some_and(|running| running.output_disconnected && running.completion.is_some());
        if !finished {
            return if changed {
                SurfaceAction::Consumed
            } else {
                SurfaceAction::Ignored
            };
        }

        let Some(running) = self.running.take() else {
            return SurfaceAction::Ignored;
        };
        self.started_command = None;
        self.touch();
        match running.completion {
            Some(CommandCompletion::Exited(status)) => {
                self.last_failed = !status.success();
                if !status.success() {
                    self.push(TranscriptKind::Error, format_exit_status(status));
                }
            }
            Some(CommandCompletion::WaitFailed(error)) => {
                self.last_failed = true;
                self.push(
                    TranscriptKind::Error,
                    format!("Could not wait for command: {error}"),
                );
            }
            None => {}
        }
        if start_next {
            self.run_pending()
        } else {
            SurfaceAction::Consumed
        }
    }

    fn interrupt(&mut self) -> SurfaceAction {
        if let Some(running) = self.running.as_mut() {
            let result = interrupt_child(&mut running.child);
            match result {
                Ok(()) => self.push(TranscriptKind::System, "Interrupt requested."),
                Err(error) => self.push(
                    TranscriptKind::Error,
                    format!("Could not interrupt command: {error}"),
                ),
            }
        } else if !self.input.is_empty() {
            self.clear_input();
        }
        SurfaceAction::Consumed
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_cursor = None;
        self.history_draft.clear();
    }

    fn insert_text(&mut self, text: &str) {
        let normalized = text.replace(['\r', '\n'], " ");
        let inserted: Vec<char> = normalized.chars().collect();
        let count = inserted.len();
        self.input.splice(self.cursor..self.cursor, inserted);
        self.cursor += count;
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_cursor {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft = self.input.iter().collect();
                self.history.len() - 1
            }
        };
        self.history_cursor = Some(index);
        self.input = self.history[index].chars().collect();
        self.cursor = self.input.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_cursor = Some(next);
            self.input = self.history[next].chars().collect();
        } else {
            self.history_cursor = None;
            self.input = self.history_draft.chars().collect();
            self.history_draft.clear();
        }
        self.cursor = self.input.len();
    }

    fn complete(&mut self) {
        if self.cursor != self.input.len() {
            return;
        }
        let input: String = self.input.iter().collect();
        if input.chars().any(char::is_whitespace) {
            return;
        }

        let mut candidates: Vec<String> = [
            ":add",
            ":commands",
            ":remove",
            ":cd",
            ":history",
            ":clear",
            ":help",
            ":quit",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(self.commands.keys().cloned())
        .filter(|candidate| candidate.starts_with(&input))
        .collect();
        candidates.sort();
        candidates.dedup();

        match candidates.as_slice() {
            [candidate] => {
                self.input = candidate.chars().collect();
                self.cursor = self.input.len();
            }
            [] => {}
            _ => self.push(TranscriptKind::System, candidates.join("  ")),
        }
    }

    fn prompt_label(&self) -> String {
        if let Some(home) = home_directory()
            && let Ok(relative) = self.cwd.strip_prefix(home)
        {
            if relative.as_os_str().is_empty() {
                return "~".to_owned();
            }
            return format!("~/{}", relative.display());
        }
        self.cwd.display().to_string()
    }

    fn render_prompt(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let label = format!("{} ❯ ", self.prompt_label());
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
        let visible: String = self.input[start..end].iter().collect();
        let before_cursor = char_slice_width(&self.input[start..self.cursor]);
        let line = Line::from(vec![
            Span::styled(label, Style::default().fg(Color::Cyan)),
            Span::raw(visible),
        ]);
        frame.render_widget(Paragraph::new(line), area);

        let x = area
            .x
            .saturating_add((label_width + before_cursor).min(usize::from(u16::MAX)) as u16)
            .min(area.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(x, area.y));
    }
}

impl Surface for CommandSurface {
    fn title(&self) -> Cow<'_, str> {
        "shell".into()
    }

    fn status(&self) -> SurfaceStatus {
        if self.running.is_some() {
            SurfaceStatus::Working
        } else if self.last_failed {
            SurfaceStatus::Failed
        } else {
            SurfaceStatus::Ready
        }
    }

    fn input_policy(&self) -> InputPolicy {
        InputPolicy::Captured
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        let visible_lines = usize::from(sections[0].height);
        let start = self.transcript.len().saturating_sub(visible_lines);
        let lines: Vec<Line<'_>> = self.transcript[start..]
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
        frame.render_widget(Paragraph::new(lines), sections[0]);
        self.render_prompt(frame, sections[1]);
    }

    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
        match event {
            SurfaceEvent::Tick(_) => self.poll_command(),
            SurfaceEvent::Paste(text) => {
                self.insert_text(&text);
                SurfaceAction::Consumed
            }
            SurfaceEvent::Key(key) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return match key.code {
                        KeyCode::Char('c') => self.interrupt(),
                        KeyCode::Char('d') if self.input.is_empty() => SurfaceAction::Detach,
                        KeyCode::Char('d') => {
                            if self.cursor < self.input.len() {
                                self.input.remove(self.cursor);
                            }
                            SurfaceAction::Consumed
                        }
                        KeyCode::Char('l') => {
                            self.transcript.clear();
                            self.touch();
                            SurfaceAction::Consumed
                        }
                        _ => SurfaceAction::Ignored,
                    };
                }

                match key.code {
                    KeyCode::Enter => self.submit(),
                    KeyCode::Char(character) => {
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
                    KeyCode::Tab => {
                        self.complete();
                        SurfaceAction::Consumed
                    }
                    KeyCode::Esc => {
                        self.clear_input();
                        SurfaceAction::Consumed
                    }
                    _ => SurfaceAction::Ignored,
                }
            }
            SurfaceEvent::Mouse(_) | SurfaceEvent::Resize { .. } => SurfaceAction::Ignored,
        }
    }

    fn shortcuts(&self) -> Vec<Shortcut> {
        vec![
            Shortcut::new("Enter", "Run command"),
            Shortcut::new("↑ / ↓", "Command history"),
            Shortcut::new("Tab", "Complete added command"),
            Shortcut::new("Ctrl-C", "Interrupt command or clear input"),
            Shortcut::new("Ctrl-D", "Detach when input is empty"),
        ]
    }
}

impl Drop for CommandSurface {
    fn drop(&mut self) {
        if let Some(running) = self.running.as_mut() {
            let _ = terminate_child(&mut running.child);
            let _ = running.child.wait();
        }
    }
}

fn split_command(line: &str) -> (&str, &str) {
    let trimmed = line.trim();
    let split = trimmed
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, _)| index);
    match split {
        Some(index) => (&trimmed[..index], trimmed[index..].trim()),
        None => (trimmed, ""),
    }
}

fn spawn_command(command: &str, cwd: &Path) -> io::Result<RunningCommand> {
    let mut process = login_shell_command(command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.process_group(0);
    }
    let mut child = process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (sender, output) = mpsc::channel();

    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, OutputStream::Stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, OutputStream::Stderr, sender.clone());
    }
    drop(sender);

    Ok(RunningCommand {
        child,
        output,
        completion: None,
        output_disconnected: false,
    })
}

#[cfg(unix)]
fn interrupt_child(child: &mut Child) -> io::Result<()> {
    signal_child_group(child, "-INT")
}

#[cfg(not(unix))]
fn interrupt_child(child: &mut Child) -> io::Result<()> {
    child.kill()
}

#[cfg(unix)]
fn terminate_child(child: &mut Child) -> io::Result<()> {
    signal_child_group(child, "-KILL")
}

#[cfg(not(unix))]
fn terminate_child(child: &mut Child) -> io::Result<()> {
    child.kill()
}

#[cfg(unix)]
fn signal_child_group(child: &Child, signal: &str) -> io::Result<()> {
    let status = Command::new("kill")
        .arg(signal)
        .arg(format!("-{}", child.id()))
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "could not signal command process group: {status}"
        )))
    }
}

#[cfg(unix)]
fn login_shell_command(command: &str) -> Command {
    let shell = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut process = Command::new(shell);
    process.arg("-lc").arg(command);
    process
}

#[cfg(windows)]
fn login_shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd.exe");
    process.arg("/C").arg(command);
    process
}

fn spawn_reader(
    reader: impl Read + Send + 'static,
    stream: OutputStream,
    sender: Sender<OutputLine>,
) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let text = match line {
                Ok(line) => line,
                Err(error) => format!("output read error: {error}"),
            };
            if sender.send(OutputLine { stream, text }).is_err() {
                break;
            }
        }
    });
}

fn format_exit_status(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "Command terminated by signal.".to_owned(),
        |code| format!("Command exited with status {code}."),
    )
}

#[cfg(unix)]
fn home_directory() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(windows)]
fn home_directory() -> Option<PathBuf> {
    env::var_os("USERPROFILE").map(PathBuf::from)
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_directory().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(remainder) = path.strip_prefix("~/")
        && let Some(home) = home_directory()
    {
        return home.join(remainder);
    }
    PathBuf::from(path)
}

fn char_slice_width(characters: &[char]) -> usize {
    let text: String = characters.iter().collect();
    Line::from(text).width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_preserves_the_argument_tail() {
        assert_eq!(
            split_command(":add greet printf 'hello world'"),
            (":add", "greet printf 'hello world'")
        );
    }

    #[test]
    fn added_commands_expand_with_invocation_arguments() {
        let mut surface = CommandSurface::new().expect("current directory should be available");
        surface.add_command("greet printf hello");

        assert_eq!(surface.expand_command("greet world"), "printf hello world");
    }

    #[test]
    fn command_names_are_validated() {
        let mut surface = CommandSurface::new().expect("current directory should be available");
        surface.add_command("not/valid echo no");

        assert!(surface.commands.is_empty());
        assert!(
            surface
                .transcript
                .last()
                .is_some_and(|entry| entry.text.contains("only letters"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn submitted_input_is_queued_while_a_command_is_running() {
        let mut surface = CommandSurface::new().expect("current directory should be available");
        surface.running = Some(
            spawn_command("cat", &surface.cwd)
                .expect("the platform should provide a shell and cat"),
        );
        surface.input = "echo later".chars().collect();
        surface.cursor = surface.input.len();

        assert!(matches!(surface.submit(), SurfaceAction::Consumed));
        assert_eq!(
            surface.pending.front().map(String::as_str),
            Some("echo later")
        );
        assert!(surface.input.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn child_commands_receive_closed_stdin() {
        use std::time::{Duration, Instant};

        let mut running = spawn_command("cat", Path::new("."))
            .expect("the platform should provide a shell and cat");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match running.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "cat should exit successfully at EOF");
                    break;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = running.child.kill();
                    let _ = running.child.wait();
                    panic!("cat kept waiting because child stdin was not closed");
                }
                Err(error) => panic!("could not inspect child status: {error}"),
            }
        }
    }

    #[test]
    fn a_durably_started_command_is_not_replayed_after_restart() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        let _ = surface.accept_line("printf once".to_owned());
        assert!(surface.mark_next_started());

        let restored = CommandSurface::restore(surface.persisted());

        assert!(restored.pending.is_empty());
        assert!(restored.last_failed);
        assert!(restored.transcript.iter().any(|entry| {
            entry.text.contains("Resident restarted") && entry.text.contains("printf once")
        }));
    }
}
