//! CLI subcommand implementations and resident supervision.

#[cfg(not(unix))]
use std::{io, io::IsTerminal as _, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    Human,
    Json,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InteractiveOptions {
    pub(crate) no_color: bool,
    pub(crate) reduced_motion: bool,
}

#[cfg(unix)]
mod imp {
    use std::{
        env, fs,
        io::{self, BufRead as _, IsTerminal as _, Write as _},
        os::unix::process::CommandExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use serde::Serialize;
    use turtletap::{
        Shell,
        resident::{
            AttachmentMode, ClientRequest, ControlResult, Durability, ResidentHost,
            ResidentHostConfig, SessionSelector,
            runtime::tokio::{TokioRuntime, TokioUnixTransport},
        },
    };

    use super::{InteractiveOptions, OutputFormat};
    use crate::app::{ShellApplication, ShellCommand};
    use crate::async_client;
    use crate::client::{SessionClient, protocol_error};
    use crate::command::split_command;
    use crate::overview::SessionDashboard;
    use crate::remote::{RemoteSurface, open_sessions};
    use turtletap::resident::{blocking, supervisor};

    pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(5);
    pub(crate) const SERVER_TICK: Duration = Duration::from_millis(10);
    pub(crate) const OUTBOUND_CAPACITY: usize = 256;
    pub(crate) const EVENT_HISTORY: usize = 1_024;
    pub(crate) const DEFAULT_SESSION: &str = "default";

    #[derive(Clone, Copy)]
    enum InitialTarget<'a> {
        Overview,
        Named {
            name: &'a str,
            mode: AttachmentMode,
            force: bool,
        },
    }

    pub(crate) fn open(options: InteractiveOptions) -> io::Result<()> {
        interactive_preflight(options)?;
        let path = socket_path();
        let _ = ensure_started(&path)?;
        attach_all(&path, InitialTarget::Overview, options)
    }

    pub(crate) fn attach_named(name: &str, options: InteractiveOptions) -> io::Result<()> {
        interactive_preflight(options)?;
        let path = socket_path();
        let _ = ensure_started(&path)?;
        attach_path(&path, name, AttachmentMode::Drive, false, options)
    }

    pub(crate) fn view(name: &str, options: InteractiveOptions) -> io::Result<()> {
        interactive_preflight(options)?;
        let path = socket_path();
        require_running(&path)?;
        attach_path(&path, name, AttachmentMode::View, false, options)
    }

    pub(crate) fn take(
        name: &str,
        yes: bool,
        no_input: bool,
        options: InteractiveOptions,
    ) -> io::Result<()> {
        interactive_preflight(options)?;
        let path = socket_path();
        require_running(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let sessions = list_sessions(&mut client)?;
        let session = find_session(sessions, name)?;
        if session.driver.is_some() && !yes {
            confirm(
                &format!(
                    "Replace the current driver for session '{}'? The other terminal will become view-only.",
                    session.name
                ),
                no_input,
            )?;
        }
        attach_path(&path, &session.name, AttachmentMode::Drive, true, options)
    }

    pub(crate) fn create(
        name: &str,
        requested_directory: Option<&Path>,
        no_attach: bool,
        options: InteractiveOptions,
        format: OutputFormat,
    ) -> io::Result<()> {
        validate_session_name(name)?;
        let directory = resolve_start_directory(requested_directory)?;
        if !no_attach {
            interactive_preflight(options)?;
        }
        let path = socket_path();
        let _ = ensure_started(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let result = client.request(ClientRequest::CreateSession {
            name: name.to_owned(),
        })?;
        let ControlResult::Created { session } = result else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident returned the wrong create response",
            ));
        };
        if let Err(error) = initialize_session_directory(&mut client, &session, &directory) {
            let cleanup = client.request(ClientRequest::StopSession {
                session: session.id,
            });
            return match cleanup {
                Ok(ControlResult::Stopping) => Err(error),
                Ok(_) => Err(io::Error::other(format!(
                    "{error}; resident returned the wrong cleanup response"
                ))),
                Err(cleanup) => Err(io::Error::other(format!(
                    "{error}; session cleanup failed: {cleanup}"
                ))),
            };
        }
        if no_attach {
            if format == OutputFormat::Json {
                return print_json(&serde_json::json!({
                    "created": true,
                    "session": session_json(&session),
                    "cwd": directory,
                    "attached": false,
                }));
            }
            println!(
                "Created session '{}' in {}.",
                session.name,
                directory.display()
            );
            println!(
                "Attach with: turtletap attach {}",
                shell_word(&session.name)
            );
            return Ok(());
        }
        attach_path(&path, &session.name, AttachmentMode::Drive, false, options)
    }

    fn initialize_session_directory(
        client: &mut SessionClient,
        session: &turtletap::resident::SessionSummary,
        directory: &Path,
    ) -> io::Result<()> {
        let attached = client.request(ClientRequest::Attach {
            session: SessionSelector::Id(session.id),
            mode: AttachmentMode::Drive,
            after: None,
            force: false,
        })?;
        let ControlResult::Attached {
            session: attached,
            lease: Some(lease),
        } = attached
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident returned the wrong initialization attach response",
            ));
        };
        if attached.id != session.id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident attached the wrong session during initialization",
            ));
        }
        let command = serde_json::to_value(ShellCommand::SetWorkingDirectory {
            path: directory.to_owned(),
        })
        .map_err(protocol_error)?;
        let accepted = client.request(ClientRequest::Command {
            session: session.id,
            lease,
            command,
        })?;
        if !matches!(
            accepted,
            ControlResult::Accepted {
                session: accepted,
                ..
            } if accepted == session.id
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident returned the wrong directory initialization response",
            ));
        }
        let _ = client.request(ClientRequest::Detach {
            session: session.id,
        });
        Ok(())
    }

    fn resolve_start_directory(requested: Option<&Path>) -> io::Result<PathBuf> {
        let current = env::current_dir()?;
        let requested = requested.unwrap_or_else(|| Path::new("."));
        let candidate = if requested.is_absolute() {
            requested.to_owned()
        } else {
            current.join(requested)
        };
        let directory = fs::canonicalize(&candidate).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "session path does not resolve to an existing directory: {} ({error})",
                    candidate.display()
                ),
            )
        })?;
        if !directory.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("session path is not a directory: {}", directory.display()),
            ));
        }
        Ok(directory)
    }

    pub(crate) fn rename(old: &str, new: &str, format: OutputFormat) -> io::Result<()> {
        validate_session_name(new)?;
        let path = socket_path();
        require_running(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let session = find_session(list_sessions(&mut client)?, old)?;
        match client.request(ClientRequest::RenameSession {
            session: session.id,
            name: new.to_owned(),
        })? {
            ControlResult::Renamed { session } => {
                if format == OutputFormat::Json {
                    return print_json(&serde_json::json!({
                        "renamed": true,
                        "old_name": old,
                        "session": session_json(&session),
                    }));
                }
                println!("Renamed '{old}' to '{}'.", session.name);
                Ok(())
            }
            _ => Err(io::Error::other(
                "resident returned the wrong rename response",
            )),
        }
    }

    pub(crate) fn list(format: OutputFormat) -> io::Result<()> {
        let path = socket_path();
        if !probe_server(&path)? {
            if format == OutputFormat::Json {
                return print_json(&Vec::<serde_json::Value>::new());
            }
            println!("No sessions; the TurtleTap resident is stopped.");
            return Ok(());
        }
        let mut client = SessionClient::connect(&path)?;
        let sessions = list_sessions(&mut client)?;
        if format == OutputFormat::Json {
            let sessions: Vec<_> = sessions.iter().map(session_json).collect();
            return print_json(&sessions);
        }
        if sessions.is_empty() {
            println!("No sessions.");
            return Ok(());
        }
        println!(
            "{:<24}  {:<8}  {:>7}  LAST ACTIVE",
            "NAME", "STATE", "CLIENTS"
        );
        for session in sessions {
            println!(
                "{:<24}  {:<8}  {:>7}  {}",
                truncate_columns(&session.name, 24),
                if session.driver.is_some() {
                    "driven"
                } else {
                    "idle"
                },
                session.attached_clients,
                session
                    .last_event_at
                    .map_or_else(|| "never".to_owned(), relative_age),
            );
        }
        Ok(())
    }

    pub(crate) fn start(format: OutputFormat) -> io::Result<()> {
        let path = socket_path();
        let started = ensure_started(&path)?;
        if format == OutputFormat::Json {
            return print_json(&serde_json::json!({
                "resident": "running",
                "started": started,
                "socket": path,
                "state_dir": state_dir(&path),
            }));
        }
        println!(
            "TurtleTap resident {}.",
            if started {
                "started"
            } else {
                "is already running"
            }
        );
        Ok(())
    }

    pub(crate) fn status(format: OutputFormat) -> io::Result<()> {
        let path = socket_path();
        if !probe_server(&path)? {
            if format == OutputFormat::Json {
                return print_json(&serde_json::json!({
                    "resident": "stopped",
                    "pid": null,
                    "leader": null,
                    "sessions": [],
                }));
            }
            println!("Resident: stopped");
            return Ok(());
        }
        let mut client = SessionClient::connect(&path)?;
        let result = client.request(ClientRequest::Status)?;
        let ControlResult::Status {
            pid,
            leader,
            sessions,
        } = result
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident returned the wrong status response",
            ));
        };
        if format == OutputFormat::Json {
            let sessions: Vec<_> = sessions.iter().map(session_json).collect();
            return print_json(&serde_json::json!({
                "resident": "running",
                "pid": pid,
                "leader": leader,
                "sessions": sessions,
            }));
        }
        println!("Resident: running");
        println!("PID: {pid}");
        println!("Leader: {leader}");
        println!("Sessions: {}", sessions.len());
        for session in sessions {
            println!(
                "  {} — {} attached client{}, {}",
                session.name,
                session.attached_clients,
                if session.attached_clients == 1 {
                    ""
                } else {
                    "s"
                },
                if session.driver.is_some() {
                    "driven"
                } else {
                    "idle"
                }
            );
        }
        Ok(())
    }

    pub(crate) fn doctor(format: OutputFormat) -> io::Result<()> {
        let socket = socket_path();
        let state = state_dir(&socket);
        let config = crate::settings::active_path();
        let config_path_error = config.as_ref().err().map(ToString::to_string);
        let config = config.ok();
        let config_result = crate::settings::shell_config("TurtleTap");
        let config_valid = config_result.is_ok();
        let config_error = config_result.err().map(|error| error.to_string());
        let resident = probe_server(&socket)?;
        let terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
        let unix = cfg!(unix);
        if format == OutputFormat::Json {
            return print_json(&serde_json::json!({
                "platform": {
                    "unix": unix,
                    "persistent_sessions_supported": unix,
                },
                "terminal": {
                    "interactive": terminal,
                    "stdin": io::stdin().is_terminal(),
                    "stdout": io::stdout().is_terminal(),
                },
                "config": {
                    "path": config,
                    "valid": config_valid,
                    "error": config_error.or(config_path_error),
                },
                "resident": {
                    "running": resident,
                    "socket": socket,
                    "state_dir": state,
                },
                "version": env!("CARGO_PKG_VERSION"),
            }));
        }
        println!("TurtleTap doctor");
        println!(
            "Platform: {}",
            if unix {
                "Unix (supported)"
            } else {
                "unsupported"
            }
        );
        println!(
            "Terminal: {}",
            if terminal {
                "interactive"
            } else {
                "not interactive"
            }
        );
        let config_label = config.as_ref().map_or_else(
            || "unresolved".to_owned(),
            |path| path.display().to_string(),
        );
        println!(
            "Configuration: {config_label} ({})",
            if config_valid { "valid" } else { "invalid" }
        );
        if let Some(error) = config_error.or(config_path_error) {
            println!("Configuration error: {error}");
        }
        println!("Resident: {}", if resident { "running" } else { "stopped" });
        println!("Socket: {}", socket.display());
        println!("State: {}", state.display());
        if !terminal {
            println!("Hint: TUI commands require both stdin and stdout to be terminals.");
        }
        Ok(())
    }

    pub(crate) fn print_json(value: &impl Serialize) -> io::Result<()> {
        println!("{}", serde_json::to_string(value).map_err(protocol_error)?);
        Ok(())
    }

    pub(crate) fn stop(format: OutputFormat) -> io::Result<()> {
        let path = socket_path();
        if !probe_server(&path)? {
            if format == OutputFormat::Json {
                return print_json(&serde_json::json!({
                    "resident": "stopped",
                    "changed": false,
                    "sessions_preserved": true,
                }));
            }
            println!("TurtleTap resident is already stopped; durable sessions are preserved.");
            return Ok(());
        }
        let mut client = SessionClient::connect(&path)?;
        let pid = match client.request(ClientRequest::Status)? {
            ControlResult::Status { pid, .. } => pid,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resident returned the wrong status response before shutdown",
                ));
            }
        };
        let _ = client.request(ClientRequest::StopLeader)?;
        drop(client);
        let deadline = Instant::now() + START_TIMEOUT;
        while (path.exists() || process_exists(pid)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if path.exists() || process_exists(pid) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "resident acknowledged shutdown but did not exit",
            ));
        }
        if format == OutputFormat::Json {
            return print_json(&serde_json::json!({
                "resident": "stopped",
                "changed": true,
                "sessions_preserved": true,
            }));
        }
        println!("TurtleTap resident stopped; durable sessions were preserved.");
        Ok(())
    }

    pub(crate) fn delete_session(
        name: &str,
        yes: bool,
        no_input: bool,
        deprecated_stop: bool,
        format: OutputFormat,
    ) -> io::Result<()> {
        let path = socket_path();
        require_running(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let session = find_session(list_sessions(&mut client)?, name)?;
        if deprecated_stop && format == OutputFormat::Human {
            eprintln!(
                "Warning: 'turtletap stop <name>' is deprecated because it deletes durable state; use 'turtletap delete <name>'."
            );
        }
        if !yes {
            confirm(
                &format!(
                    "Delete session '{}' and all of its durable transcript, history, and settings?",
                    session.name
                ),
                no_input,
            )?;
        }
        let _ = client.request(ClientRequest::StopSession {
            session: session.id,
        })?;
        if format == OutputFormat::Json {
            return print_json(&serde_json::json!({
                "deleted": true,
                "session": session_json(&session),
            }));
        }
        println!("Deleted session '{}' and its durable state.", session.name);
        Ok(())
    }

    pub(crate) fn serve(path: PathBuf) -> io::Result<()> {
        supervisor::validate_socket_path(&path)?;
        let state_dir = state_dir(&path);
        let mut config =
            ResidentHostConfig::new(&path, state_dir.clone(), env!("CARGO_PKG_VERSION"))
                .with_initial_session(DEFAULT_SESSION);
        config.tick_rate = SERVER_TICK;
        config.outbound_capacity = OUTBOUND_CAPACITY;
        config.event_history = EVENT_HISTORY;
        config.durability = Durability::Flush;
        blocking::serve(ResidentHost::new(
            ShellApplication::new(state_dir, config.durability),
            TokioRuntime,
            TokioUnixTransport,
            config,
        ))
    }

    pub(crate) fn attach_path(
        path: &Path,
        name: &str,
        mode: AttachmentMode,
        force: bool,
        options: InteractiveOptions,
    ) -> io::Result<()> {
        attach_all(path, InitialTarget::Named { name, mode, force }, options)
    }

    fn attach_all(
        path: &Path,
        target: InitialTarget<'_>,
        options: InteractiveOptions,
    ) -> io::Result<()> {
        let use_async =
            env::var_os("TURTLETAP_ASYNC_ATTACH").as_deref() != Some(std::ffi::OsStr::new("0"));
        let reason = if use_async {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(attach_all_async(path, target, options))?
        } else {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            let mut shell = runtime.block_on(build_shell(path, target, options))?;
            shell.attach()?
        };
        print_detach_summary(target, reason);
        Ok(())
    }

    async fn attach_all_async(
        path: &Path,
        target: InitialTarget<'_>,
        options: InteractiveOptions,
    ) -> io::Result<turtletap::ExitReason> {
        let mut shell = build_shell(path, target, options).await?;
        shell.attach_async().await
    }

    async fn build_shell(
        path: &Path,
        target: InitialTarget<'_>,
        options: InteractiveOptions,
    ) -> io::Result<Shell> {
        let mut config = crate::settings::shell_config("TurtleTap")?;
        if options.no_color {
            config.theme = config.theme.without_color();
        }
        let bindings = config.bindings.clone();
        let open_sessions = open_sessions();
        let (sessions, dashboard_handle) = async_client::connect_controller(path).await?;
        let target_session = match target {
            InitialTarget::Named { name, .. } => Some(
                sessions
                    .iter()
                    .find(|session| session.name == name)
                    .cloned()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("unknown resident session '{name}'"),
                        )
                    })?,
            ),
            InitialTarget::Overview => None,
        };
        let dashboard = SessionDashboard::connect_async(
            path,
            config.theme.clone(),
            bindings.clone(),
            open_sessions.clone(),
            sessions,
            dashboard_handle,
            options.no_color,
        );
        let mut shell = Shell::new(config).with_pulse_enabled(!options.reduced_motion);
        let _dashboard_id = shell.add_surface(dashboard);
        if let Some(session) = target_session {
            let InitialTarget::Named { mode, force, .. } = target else {
                unreachable!("only named targets carry a session");
            };
            let (summary, instance, lease, snapshot, handle) = async_client::connect_and_attach(
                path,
                SessionSelector::Id(session.id),
                mode,
                force,
            )
            .await?;
            let surface = RemoteSurface::attach_async(
                summary,
                instance,
                lease,
                snapshot,
                handle,
                mode,
                crate::remote::RemoteUi {
                    bindings: bindings.clone(),
                    open_sessions: open_sessions.clone(),
                },
            )?;
            let _ = shell.add_surface(surface);
        }
        Ok(shell)
    }

    pub(crate) fn require_running(path: &Path) -> io::Result<()> {
        if probe_server(path)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no TurtleTap resident is running; run 'turtletap start' first",
            ))
        }
    }

    /// Guarantees a resident is serving `path`, spawning this same binary in
    /// `__serve` mode when one must be started. Returns whether it spawned.
    pub(crate) fn ensure_started(path: &Path) -> io::Result<bool> {
        let mut config = supervisor::EnsureConfig::new(
            path,
            env!("CARGO_PKG_VERSION"),
            crate::client::CLIENT_NAME,
        );
        config.start_timeout = START_TIMEOUT;
        let outcome = supervisor::ensure_leader(config, |socket| {
            Command::new(env::current_exe()?)
                .arg("__serve")
                .arg(socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
        })?;
        Ok(outcome == supervisor::EnsureOutcome::Spawned)
    }

    pub(crate) fn probe_server(path: &Path) -> io::Result<bool> {
        supervisor::probe(path, env!("CARGO_PKG_VERSION"), crate::client::CLIENT_NAME)
    }

    pub(crate) fn socket_path() -> PathBuf {
        if let Some(path) = env::var_os("TURTLETAP_SOCKET").filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
        supervisor::default_socket_path("turtletap")
    }

    pub(crate) fn state_dir(socket: &Path) -> PathBuf {
        if let Some(path) = env::var_os("TURTLETAP_STATE_DIR").filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
        supervisor::default_state_dir(socket)
    }

    fn interactive_preflight(options: InteractiveOptions) -> io::Result<()> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "an interactive terminal is required for the TurtleTap dashboard and session views",
            ));
        }
        let mut config = crate::settings::shell_config("TurtleTap")?;
        if options.no_color {
            config.theme = config.theme.without_color();
        }
        let _ = config;
        Ok(())
    }

    fn list_sessions(
        client: &mut SessionClient,
    ) -> io::Result<Vec<turtletap::resident::SessionSummary>> {
        match client.request(ClientRequest::ListSessions)? {
            ControlResult::Sessions { sessions } => Ok(sessions),
            _ => Err(io::Error::other(
                "resident returned the wrong list response",
            )),
        }
    }

    fn find_session(
        sessions: Vec<turtletap::resident::SessionSummary>,
        name: &str,
    ) -> io::Result<turtletap::resident::SessionSummary> {
        sessions
            .into_iter()
            .find(|session| session.name == name.trim())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("unknown resident session '{}'", name.trim()),
                )
            })
    }

    fn validate_session_name(name: &str) -> io::Result<()> {
        if name.trim().is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session names must be 1-64 printable characters without control characters",
            ));
        }
        Ok(())
    }

    fn session_json(session: &turtletap::resident::SessionSummary) -> serde_json::Value {
        serde_json::json!({
            "id": session.id,
            "name": session.name,
            "driver": session.driver,
            "attached_clients": session.attached_clients,
            "last_event_at": session.last_event_at,
            "digest": session.digest,
        })
    }

    fn confirm(prompt: &str, no_input: bool) -> io::Result<()> {
        if no_input || !io::stdin().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{prompt} Pass '--yes' to confirm without prompting."),
            ));
        }
        eprint!("{prompt} [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "operation cancelled; no state was changed",
            ))
        }
    }

    fn print_detach_summary(target: InitialTarget<'_>, reason: turtletap::ExitReason) {
        match (target, reason) {
            (InitialTarget::Named { name, .. }, turtletap::ExitReason::Detached) => {
                println!(
                    "Detached from '{}'; the session remains available. Reattach with: turtletap attach {}",
                    name,
                    shell_word(name)
                );
            }
            (InitialTarget::Overview, turtletap::ExitReason::Detached) => {
                println!("Detached from TurtleTap; resident sessions remain available.");
            }
            (_, turtletap::ExitReason::NoSurfaces) => {
                println!("Closed TurtleTap; resident sessions remain available.");
            }
        }
    }

    fn shell_word(value: &str) -> String {
        if value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        }) {
            value.to_owned()
        } else {
            format!("'{}'", value.replace('\'', "'\"'\"'"))
        }
    }

    fn truncate_columns(value: &str, width: usize) -> String {
        let count = value.chars().count();
        if count <= width {
            return value.to_owned();
        }
        let mut output: String = value.chars().take(width.saturating_sub(1)).collect();
        output.push('…');
        output
    }

    fn relative_age(millis: u64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let seconds = now.saturating_sub(millis) / 1_000;
        if seconds < 60 {
            format!("{seconds}s ago")
        } else if seconds < 3_600 {
            format!("{}m ago", seconds / 60)
        } else if seconds < 86_400 {
            format!("{}h ago", seconds / 3_600)
        } else {
            format!("{}d ago", seconds / 86_400)
        }
    }

    fn process_exists(pid: u32) -> bool {
        let exists = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !exists {
            return false;
        }
        Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map_or(true, |output| {
                !output.status.success()
                    || !String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .starts_with('Z')
            })
    }

    pub(crate) fn is_detach_command(line: &str) -> bool {
        matches!(split_command(line).0, ":quit" | ":detach" | "exit")
    }
}

