//! The interactive command surface: transcript, history, and child processes.

#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
    env,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use turtletap::{
    Frame, InputPolicy, KeyBinding, KeyCode, KeyEvent, KeyModifiers, MouseEventKind, Rect,
    ShellBindings, Shortcut, Surface, SurfaceAction, SurfaceCommand, SurfaceEvent, SurfaceStatus,
    resident::EffectWake,
    tui::{
        layout::{Constraint, Direction, Layout, Position},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::Paragraph,
    },
};

pub(crate) const MAX_TRANSCRIPT_LINES: usize = 5_000;
pub(crate) const MAX_HISTORY_LINES: usize = 1_000;
pub(crate) const ACTIVE_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(5);

const CLEAR_TRANSCRIPT: &str = "command.clear";
const INTERRUPT_COMMAND: &str = "command.interrupt";
const SHOW_COMMAND_HELP: &str = "command.help";
const SHOW_COMMANDS: &str = "command.commands";
const SHOW_QUEUE: &str = "command.queue";

const fn initial_command_id() -> u64 {
    1
}

pub(crate) fn matches_binding(bindings: &[KeyBinding], key: KeyEvent) -> bool {
    bindings.iter().any(|binding| binding.matches(key))
}

pub(crate) fn binding_labels(bindings: &[KeyBinding]) -> String {
    if bindings.is_empty() {
        return "disabled".to_owned();
    }
    bindings
        .iter()
        .map(|binding| binding.label())
        .collect::<Vec<_>>()
        .join(" / ")
}

pub(crate) fn command_shortcut(command: SurfaceCommand, bindings: &[KeyBinding]) -> SurfaceCommand {
    if let Some(binding) = bindings.first() {
        command.with_shortcut(binding.label())
    } else {
        command
    }
}

