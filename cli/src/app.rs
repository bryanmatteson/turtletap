//! Resident application: session snapshots, events, and recovery.

use std::{
    io::{self},
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use turtletap::resident::{
    ApplicationError, Durability, EffectContext, EffectId, EffectRequest, ResidentApplication,
    ResidentSession, SessionId, SessionTransition, ShutdownReason,
};

use crate::command::{
    CommandSurface, PersistedCommandSurface, RunningCommand, ShellRecoveryState, TranscriptEntry,
    spawn_command_with_wake,
};
use crate::worker::WorkerManager;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SessionSnapshot {
    pub(crate) revision: u64,
    pub(crate) transcript: Vec<TranscriptEntry>,
    pub(crate) history: Vec<String>,
    pub(crate) commands: Vec<String>,
    pub(crate) cwd_label: String,
    pub(crate) running: bool,
    pub(crate) last_failed: bool,
    pub(crate) queued: usize,
}

impl SessionSnapshot {
    pub(crate) fn from_session(session: &CommandSurface) -> Self {
        Self {
            revision: session.revision(),
            transcript: session.transcript().to_vec(),
            history: session.history().to_vec(),
            commands: session.command_names(),
            cwd_label: session.prompt_label(),
            running: session.is_running(),
            last_failed: session.has_failed(),
            queued: session.queued(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ShellCommand {
    Prepare,
    SetWorkingDirectory { path: PathBuf },
    Submit { line: String },
    Interrupt,
    Clear,
    QueueStatus,
    CancelQueued { target: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ShellEvent {
    Appended {
        revision: u64,
        entries: Vec<TranscriptEntry>,
        running: bool,
        last_failed: bool,
        queued: usize,
        cwd_label: String,
        history: Vec<String>,
        commands: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<PersistedCommandSurface>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<ShellRecoveryState>,
    },
    Reset {
        snapshot: SessionSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<PersistedCommandSurface>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<ShellRecoveryState>,
    },
}

impl ShellEvent {
    pub(crate) fn between(previous: &SessionSnapshot, current: &SessionSnapshot) -> Self {
        let prefix_matches = current.transcript.starts_with(&previous.transcript);
        if prefix_matches {
            Self::Appended {
                revision: current.revision,
                entries: current.transcript[previous.transcript.len()..].to_vec(),
                running: current.running,
                last_failed: current.last_failed,
                queued: current.queued,
                cwd_label: current.cwd_label.clone(),
                history: current.history.clone(),
                commands: current.commands.clone(),
                state: None,
                recovery: None,
            }
        } else {
            Self::Reset {
                snapshot: current.clone(),
                state: None,
                recovery: None,
            }
        }
    }

    pub(crate) fn from_surface(previous: &SessionSnapshot, surface: &CommandSurface) -> Self {
        let current = SessionSnapshot::from_session(surface);
        let recovery = surface.recovery_state();
        match Self::between(previous, &current) {
            Self::Appended {
                revision,
                entries,
                running,
                last_failed,
                queued,
                cwd_label,
                history,
                commands,
                ..
            } => Self::Appended {
                revision,
                entries,
                running,
                last_failed,
                queued,
                cwd_label,
                history,
                commands,
                state: None,
                recovery: Some(recovery),
            },
            Self::Reset { snapshot, .. } => Self::Reset {
                snapshot,
                state: None,
                recovery: Some(recovery),
            },
        }
    }

    pub(crate) fn replay_into(&self, surface: &mut CommandSurface) -> Result<(), ApplicationError> {
        match self {
            Self::Appended {
                entries,
                state,
                recovery,
                ..
            } => {
                if let Some(state) = state {
                    *surface = CommandSurface::restore(state.clone());
                    return Ok(());
                }
                let recovery = recovery.as_ref().ok_or_else(missing_recovery_state)?;
                surface.append_transcript(entries.iter().cloned());
                surface.apply_recovery(recovery);
            }
            Self::Reset {
                snapshot,
                state,
                recovery,
            } => {
                if let Some(state) = state {
                    *surface = CommandSurface::restore(state.clone());
                    return Ok(());
                }
                let recovery = recovery.as_ref().ok_or_else(missing_recovery_state)?;
                surface.replace_transcript(&snapshot.transcript);
                surface.apply_recovery(recovery);
            }
        }
        Ok(())
    }

    pub(crate) fn appended_count(&self, previous: &SessionSnapshot) -> usize {
        match self {
            Self::Appended { entries, .. } => entries.len(),
            Self::Reset { snapshot, .. } => {
                transcript_append_count(&previous.transcript, &snapshot.transcript)
            }
        }
    }

    pub(crate) fn apply(&self, snapshot: &mut SessionSnapshot) {
        match self {
            Self::Appended {
                revision,
                entries,
                running,
                last_failed,
                queued,
                cwd_label,
                history,
                commands,
                ..
            } => {
                snapshot.revision = *revision;
                snapshot.transcript.extend(entries.iter().cloned());
                let excess = snapshot
                    .transcript
                    .len()
                    .saturating_sub(crate::command::MAX_TRANSCRIPT_LINES);
                if excess > 0 {
                    snapshot.transcript.drain(..excess);
                }
                snapshot.running = *running;
                snapshot.last_failed = *last_failed;
                snapshot.queued = *queued;
                snapshot.cwd_label.clone_from(cwd_label);
                snapshot.history.clone_from(history);
                snapshot.commands.clone_from(commands);
            }
            Self::Reset {
                snapshot: current, ..
            } => snapshot.clone_from(current),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ShellApplication {
    workers: WorkerManager,
}

impl ShellApplication {
    pub(crate) fn new(state_root: PathBuf, durability: Durability) -> Self {
        Self {
            workers: WorkerManager::new(state_root, durability),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ShellEffect {
    PrepareWorker,
    /// One-shot child encoded by version-1 journal effects.
    Run {
        command: String,
        cwd: PathBuf,
    },
    /// Deduplicated dispatch through a durable per-session worker.
    WorkerRun {
        command_id: u64,
        command: String,
        cwd: PathBuf,
        after: u64,
    },
}

pub(crate) enum ShellEffectOutput {
    Prepared,
    Running(RunningCommand),
}

impl ResidentApplication for ShellApplication {
    type Command = ShellCommand;
    type Event = ShellEvent;
    type Snapshot = SessionSnapshot;
    type State = PersistedCommandSurface;
    type Effect = ShellEffect;
    type EffectOutput = ShellEffectOutput;
    type Session = CommandSurface;

    const STORAGE_VERSION: u32 = 2;

    fn create(&self, _name: &str) -> Result<Self::Session, ApplicationError> {
        CommandSurface::new().map_err(shell_io)
    }

    fn restore(&self, state: Self::State) -> Result<Self::Session, ApplicationError> {
        Ok(CommandSurface::restore(state))
    }

    fn session_attached(&self, session: SessionId) -> Result<(), ApplicationError> {
        self.workers
            .prepare(session)
            .map_err(|error| ApplicationError::new("worker_prepare", error.to_string()))
    }

    fn session_stopping(&self, session: SessionId) -> Result<(), ApplicationError> {
        self.workers
            .stop_session(session)
            .map_err(|error| ApplicationError::new("worker_stop", error.to_string()))
    }

    fn host_stopping(&self, reason: ShutdownReason) -> Result<(), ApplicationError> {
        self.workers
            .stop_all(reason)
            .map_err(|error| ApplicationError::new("worker_stop", error.to_string()))
    }

    async fn execute(
        &self,
        context: EffectContext,
        effect: Self::Effect,
    ) -> Result<Self::EffectOutput, ApplicationError> {
        match effect {
            ShellEffect::PrepareWorker => {
                self.workers
                    .prepare(context.session)
                    .map_err(|error| ApplicationError::new("worker_prepare", error.to_string()))?;
                Ok(ShellEffectOutput::Prepared)
            }
            ShellEffect::Run { command, cwd } => {
                spawn_command_with_wake(&command, &cwd, Some(context.wake_handle()))
                    .map(ShellEffectOutput::Running)
                    .map_err(shell_io)
            }
            ShellEffect::WorkerRun {
                command_id,
                command,
                cwd,
                after,
            } => self
                .workers
                .execute(
                    context.session,
                    command_id,
                    &command,
                    &cwd,
                    after,
                    Some(context.wake_handle()),
                )
                .map(ShellEffectOutput::Running)
                .map_err(shell_io),
        }
    }

    fn migrate(
        &self,
        stored_version: u32,
        state: serde_json::Value,
    ) -> Result<Self::State, ApplicationError> {
        if !matches!(stored_version, 0 | 1 | Self::STORAGE_VERSION) {
            return Err(ApplicationError::new(
                "unsupported_storage_version",
                format!(
                    "stored shell state is version {stored_version}; this binary requires {}",
                    Self::STORAGE_VERSION
                ),
            ));
        }
        serde_json::from_value(state)
            .map_err(|error| ApplicationError::new("invalid_checkpoint", error.to_string()))
    }
}

impl ResidentSession for CommandSurface {
    type Command = ShellCommand;
    type Event = ShellEvent;
    type Snapshot = SessionSnapshot;
    type State = PersistedCommandSurface;
    type Effect = ShellEffect;
    type EffectOutput = ShellEffectOutput;

    fn snapshot(&self) -> Self::Snapshot {
        SessionSnapshot::from_session(self)
    }

    fn state(&self) -> Self::State {
        self.persisted()
    }

    fn digest(&self) -> Option<serde_json::Value> {
        Some(self.digest())
    }

    fn command(
        &mut self,
        command: Self::Command,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        let previous = SessionSnapshot::from_session(self);
        let effects = match command {
            ShellCommand::Prepare => {
                if self.is_running() || self.has_started_command() {
                    Vec::new()
                } else {
                    vec![EffectRequest::at_least_once(ShellEffect::PrepareWorker)]
                }
            }
            ShellCommand::SetWorkingDirectory { path } => {
                self.set_working_directory(path).map_err(shell_io)?;
                Vec::new()
            }
            ShellCommand::Submit { line } => {
                let _ = self.accept_line(line);
                resident_effects(self)
            }
            ShellCommand::Interrupt => {
                let _ = self.interrupt();
                Vec::new()
            }
            ShellCommand::Clear => {
                self.clear_transcript();
                Vec::new()
            }
            ShellCommand::QueueStatus => {
                self.show_queue();
                Vec::new()
            }
            ShellCommand::CancelQueued { target } => {
                self.cancel_queued(&target);
                Vec::new()
            }
        };
        Ok(shell_transition(previous, self, effects))
    }

    fn poll(
        &mut self,
        _elapsed: Duration,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        let previous = SessionSnapshot::from_session(self);
        let _ = self.poll_command_deferred();
        let effects = resident_effects(self);
        Ok(shell_transition(previous, self, effects))
    }

    fn effect_completed(
        &mut self,
        _effect: EffectId,
        output: Result<Self::EffectOutput, ApplicationError>,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        let previous = SessionSnapshot::from_session(self);
        match output {
            Ok(ShellEffectOutput::Prepared) => {}
            Ok(ShellEffectOutput::Running(running)) => self.command_launched(running),
            Err(error) if error.code() == "worker_prepare" => {}
            Err(error)
                if error.code() == "effect_outcome_unknown" && !self.has_started_command() =>
            {
                self.command_outcome_unknown();
            }
            Err(error) => self.command_launch_failed(error),
        }
        let effects = resident_effects(self);
        Ok(shell_transition(previous, self, effects))
    }

    fn replay(&mut self, event: &Self::Event) -> Result<(), ApplicationError> {
        event.replay_into(self)
    }
}

pub(crate) fn missing_recovery_state() -> ApplicationError {
    ApplicationError::new(
        "legacy_journal_event",
        "journal event does not contain replayable application state",
    )
}

pub(crate) fn shell_transition(
    previous: SessionSnapshot,
    surface: &CommandSurface,
    effects: Vec<EffectRequest<ShellEffect>>,
) -> SessionTransition<ShellEvent, ShellEffect> {
    let current = SessionSnapshot::from_session(surface);
    let events = (current != previous)
        .then(|| ShellEvent::from_surface(&previous, surface))
        .into_iter()
        .collect();
    SessionTransition::with_effects(events, effects)
}

pub(crate) fn resident_effects(surface: &mut CommandSurface) -> Vec<EffectRequest<ShellEffect>> {
    surface
        .start_next_external()
        .map(|(command_id, command, cwd, _reconnect)| {
            if std::env::var_os("TURTLETAP_PERSISTENT_SHELL").as_deref()
                == Some(std::ffi::OsStr::new("0"))
            {
                vec![EffectRequest::at_most_once(ShellEffect::Run {
                    command,
                    cwd,
                })]
            } else {
                surface.mark_started_worker();
                vec![EffectRequest::at_least_once(ShellEffect::WorkerRun {
                    command_id,
                    command,
                    cwd,
                    after: surface.worker_cursor(),
                })]
            }
        })
        .unwrap_or_default()
}

pub(crate) fn shell_io(error: io::Error) -> ApplicationError {
    ApplicationError::new("shell_io", error.to_string())
}

pub(crate) fn transcript_append_count(
    previous: &[TranscriptEntry],
    current: &[TranscriptEntry],
) -> usize {
    if current.starts_with(previous) {
        return current.len().saturating_sub(previous.len());
    }
    if current.is_empty() {
        return 0;
    }
    for removed in 1..previous.len() {
        let overlap = previous.len() - removed;
        if overlap <= current.len() && previous[removed..] == current[..overlap] {
            return current.len() - overlap;
        }
    }
    current.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::TranscriptKind;

    #[test]
    fn prepare_command_prewarms_without_changing_visible_state() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        let before = SessionSnapshot::from_session(&surface);

        let transition = ResidentSession::command(&mut surface, ShellCommand::Prepare)
            .expect("prepare command should be accepted");

        assert!(transition.events.is_empty());
        assert_eq!(transition.effects.len(), 1);
        assert!(matches!(
            transition.effects[0].effect,
            ShellEffect::PrepareWorker
        ));
        assert_eq!(SessionSnapshot::from_session(&surface), before);
    }

    #[test]
    fn incremental_event_roundtrips() {
        let mut before = SessionSnapshot {
            revision: 1,
            transcript: vec![],
            history: vec![],
            commands: vec![],
            cwd_label: "~".to_owned(),
            running: false,
            last_failed: false,
            queued: 0,
        };
        let mut after = before.clone();
        after.revision = 2;
        after.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Stdout,
            text: "hello".to_owned(),
        });
        let event = ShellEvent::between(&before, &after);
        event.apply(&mut before);
        assert_eq!(before, after);
    }

    #[test]
    fn recovery_event_size_does_not_scale_with_existing_transcript() {
        let mut surface = CommandSurface::new().expect("surface should initialize");
        for index in 0..1_000 {
            surface.push(
                TranscriptKind::Stdout,
                format!("{index:04} {}", "x".repeat(200)),
            );
        }
        let previous = SessionSnapshot::from_session(&surface);
        let baseline = surface.persisted();

        let _ = surface.accept_line(":add greet printf hello".to_owned());
        assert!(resident_effects(&mut surface).is_empty());
        let event = ShellEvent::from_surface(&previous, &surface);

        let event_size = serde_json::to_vec(&event)
            .expect("recovery event should serialize")
            .len();
        let checkpoint_size = serde_json::to_vec(&surface.persisted())
            .expect("checkpoint should serialize")
            .len();
        assert!(
            event_size * 20 < checkpoint_size,
            "event was {event_size} bytes for a {checkpoint_size}-byte checkpoint"
        );

        let mut recovered = CommandSurface::restore(baseline);
        event
            .replay_into(&mut recovered)
            .expect("compact event should replay");
        assert_eq!(recovered.persisted(), surface.persisted());
    }

    #[test]
    fn transcript_append_count_handles_a_pruned_front() {
        let entry = |text: &str| TranscriptEntry {
            kind: TranscriptKind::Stdout,
            text: text.to_owned(),
        };
        let previous = vec![entry("a"), entry("b"), entry("c"), entry("d")];
        let current = vec![entry("c"), entry("d"), entry("e"), entry("f")];

        assert_eq!(transcript_append_count(&previous, &current), 2);
        assert_eq!(transcript_append_count(&previous, &[]), 0);
        assert_eq!(transcript_append_count(&previous, &[entry("new")]), 1);
    }
}