#[cfg(unix)]
pub(crate) use imp::*;

#[cfg(not(unix))]
pub(crate) fn open(options: InteractiveOptions) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "an interactive terminal is required for the TurtleTap command shell",
        ));
    }
    let mut config = crate::settings::shell_config("TurtleTap")?;
    if options.no_color {
        config.theme = config.theme.without_color();
    }
    let mut shell = turtletap::Shell::new(config);
    shell.add_surface(super::CommandSurface::new()?);
    let shell = shell.with_pulse_enabled(!options.reduced_motion);
    let mut shell = shell;
    let _reason = shell.attach()?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn attach_named(_name: &str, _options: InteractiveOptions) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn view(_name: &str, _options: InteractiveOptions) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn take(
    _name: &str,
    _yes: bool,
    _no_input: bool,
    _options: InteractiveOptions,
) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn create(
    _name: &str,
    _requested_directory: Option<&std::path::Path>,
    _no_attach: bool,
    _options: InteractiveOptions,
    _format: OutputFormat,
) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn rename(_old: &str, _new: &str, _format: OutputFormat) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn list(_format: OutputFormat) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn start(_format: OutputFormat) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn status(_format: OutputFormat) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn stop(_format: OutputFormat) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn delete_session(
    _name: &str,
    _yes: bool,
    _no_input: bool,
    _deprecated_stop: bool,
    _format: OutputFormat,
) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn doctor(format: OutputFormat) -> io::Result<()> {
    if format == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "platform": {
                    "unix": false,
                    "persistent_sessions_supported": false,
                },
                "version": env!("CARGO_PKG_VERSION"),
            })
        );
    } else {
        println!("Persistent TurtleTap sessions require Unix on this release.");
    }
    Ok(())
}
#[cfg(not(unix))]
pub(crate) fn print_json(value: &impl serde::Serialize) -> io::Result<()> {
    println!(
        "{}",
        serde_json::to_string(value).map_err(io::Error::other)?
    );
    Ok(())
}
#[cfg(not(unix))]
pub(crate) fn serve(_path: PathBuf) -> io::Result<()> {
    unsupported()
}

#[cfg(not(unix))]
fn unsupported() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "resident sessions require a Unix platform",
    ))
}