pub(crate) fn leader_binding_label(leaders: &[KeyBinding], suffixes: &[KeyBinding]) -> String {
    match (leaders.first(), suffixes.first()) {
        (Some(leader), Some(suffix)) => format!("{} {}", leader.label(), suffix.label()),
        _ => "disabled".to_owned(),
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Scrollback {
    lines_from_bottom: usize,
    viewport_height: usize,
}

impl Scrollback {
    pub(crate) fn window(&mut self, total_lines: usize, viewport_height: usize) -> (usize, usize) {
        self.viewport_height = viewport_height;
        self.lines_from_bottom = self
            .lines_from_bottom
            .min(total_lines.saturating_sub(viewport_height));
        let end = total_lines.saturating_sub(self.lines_from_bottom);
        (end.saturating_sub(viewport_height), end)
    }

    pub(crate) fn scroll_up(&mut self, total_lines: usize, amount: usize) {
        let maximum = total_lines.saturating_sub(self.viewport_height);
        self.lines_from_bottom = self.lines_from_bottom.saturating_add(amount).min(maximum);
    }

    pub(crate) fn scroll_down(&mut self, amount: usize) {
        self.lines_from_bottom = self.lines_from_bottom.saturating_sub(amount);
    }

    pub(crate) fn top(&mut self, total_lines: usize) {
        self.lines_from_bottom = total_lines.saturating_sub(self.viewport_height);
    }

    pub(crate) fn follow(&mut self) {
        self.lines_from_bottom = 0;
    }

    pub(crate) fn appended(&mut self, count: usize, total_lines: usize) {
        if self.lines_from_bottom > 0 {
            self.lines_from_bottom = self
                .lines_from_bottom
                .saturating_add(count)
                .min(total_lines.saturating_sub(self.viewport_height));
        }
    }

    pub(crate) fn page_size(&self) -> usize {
        self.viewport_height.saturating_sub(1).max(1)
    }

    pub(crate) fn status_line(&self, scroll_up: String, scroll_down: String) -> Line<'static> {
        if self.lines_from_bottom == 0 {
            Line::from(vec![
                Span::styled("● live", Style::default().fg(Color::Green)),
                Span::styled(
                    format!(" · {scroll_up} scrollback"),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("↑ history", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(
                        " · {} newer line{} · {scroll_down} return",
                        self.lines_from_bottom,
                        if self.lines_from_bottom == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum TranscriptKind {
    System,
    Command,
    Stdout,
    Stderr,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TranscriptEntry {
    pub(crate) kind: TranscriptKind,
    pub(crate) text: String,
}

#[derive(Clone, Copy)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

pub(crate) struct OutputLine {
    stream: OutputStream,
    text: String,
    sequence: Option<u64>,
}

pub(crate) struct RunningCommand {
    child: Option<Child>,
    #[cfg(unix)]
    worker: Option<UnixStream>,
    output: Receiver<OutputLine>,
    completion: Option<CommandCompletion>,
    output_disconnected: bool,
    local_waker: Option<Arc<Mutex<Option<Waker>>>>,
    worker_sequence: u64,
    worker_completion: Option<Arc<Mutex<Option<i32>>>>,
}

#[derive(Clone)]
enum OutputNotifier {
    Resident(EffectWake),
    Local(Arc<Mutex<Option<Waker>>>),
}

impl OutputNotifier {
    fn notify(&self) {
        match self {
            Self::Resident(wake) => wake.notify(),
            Self::Local(waker) => {
                if let Ok(mut slot) = waker.lock()
                    && let Some(waker) = slot.take()
                {
                    waker.wake();
                }
            }
        }
    }
}

pub(crate) enum CommandCompletion {
    Exited(ExitStatus),
    WorkerExited(i32),
    WaitFailed(String),
}

pub(crate) struct CommandSurface {
    bindings: ShellBindings,
    input: Vec<char>,
    cursor: usize,
    transcript: Vec<TranscriptEntry>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    commands: BTreeMap<String, String>,
    environment: BTreeMap<String, String>,
    pending: VecDeque<String>,
    started_command: Option<String>,
    started_command_id: Option<u64>,
    next_command_id: u64,
    cwd: PathBuf,
    running: Option<RunningCommand>,
    reconnect_scheduled: bool,
    last_worker_sequence: u64,
    worker_recoverable: bool,
    last_failed: bool,
    revision: u64,
    scrollback: Scrollback,
    multiline_confirmation: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PersistedCommandSurface {
    transcript: Vec<TranscriptEntry>,
    history: Vec<String>,
    commands: BTreeMap<String, String>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    pending: VecDeque<String>,
    cwd: PathBuf,
    running_command: Option<String>,
    #[serde(default)]
    running_command_id: Option<u64>,
    #[serde(default = "initial_command_id")]
    next_command_id: u64,
    #[serde(default)]
    last_worker_sequence: u64,
    #[serde(default)]
    worker_recoverable: bool,
    last_failed: bool,
    revision: u64,
}

impl CommandSurface {
    pub(crate) fn new() -> io::Result<Self> {
        let cwd = env::current_dir()?;
        let mut surface = Self {
            bindings: ShellBindings::default(),
            input: Vec::new(),
            cursor: 0,
            transcript: Vec::new(),
            history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            commands: BTreeMap::new(),
            environment: BTreeMap::new(),
            pending: VecDeque::new(),
            started_command: None,
            started_command_id: None,
            next_command_id: initial_command_id(),
            cwd,
            running: None,
            reconnect_scheduled: false,
            last_worker_sequence: 0,
            worker_recoverable: false,
            last_failed: false,
            revision: 0,
            scrollback: Scrollback::default(),
            multiline_confirmation: false,
        };
        surface.push(TranscriptKind::System, "TurtleTap command shell");
        surface.push(
            TranscriptKind::System,
            "Type :help for commands · Esc on an empty prompt opens the action bar",
        );
        surface.push(
            TranscriptKind::System,
            "Child commands receive closed stdin. Run interactive programs in a separate terminal.",
        );
        Ok(surface)
    }

    pub(crate) fn push(&mut self, kind: TranscriptKind, text: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            kind,
            text: text.into(),
        });
        let excess = self.transcript.len().saturating_sub(MAX_TRANSCRIPT_LINES);
        if excess > 0 {
            self.transcript.drain(..excess);
        }
        self.scrollback.appended(1, self.transcript.len());
        self.touch();
    }

    pub(crate) fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub(crate) fn submit(&mut self) -> SurfaceAction {
        let line: String = self.input.iter().collect();
        let line = line.trim().to_owned();
        if self.multiline_confirmation {
            self.multiline_confirmation = false;
            self.push(
                TranscriptKind::System,
                "Multiline command ready. Press Enter again to run it, or Esc to cancel.",
            );
            return SurfaceAction::Consumed;
        }
        self.clear_input();
        if matches!(split_command(&line).0, ":queue" | ":cancel") {
            return self.run_builtin(&line).unwrap_or(SurfaceAction::Consumed);
        }
        let action = self.accept_line(line);
        if matches!(action, SurfaceAction::Ignored) {
            return action;
        }
        self.run_pending()
    }

    pub(crate) fn accept_line(&mut self, line: String) -> SurfaceAction {
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

    pub(crate) fn persisted(&self) -> PersistedCommandSurface {
        PersistedCommandSurface {
            transcript: self.transcript.clone(),
            history: self.history.clone(),
            commands: self.commands.clone(),
            environment: self.environment.clone(),
            pending: self.pending.clone(),
            cwd: self.cwd.clone(),
            running_command: self.started_command.clone(),
            running_command_id: self.started_command_id,
            next_command_id: self.next_command_id,
            last_worker_sequence: self
                .running
                .as_ref()
                .map_or(self.last_worker_sequence, |running| running.worker_sequence),
            worker_recoverable: self.worker_recoverable,
            last_failed: self.last_failed,
            revision: self.revision,
        }
    }

    pub(crate) fn restore(state: PersistedCommandSurface) -> Self {
        let interrupted = state.running_command;
        let recoverable = state.worker_recoverable && state.running_command_id.is_some();
        let mut pending = state.pending;
        if !recoverable
            && interrupted
                .as_ref()
                .is_some_and(|command| pending.front() == Some(command))
        {
            pending.pop_front();
        }
        let mut surface = Self {
            bindings: ShellBindings::default(),
            input: Vec::new(),
            cursor: 0,
            transcript: state.transcript,
            history: state.history,
            history_cursor: None,
            history_draft: String::new(),
            commands: state.commands,
            environment: state.environment,
            pending,
            started_command: recoverable.then(|| interrupted.clone()).flatten(),
            started_command_id: recoverable.then_some(state.running_command_id).flatten(),
            next_command_id: state.next_command_id,
            cwd: state.cwd,
            running: None,
            reconnect_scheduled: false,
            last_worker_sequence: state.last_worker_sequence,
            worker_recoverable: recoverable,
            last_failed: state.last_failed,
            revision: state.revision,
            scrollback: Scrollback::default(),
            multiline_confirmation: false,
        };
        if !recoverable && let Some(command) = interrupted {
            surface.last_failed = true;
            surface.push(
                TranscriptKind::Error,
                format!("Resident restarted while this command was running: {command}"),
            );
        }
        surface
    }

    pub(crate) fn execute(&mut self, line: &str) -> SurfaceAction {
        if self.started_command.is_none() {
            self.started_command = Some(line.to_owned());
            self.started_command_id = Some(self.allocate_command_id());
            self.touch();
        }
        if let Some(action) = self.run_builtin(line) {
            self.started_command = None;
            self.started_command_id = None;
            self.touch();
            return action;
        }

        let command = self.with_environment(&self.expand_command(line));
        match spawn_command(&command, &self.cwd) {
            Ok(running) => {
                self.running = Some(running);
                self.last_failed = false;
            }
            Err(error) => {
                self.started_command = None;
                self.started_command_id = None;
                self.last_failed = true;
                self.push(
                    TranscriptKind::Error,
                    format!("Could not start command: {error}"),
                );
            }
        }
        SurfaceAction::Consumed
    }

    pub(crate) fn run_pending(&mut self) -> SurfaceAction {
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
    pub(crate) fn mark_next_started(&mut self) -> bool {
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

    pub(crate) fn run_builtin(&mut self, line: &str) -> Option<SurfaceAction> {
        let (name, arguments) = split_command(line);
        match name {
            ":quit" | ":detach" | "exit" => Some(SurfaceAction::Detach),
            ":clear" | "clear" => {
                self.transcript.clear();
                self.scrollback.follow();
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
            ":queue" => {
                self.show_queue();
                Some(SurfaceAction::Consumed)
            }
            ":cancel" => {
                self.cancel_queued(arguments);
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
            "export" => {
                self.export_variable(arguments);
                Some(SurfaceAction::Consumed)
            }
            "unset" => {
                self.unset_variable(arguments);
                Some(SurfaceAction::Consumed)
            }
            "alias" => {
                self.alias_command(arguments);
                Some(SurfaceAction::Consumed)
            }
            "unalias" => {
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

    pub(crate) fn show_help(&mut self) {
        for line in [
            ":add <name> <command>  Add a session-local command",
            ":commands              List added commands",
            ":remove <name>         Remove an added command",
            ":cd [path]             Change working directory",
            "export NAME=value       Persist an environment value",
            "unset NAME              Remove a persistent environment value",
            "alias NAME=command      Persist a command alias",
            "unalias NAME            Remove a command alias",
            ":history               Show command history",
            ":queue                 Show commands waiting to run",
            ":cancel <n|all>        Remove queued commands",
            ":clear                 Clear the transcript",
            ":quit                  Detach",
            "",
            "Added commands can be invoked by name with optional arguments.",
            "Unknown input runs through your login shell.",
        ] {
            self.push(TranscriptKind::System, line);
        }
    }

    pub(crate) fn add_command(&mut self, arguments: &str) {
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

    pub(crate) fn remove_command(&mut self, arguments: &str) {
        let name = arguments.trim();
        if name.is_empty() {
            self.push(TranscriptKind::Error, "Usage: :remove <name>");
        } else if self.commands.remove(name).is_some() {
            self.push(TranscriptKind::System, format!("Removed command '{name}'."));
        } else {
            self.push(TranscriptKind::Error, format!("No command named '{name}'."));
        }
    }

    pub(crate) fn show_commands(&mut self) {
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

    pub(crate) fn show_queue(&mut self) {
        if self.pending.is_empty() {
            self.push(TranscriptKind::System, "No commands are queued.");
            return;
        }
        let entries: Vec<_> = self
            .pending
            .iter()
            .enumerate()
            .map(|(index, command)| format!("{:>4}  {command}", index + 1))
            .collect();
        for entry in entries {
            self.push(TranscriptKind::Stdout, entry);
        }
    }

    pub(crate) fn cancel_queued(&mut self, arguments: &str) {
        let target = arguments.trim();
        if target == "all" {
            let removed = self.pending.len();
            self.pending.clear();
            self.push(
                TranscriptKind::System,
                format!(
                    "Cancelled {removed} queued command{}.",
                    if removed == 1 { "" } else { "s" }
                ),
            );
            return;
        }
        let Ok(index) = target.parse::<usize>() else {
            self.push(TranscriptKind::Error, "Usage: :cancel <queue-number|all>");
            return;
        };
        if index == 0 || index > self.pending.len() {
            self.push(
                TranscriptKind::Error,
                format!("Queue entry {index} does not exist. Use :queue to inspect the queue."),
            );
            return;
        }
        if let Some(command) = self.pending.remove(index - 1) {
            self.push(
                TranscriptKind::System,
                format!("Cancelled queued command: {command}"),
            );
        }
    }

    pub(crate) fn expand_command(&self, line: &str) -> String {
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

    fn with_environment(&self, command: &str) -> String {
        if self.environment.is_empty() {
            return command.to_owned();
        }
        let exports = self
            .environment
            .iter()
            .map(|(name, value)| format!("export {name}={}", shell_quote(value)))
            .collect::<Vec<_>>()
            .join("; ");
        format!("{exports}; {command}")
    }

    fn export_variable(&mut self, arguments: &str) {
        let Some((name, value)) = arguments.split_once('=') else {
            self.push(TranscriptKind::Error, "Usage: export NAME=value");
            return;
        };
        let name = name.trim();
        if !valid_environment_name(name) {
            self.push(
                TranscriptKind::Error,
                "Environment names must begin with a letter or '_' and contain only letters, numbers, and '_'.",
            );
            return;
        }
        self.environment
            .insert(name.to_owned(), unquote_shell_value(value.trim()));
        self.push(
            TranscriptKind::System,
            format!("Exported {name} for future commands."),
        );
    }

    fn unset_variable(&mut self, arguments: &str) {
        let name = arguments.trim();
        if !valid_environment_name(name) {
            self.push(TranscriptKind::Error, "Usage: unset NAME");
            return;
        }
        self.environment.remove(name);
        self.push(TranscriptKind::System, format!("Unset {name}."));
    }

    fn alias_command(&mut self, arguments: &str) {
        if arguments.trim().is_empty() {
            self.show_commands();
            return;
        }
        let Some((name, command)) = arguments.split_once('=') else {
            self.push(TranscriptKind::Error, "Usage: alias NAME=command");
            return;
        };
        self.add_command(&format!(
            "{} {}",
            name.trim(),
            unquote_shell_value(command.trim())
        ));
    }

    pub(crate) fn change_directory(&mut self, arguments: &str) {
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

    pub(crate) fn poll_command(&mut self) -> SurfaceAction {
        self.poll_command_inner(true)
    }

    pub(crate) fn poll_command_deferred(&mut self) -> SurfaceAction {
        self.poll_command_inner(false)
    }

    pub(crate) fn poll_command_inner(&mut self, start_next: bool) -> SurfaceAction {
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
            running.completion = match running.child.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => Some(CommandCompletion::Exited(status)),
                    Ok(None) => None,
                    Err(error) => Some(CommandCompletion::WaitFailed(error.to_string())),
                },
                None => running
                    .worker_completion
                    .as_ref()
                    .and_then(|completion| completion.lock().ok().and_then(|value| *value))
                    .map(CommandCompletion::WorkerExited),
            };
        }

        let mut visible = Vec::new();
        for line in lines {
            if let Some(sequence) = line.sequence {
                if sequence <= running.worker_sequence {
                    continue;
                }
                running.worker_sequence = sequence;
            }
            let kind = match line.stream {
                OutputStream::Stdout => TranscriptKind::Stdout,
                OutputStream::Stderr => TranscriptKind::Stderr,
            };
            visible.push((kind, line.text));
        }
        self.last_worker_sequence = running.worker_sequence;
        let changed = !visible.is_empty();
        let _ = running;
        for (kind, text) in visible {
            self.push(kind, text);
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
        self.started_command_id = None;
        self.worker_recoverable = false;
        self.reconnect_scheduled = false;
        self.touch();
        match running.completion {
            Some(CommandCompletion::Exited(status)) => {
                self.last_failed = !status.success();
                if !status.success() {
                    self.push(TranscriptKind::Error, format_exit_status(status));
                }
            }
            Some(CommandCompletion::WorkerExited(code)) => {
                self.last_failed = code != 0;
                if code != 0 {
                    self.push(
                        TranscriptKind::Error,
                        format!("Command exited with status {code}."),
                    );
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

    pub(crate) fn interrupt(&mut self) -> SurfaceAction {
        if let Some(running) = self.running.as_mut() {
            let result = running.interrupt();
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

    pub(crate) fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_cursor = None;
        self.history_draft.clear();
        self.multiline_confirmation = false;
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.contains('\n') {
            self.multiline_confirmation = true;
        }
        let inserted: Vec<char> = normalized.chars().collect();
        let count = inserted.len();
        self.input.splice(self.cursor..self.cursor, inserted);
        self.cursor += count;
    }

    pub(crate) fn history_previous(&mut self) {
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

    pub(crate) fn history_next(&mut self) {
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

    pub(crate) fn complete(&mut self) {
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
            ":queue",
            ":cancel",
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

    pub(crate) fn prompt_label(&self) -> String {
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

    pub(crate) fn render_prompt(&self, frame: &mut Frame<'_>, area: Rect) {
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

    /// Monotonic counter incremented whenever visible state changes.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Committed transcript lines, oldest first.
    pub(crate) fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    /// Submitted command lines, oldest first.
    pub(crate) fn history(&self) -> &[String] {
        &self.history
    }

    /// Names of the user-defined command aliases.
    pub(crate) fn command_names(&self) -> Vec<String> {
        self.commands.keys().cloned().collect()
    }

    /// Whether a child process is executing.
    pub(crate) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Whether the most recent command exited non-zero.
    pub(crate) fn has_failed(&self) -> bool {
        self.last_failed
    }

    /// Number of command lines waiting to run.
    pub(crate) fn queued(&self) -> usize {
        self.pending.len()
    }

    /// Captures the durable state needed to rebuild this surface after a
    /// leader restart. The transcript is carried separately by the event.
    pub(crate) fn recovery_state(&self) -> ShellRecoveryState {
        ShellRecoveryState {
            history: self.history.clone(),
            commands: self.commands.clone(),
            environment: self.environment.clone(),
            pending: self.pending.clone(),
            cwd: self.cwd.clone(),
            running_command: self.started_command.clone(),
            running_command_id: self.started_command_id,
            next_command_id: self.next_command_id,
            last_worker_sequence: self
                .running
                .as_ref()
                .map_or(self.last_worker_sequence, |running| running.worker_sequence),
            worker_recoverable: self.worker_recoverable,
            last_failed: self.last_failed,
            revision: self.revision,
        }
    }

    /// Restores durable state captured by [`Self::recovery_state`]. Any local
    /// child process and in-progress history navigation are discarded, because
    /// neither survives the leader that owned them.
    pub(crate) fn apply_recovery(&mut self, state: &ShellRecoveryState) {
        self.history.clone_from(&state.history);
        self.history_cursor = None;
        self.history_draft.clear();
        self.commands.clone_from(&state.commands);
        self.environment.clone_from(&state.environment);
        self.pending.clone_from(&state.pending);
        self.cwd.clone_from(&state.cwd);
        self.started_command.clone_from(&state.running_command);
        self.started_command_id = state.running_command_id;
        self.next_command_id = state.next_command_id;
        self.running = None;
        self.reconnect_scheduled = false;
        self.last_worker_sequence = state.last_worker_sequence;
        self.worker_recoverable = state.worker_recoverable;
        self.last_failed = state.last_failed;
        self.revision = state.revision;
    }

    /// Appends replayed transcript entries, pruning the oldest beyond the cap.
    pub(crate) fn append_transcript(&mut self, entries: impl IntoIterator<Item = TranscriptEntry>) {
        self.transcript.extend(entries);
        let excess = self.transcript.len().saturating_sub(MAX_TRANSCRIPT_LINES);
        if excess > 0 {
            self.transcript.drain(..excess);
        }
    }

    /// Replaces the transcript wholesale, used when a replayed event does not
    /// extend what this surface already holds.
    pub(crate) fn replace_transcript(&mut self, entries: &[TranscriptEntry]) {
        self.transcript.clear();
        self.transcript.extend_from_slice(entries);
    }

    /// A compact summary for the session list: the last few transcript lines
    /// and whether a command is running. Capped so the payload stays small even
    /// when the transcript is enormous.
    pub(crate) fn digest(&self) -> serde_json::Value {
        const PREVIEW_LINES: usize = 3;
        const PREVIEW_BYTES: usize = 200;
        let preview: Vec<String> = self
            .transcript
            .iter()
            .rev()
            .take(PREVIEW_LINES)
            .map(|entry| truncate_utf8_bytes(&entry.text, PREVIEW_BYTES).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let status = if self.running.is_some() {
            "working"
        } else if self.last_failed {
            "failed"
        } else if self.transcript.is_empty() {
            "ready"
        } else {
            "complete"
        };
        serde_json::json!({
            "preview": preview,
            "status": status,
        })
    }

    /// Discards the transcript and returns to following live output.
    pub(crate) fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.scrollback.follow();
        self.touch();
    }

    /// Whether a command has been dequeued and is awaiting its child process.
    pub(crate) fn has_started_command(&self) -> bool {
        self.started_command.is_some()
    }

    /// Records that the started command's child process is now running.
    pub(crate) fn command_launched(&mut self, running: RunningCommand) {
        let mut running = running;
        running.worker_sequence = self.last_worker_sequence;
        if self
            .started_command
            .as_ref()
            .is_some_and(|command| self.pending.front() == Some(command))
        {
            let _ = self.pending.pop_front();
        }
        self.running = Some(running);
        self.reconnect_scheduled = false;
        self.last_failed = false;
    }

    pub(crate) fn mark_started_worker(&mut self) {
        self.worker_recoverable = true;
        self.touch();
    }

    pub(crate) fn worker_cursor(&self) -> u64 {
        self.last_worker_sequence
    }

    /// Records a child-process spawn failure.
    pub(crate) fn command_launch_failed(&mut self, message: impl std::fmt::Display) {
        if self
            .started_command
            .as_ref()
            .is_some_and(|command| self.pending.front() == Some(command))
        {
            let _ = self.pending.pop_front();
        }
        self.started_command = None;
        self.started_command_id = None;
        self.worker_recoverable = false;
        self.reconnect_scheduled = false;
        self.last_failed = true;
        self.push(
            TranscriptKind::Error,
            format!("Could not start command: {message}"),
        );
    }

    /// Drops the pending entry for a launch whose outcome is unknown, leaving
    /// the surface otherwise untouched.
    pub(crate) fn command_outcome_unknown(&mut self) {
        if self.started_command.is_some() {
            let _ = self.pending.pop_front();
        }
        self.started_command = None;
        self.started_command_id = None;
        self.worker_recoverable = false;
        self.reconnect_scheduled = false;
        self.last_failed = true;
        self.push(
            TranscriptKind::Error,
            "The command may have started, but its outcome could not be recovered.",
        );
    }

    /// Advances the pending queue, running builtins inline, and returns the
    /// command line and working directory for the next external command.
    ///
    /// Returns `None` when a command is already running, one is already
    /// started, or the queue drains without reaching an external command.
    pub(crate) fn start_next_external(&mut self) -> Option<(u64, String, PathBuf, bool)> {
        if self.running.is_none()
            && self.worker_recoverable
            && !self.reconnect_scheduled
            && let (Some(command_id), Some(line)) =
                (self.started_command_id, self.started_command.clone())
        {
            self.reconnect_scheduled = true;
            return Some((
                command_id,
                self.with_environment(&self.expand_command(&line)),
                self.cwd.clone(),
                true,
            ));
        }
        while self.running.is_none() && self.started_command.is_none() {
            let line = self.pending.front().cloned()?;
            self.last_worker_sequence = 0;
            self.worker_recoverable = false;
            self.started_command = Some(line.clone());
            let command_id = self.allocate_command_id();
            self.started_command_id = Some(command_id);
            self.touch();
            if self.run_builtin(&line).is_some() {
                let _ = self.pending.pop_front();
                self.started_command = None;
                self.started_command_id = None;
                self.touch();
                continue;
            }
            return Some((
                command_id,
                self.with_environment(&self.expand_command(&line)),
                self.cwd.clone(),
                false,
            ));
        }
        None
    }

    fn allocate_command_id(&mut self) -> u64 {
        let command_id = self.next_command_id.max(initial_command_id());
        self.next_command_id = command_id.saturating_add(1);
        command_id
    }
}

impl RunningCommand {
    fn interrupt(&mut self) -> io::Result<()> {
        if let Some(child) = self.child.as_mut() {
            return interrupt_child(child);
        }
        #[cfg(unix)]
        if let Some(worker) = self.worker.as_mut() {
            crate::worker::write_frame(worker, &crate::worker::WorkerRequest::Interrupt)?;
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "command process is no longer available",
        ))
    }
}

#[cfg(unix)]
pub(crate) fn running_from_worker(
    stream: UnixStream,
    control: UnixStream,
    wake: Option<EffectWake>,
) -> io::Result<RunningCommand> {
    let (sender, output) = mpsc::channel();
    let completion = Arc::new(Mutex::new(None));
    let reader_completion = Arc::clone(&completion);
    thread::spawn(move || {
        let mut stream = stream;
        loop {
            let event = match crate::worker::read_frame::<crate::worker::WorkerEvent>(&mut stream) {
                Ok(event) => event,
                Err(error) => {
                    let _ = sender.send(OutputLine {
                        stream: OutputStream::Stderr,
                        text: format!("worker protocol error: {error}"),
                        sequence: None,
                    });
                    if let Ok(mut completion) = reader_completion.lock() {
                        *completion = Some(125);
                    }
                    break;
                }
            };
            match event {
                crate::worker::WorkerEvent::Ready => {}
                crate::worker::WorkerEvent::OutputGap {
                    first_available, ..
                } => {
                    let _ = sender.send(OutputLine {
                        stream: OutputStream::Stderr,
                        text: format!(
                            "Earlier worker output was pruned; replay resumes at sequence {first_available}."
                        ),
                        sequence: None,
                    });
                }
                crate::worker::WorkerEvent::Output {
                    sequence,
                    stderr,
                    text,
                } => {
                    if sender
                        .send(OutputLine {
                            stream: if stderr {
                                OutputStream::Stderr
                            } else {
                                OutputStream::Stdout
                            },
                            text,
                            sequence: Some(sequence),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                crate::worker::WorkerEvent::Completed { sequence: _, code } => {
                    if let Ok(mut completion) = reader_completion.lock() {
                        *completion = Some(code);
                    }
                    drop(sender);
                    if let Some(wake) = &wake {
                        wake.notify();
                    }
                    return;
                }
                crate::worker::WorkerEvent::Error { message } => {
                    let _ = sender.send(OutputLine {
                        stream: OutputStream::Stderr,
                        text: message,
                        sequence: None,
                    });
                }
                crate::worker::WorkerEvent::Stopped => {}
            }
            if let Some(wake) = &wake {
                wake.notify();
            }
        }
        if let Some(wake) = &wake {
            wake.notify();
        }
    });
    Ok(RunningCommand {
        child: None,
        worker: Some(control),
        output,
        completion: None,
        output_disconnected: false,
        local_waker: None,
        worker_sequence: 0,
        worker_completion: Some(completion),
    })
}

fn truncate_utf8_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut boundary = limit;
    while !text.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    &text[..boundary]
}

/// Durable, non-transcript state required to rebuild a [`CommandSurface`]
/// after its resident leader restarts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ShellRecoveryState {
    history: Vec<String>,
    commands: BTreeMap<String, String>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    pending: VecDeque<String>,
    cwd: PathBuf,
    running_command: Option<String>,
    #[serde(default)]
    running_command_id: Option<u64>,
    #[serde(default = "initial_command_id")]
    next_command_id: u64,
    #[serde(default)]
    last_worker_sequence: u64,
    #[serde(default)]
    worker_recoverable: bool,
    last_failed: bool,
    revision: u64,
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

    fn reconfigure(&mut self, config: &turtletap::ShellConfig) {
        self.bindings.clone_from(&config.bindings);
    }

    fn opens_action_bar_on_escape(&self) -> bool {
        self.input.is_empty()
    }

    fn poll_interval(&self) -> Option<Duration> {
        (self.started_command.is_some() || !self.pending.is_empty())
            .then_some(ACTIVE_COMMAND_POLL_INTERVAL)
    }

    fn poll_background(&mut self, context: &mut Context<'_>) -> Poll<SurfaceAction> {
        let Some(waker) = self
            .running
            .as_ref()
            .and_then(|running| running.local_waker.as_ref())
            .cloned()
        else {
            return Poll::Pending;
        };
        if let Ok(mut slot) = waker.lock() {
            *slot = Some(context.waker().clone());
        }
        match self.poll_command() {
            SurfaceAction::Ignored => Poll::Pending,
            action => Poll::Ready(action),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        let visible_lines = usize::from(sections[0].height);
        let (start, end) = self.scrollback.window(self.transcript.len(), visible_lines);
        let lines: Vec<Line<'_>> = self.transcript[start..end]
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
        let mut status = self.scrollback.status_line(
            leader_binding_label(&self.bindings.leaders, &self.bindings.leader_scroll_up),
            leader_binding_label(&self.bindings.leaders, &self.bindings.leader_scroll_down),
        );
        if !self.pending.is_empty() {
            status.push_span(Span::styled(
                format!(" · {} queued", self.pending.len()),
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(Paragraph::new(status), sections[1]);
        self.render_prompt(frame, sections[2]);
    }

    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
        match event {
            SurfaceEvent::Tick(_) => self.poll_command(),
            SurfaceEvent::Paste(text) => {
                self.insert_text(&text);
                SurfaceAction::Consumed
            }
            SurfaceEvent::ScrollPageUp => {
                let page = self.scrollback.page_size();
                self.scrollback.scroll_up(self.transcript.len(), page);
                SurfaceAction::Consumed
            }
            SurfaceEvent::ScrollPageDown => {
                let page = self.scrollback.page_size();
                self.scrollback.scroll_down(page);
                SurfaceAction::Consumed
            }
            SurfaceEvent::Key(key) => {
                if matches_binding(&self.bindings.session_clear, key) {
                    self.transcript.clear();
                    self.scrollback.follow();
                    self.touch();
                    return SurfaceAction::Consumed;
                }
                if matches_binding(&self.bindings.session_scroll_up, key) {
                    let page = self.scrollback.page_size();
                    self.scrollback.scroll_up(self.transcript.len(), page);
                    return SurfaceAction::Consumed;
                }
                if matches_binding(&self.bindings.session_scroll_down, key) {
                    let page = self.scrollback.page_size();
                    self.scrollback.scroll_down(page);
                    return SurfaceAction::Consumed;
                }
                if matches_binding(&self.bindings.session_interrupt, key) {
                    return self.interrupt();
                }
                if self.input.is_empty() && matches_binding(&self.bindings.session_detach, key) {
                    return SurfaceAction::Detach;
                }
                if matches_binding(&self.bindings.session_scroll_top, key) {
                    self.scrollback.top(self.transcript.len());
                    return SurfaceAction::Consumed;
                }
                if matches_binding(&self.bindings.session_scroll_bottom, key) {
                    self.scrollback.follow();
                    return SurfaceAction::Consumed;
                }
                if let Some(start) =
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
                    KeyCode::Esc => {
                        self.clear_input();
                        SurfaceAction::Consumed
                    }
                    _ => SurfaceAction::Ignored,
                }
            }
            SurfaceEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.scrollback.scroll_up(self.transcript.len(), 3);
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
                "Detach when input is empty",
            ),
        ]
    }

    fn commands(&self) -> Vec<SurfaceCommand> {
        let mut commands = vec![
            command_shortcut(
                SurfaceCommand::new(CLEAR_TRANSCRIPT, "Clear transcript")
                    .with_description("Command session"),
                &self.bindings.session_clear,
            ),
            SurfaceCommand::new(SHOW_QUEUE, "Show queued commands")
                .with_description("Command session"),
            SurfaceCommand::new(SHOW_COMMANDS, "Show added commands")
                .with_description("Command session"),
            SurfaceCommand::new(SHOW_COMMAND_HELP, "Show command help")
                .with_description("Command session"),
        ];
        if self.running.is_some() {
            commands.insert(
                0,
                command_shortcut(
                    SurfaceCommand::new(INTERRUPT_COMMAND, "Interrupt running command")
                        .with_description("Command session"),
                    &self.bindings.session_interrupt,
                ),
            );
        }
        commands
    }

    fn execute_command(&mut self, id: &str) -> SurfaceAction {
        match id {
            CLEAR_TRANSCRIPT => self.run_builtin(":clear").unwrap_or(SurfaceAction::Ignored),
            INTERRUPT_COMMAND => self.interrupt(),
            SHOW_COMMAND_HELP => self.run_builtin(":help").unwrap_or(SurfaceAction::Ignored),
            SHOW_COMMANDS => self
                .run_builtin(":commands")
                .unwrap_or(SurfaceAction::Ignored),
            SHOW_QUEUE => self.run_builtin(":queue").unwrap_or(SurfaceAction::Ignored),
            _ => SurfaceAction::Ignored,
        }
    }
}

impl Drop for CommandSurface {
    fn drop(&mut self) {
        if let Some(running) = self.running.as_mut()
            && let Some(child) = running.child.as_mut()
        {
            let _ = terminate_child(child);
            let _ = child.wait();
        }
    }
}

pub(crate) fn split_command(line: &str) -> (&str, &str) {
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

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn unquote_shell_value(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn spawn_command(command: &str, cwd: &Path) -> io::Result<RunningCommand> {
    let local_waker = Arc::new(Mutex::new(None));
    spawn_command_with_notifier(
        command,
        cwd,
        Some(OutputNotifier::Local(Arc::clone(&local_waker))),
        Some(local_waker),
    )
}

pub(crate) fn spawn_command_with_wake(
    command: &str,
    cwd: &Path,
    wake: Option<EffectWake>,
) -> io::Result<RunningCommand> {
    spawn_command_with_notifier(command, cwd, wake.map(OutputNotifier::Resident), None)
}

fn spawn_command_with_notifier(
    command: &str,
    cwd: &Path,
    wake: Option<OutputNotifier>,
    local_waker: Option<Arc<Mutex<Option<Waker>>>>,
) -> io::Result<RunningCommand> {
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
        spawn_reader(stdout, OutputStream::Stdout, sender.clone(), wake.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, OutputStream::Stderr, sender.clone(), wake.clone());
    }
    drop(sender);

    Ok(RunningCommand {
        child: Some(child),
        #[cfg(unix)]
        worker: None,
        output,
        completion: None,
        output_disconnected: false,
        local_waker,
        worker_sequence: 0,
        worker_completion: None,
    })
}

#[cfg(unix)]
pub(crate) fn interrupt_child(child: &mut Child) -> io::Result<()> {
    signal_child_group(child, "-INT")
}

#[cfg(not(unix))]
pub(crate) fn interrupt_child(child: &mut Child) -> io::Result<()> {
    child.kill()
}

#[cfg(unix)]
pub(crate) fn terminate_child(child: &mut Child) -> io::Result<()> {
    signal_child_group(child, "-KILL")
}

#[cfg(not(unix))]
pub(crate) fn terminate_child(child: &mut Child) -> io::Result<()> {
    child.kill()
}

#[cfg(unix)]
pub(crate) fn signal_child_group(child: &Child, signal: &str) -> io::Result<()> {
    let status = Command::new("kill")
        .arg(signal)
        .arg("--")
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
pub(crate) fn login_shell_command(command: &str) -> Command {
    let shell = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut process = Command::new(shell);
    process.arg("-lc").arg(command);
    process
}

#[cfg(windows)]
pub(crate) fn login_shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd.exe");
    process.arg("/C").arg(command);
    process
}

fn spawn_reader(
    reader: impl Read + Send + 'static,
    stream: OutputStream,
    sender: Sender<OutputLine>,
    wake: Option<OutputNotifier>,
) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let text = match line {
                Ok(line) => line,
                Err(error) => format!("output read error: {error}"),
            };
            if sender
                .send(OutputLine {
                    stream,
                    text,
                    sequence: None,
                })
                .is_err()
            {
                break;
            }
            if let Some(wake) = &wake {
                wake.notify();
            }
        }
        if let Some(wake) = &wake {
            wake.notify();
        }
    });
}

pub(crate) fn format_exit_status(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "Command terminated by signal.".to_owned(),
        |code| format!("Command exited with status {code}."),
    )
}

#[cfg(unix)]
pub(crate) fn home_directory() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(windows)]
pub(crate) fn home_directory() -> Option<PathBuf> {
    env::var_os("USERPROFILE").map(PathBuf::from)
}

pub(crate) fn expand_home(path: &str) -> PathBuf {
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

pub(crate) fn char_slice_width(characters: &[char]) -> usize {
    let text: String = characters.iter().collect();
    Line::from(text).width()
}

pub(crate) fn session_cursor_target(
    bindings: &ShellBindings,
    input: &[char],
    cursor: usize,
    key: KeyEvent,
) -> Option<usize> {
    let cursor = cursor.min(input.len());
    if matches_binding(&bindings.session_line_start, key) {
        return Some(0);
    }
    if matches_binding(&bindings.session_line_end, key) {
        return Some(input.len());
    }
    if matches_binding(&bindings.session_word_left, key) {
        return Some(previous_word_boundary(input, cursor));
    }
    if matches_binding(&bindings.session_word_right, key) {
        return Some(next_word_boundary(input, cursor));
    }
    None
}

pub(crate) fn session_delete_start(
    bindings: &ShellBindings,
    input: &[char],
    cursor: usize,
    key: KeyEvent,
) -> Option<usize> {
    let cursor = cursor.min(input.len());
    if matches_binding(&bindings.session_delete_to_start, key) {
        return Some(0);
    }
    matches_binding(&bindings.session_delete_word, key)
        .then(|| previous_word_boundary(input, cursor))
}

pub(crate) fn previous_word_boundary(input: &[char], cursor: usize) -> usize {
    let mut cursor = cursor.min(input.len());
    while cursor > 0 && !is_word_character(input[cursor - 1]) {
        cursor -= 1;
    }
    while cursor > 0 && is_word_character(input[cursor - 1]) {
        cursor -= 1;
    }
    cursor
}

pub(crate) fn next_word_boundary(input: &[char], cursor: usize) -> usize {
    let mut cursor = cursor.min(input.len());
    while cursor < input.len() && is_word_character(input[cursor]) {
        cursor += 1;
    }
    while cursor < input.len() && !is_word_character(input[cursor]) {
        cursor += 1;
    }
    cursor
}

pub(crate) fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use turtletap::KeyEvent;

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

    #[test]
    fn action_bar_commands_expose_and_execute_command_session_operations() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        surface.push(TranscriptKind::Stdout, "visible output");

        let commands = surface.commands();
        assert!(
            commands
                .iter()
                .any(|command| command.id == CLEAR_TRANSCRIPT)
        );
        assert!(commands.iter().any(|command| command.id == SHOW_QUEUE));

        assert!(matches!(
            surface.execute_command(CLEAR_TRANSCRIPT),
            SurfaceAction::Consumed
        ));
        assert!(surface.transcript.is_empty());
    }

    #[test]
    fn escape_action_bar_fallback_is_available_only_with_empty_input() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        assert!(surface.opens_action_bar_on_escape());

        surface.insert_text("echo");
        assert!(!surface.opens_action_bar_on_escape());

        let _ = surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::empty(),
        )));
        assert!(surface.opens_action_bar_on_escape());
    }

    #[test]
    fn session_actions_use_custom_bindings_and_empty_lists_disable_defaults() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        surface.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Stdout,
            text: "keep until custom clear".to_owned(),
        });
        surface.insert_text("hello");
        surface.bindings.session_clear = vec![KeyBinding::new(KeyCode::Left, KeyModifiers::ALT)];

        let _ = surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL,
        )));
        assert!(!surface.transcript.is_empty());

        let _ = surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::ALT,
        )));
        assert!(surface.transcript.is_empty());
        assert_eq!(surface.cursor, 5, "configured action must precede editing");
    }

    #[test]
    fn digest_truncates_unicode_on_a_character_boundary() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        surface.push(TranscriptKind::Stdout, format!("{}é", "a".repeat(199)));

        let digest = surface.digest();
        let preview = digest["preview"]
            .as_array()
            .and_then(|preview| preview.last())
            .and_then(serde_json::Value::as_str)
            .expect("digest preview should be text");

        assert_eq!(preview.len(), 199);
        assert_eq!(digest["status"], "complete");
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
    fn multiline_paste_requires_a_second_enter_before_queueing() {
        let mut surface = CommandSurface::new().expect("current directory should be available");
        surface.running = Some(
            spawn_command("cat", &surface.cwd)
                .expect("the platform should provide a shell and cat"),
        );
        surface.insert_text("printf one\nprintf two");

        assert!(matches!(surface.submit(), SurfaceAction::Consumed));
        assert!(surface.pending.is_empty());
        assert!(!surface.input.is_empty());
        assert!(surface.transcript.last().is_some_and(|entry| {
            entry
                .text
                .contains("Press Enter again to run it, or Esc to cancel")
        }));

        assert!(matches!(surface.submit(), SurfaceAction::Consumed));
        assert_eq!(
            surface.pending.front().map(String::as_str),
            Some("printf one\nprintf two")
        );
    }

    #[test]
    fn queued_commands_can_be_inspected_and_cancelled() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        surface.pending.push_back("first".to_owned());
        surface.pending.push_back("second".to_owned());

        surface.show_queue();
        assert!(
            surface
                .transcript
                .iter()
                .any(|entry| entry.text.contains("2  second"))
        );
        surface.cancel_queued("1");
        assert_eq!(surface.pending.front().map(String::as_str), Some("second"));
        surface.cancel_queued("all");
        assert!(surface.pending.is_empty());
    }

    #[test]
    fn exported_environment_and_aliases_survive_recovery() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        let _ = surface.run_builtin("export PROFILE='release build'");
        let _ = surface.run_builtin("alias ll='ls -la'");

        assert_eq!(
            surface.with_environment("printf ok"),
            "export PROFILE='release build'; printf ok"
        );
        let restored = CommandSurface::restore(surface.persisted());
        assert_eq!(
            restored.environment.get("PROFILE").map(String::as_str),
            Some("release build")
        );
        assert_eq!(
            restored.commands.get("ll").map(String::as_str),
            Some("ls -la")
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_commands_receive_closed_stdin() {
        use std::time::{Duration, Instant};

        let mut running = spawn_command("cat", Path::new("."))
            .expect("the platform should provide a shell and cat");
        let child = running.child.as_mut().expect("legacy command owns a child");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "cat should exit successfully at EOF");
                    break;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("cat kept waiting ten seconds because child stdin was not closed");
                }
                Err(error) => panic!("could not inspect child status: {error}"),
            }
        }
    }

    #[test]
    fn active_commands_request_low_latency_polling() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        assert_eq!(surface.poll_interval(), None);

        surface.pending.push_back("printf ready".to_owned());
        assert_eq!(surface.poll_interval(), Some(ACTIVE_COMMAND_POLL_INTERVAL));
    }

    #[test]
    fn active_polling_meets_the_five_millisecond_latency_budget() {
        assert!(
            ACTIVE_COMMAND_POLL_INTERVAL <= Duration::from_millis(5),
            "active command polling exceeded its latency budget"
        );
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

    #[test]
    fn scrollback_holds_position_while_new_output_arrives() {
        let mut scrollback = Scrollback::default();
        assert_eq!(scrollback.window(100, 10), (90, 100));

        scrollback.scroll_up(100, 9);
        assert_eq!(scrollback.window(100, 10), (81, 91));

        scrollback.appended(3, 103);
        assert_eq!(scrollback.window(103, 10), (81, 91));
        assert_eq!(scrollback.lines_from_bottom, 12);

        scrollback.follow();
        assert_eq!(scrollback.window(103, 10), (93, 103));
    }

    #[test]
    fn natural_writing_keys_move_by_word_and_line() {
        let input: Vec<_> = "echo alpha-beta gamma".chars().collect();
        let gamma = input
            .iter()
            .position(|character| *character == 'g')
            .expect("gamma should be present");

        assert_eq!(
            session_cursor_target(
                &ShellBindings::default(),
                &input,
                input.len(),
                KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            ),
            Some(gamma)
        );
        assert_eq!(
            session_cursor_target(
                &ShellBindings::default(),
                &input,
                0,
                KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            ),
            Some(5)
        );
        assert_eq!(
            session_cursor_target(
                &ShellBindings::default(),
                &input,
                gamma,
                KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER),
            ),
            Some(0)
        );
        assert_eq!(
            session_cursor_target(
                &ShellBindings::default(),
                &input,
                0,
                KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER),
            ),
            Some(input.len())
        );

        let mut surface = CommandSurface::new().expect("surface should initialize");
        surface.input = input;
        surface.cursor = surface.input.len();
        assert!(matches!(
            surface.handle(SurfaceEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::ALT,
            ))),
            SurfaceAction::Consumed
        ));
        assert_eq!(surface.cursor, gamma);
        surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::SUPER,
        )));
        assert_eq!(surface.cursor, 0);
        surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::SUPER,
        )));
        assert_eq!(surface.cursor, surface.input.len());
    }

    #[test]
    fn terminal_fallbacks_navigate_and_delete_naturally() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        surface.input = "echo alpha beta".chars().collect();
        surface.cursor = surface.input.len();

        surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        )));
        assert_eq!(surface.input.iter().collect::<String>(), "echo alpha beta");
        assert_eq!(surface.cursor, 11);

        surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::ALT,
        )));
        assert_eq!(surface.cursor, surface.input.len());

        surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::ALT,
        )));
        assert_eq!(surface.input.iter().collect::<String>(), "echo alpha ");

        surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::SUPER,
        )));
        assert!(surface.input.is_empty());
        assert_eq!(surface.cursor, 0);

        surface.input = "fallback".chars().collect();
        surface.cursor = surface.input.len();
        surface.handle(SurfaceEvent::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        assert!(surface.input.is_empty());
        assert_eq!(surface.cursor, 0);
    }

    #[test]
    fn cmd_k_clears_the_transcript_and_returns_to_live_output() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        surface.scrollback.lines_from_bottom = 4;
        assert!(!surface.transcript.is_empty());

        assert!(matches!(
            surface.handle(SurfaceEvent::Key(KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::SUPER,
            ))),
            SurfaceAction::Consumed
        ));
        assert!(surface.transcript.is_empty());
        assert_eq!(surface.scrollback.lines_from_bottom, 0);
    }
}
