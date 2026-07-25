//! Installable TurtleTap command shell.

use std::{
    env,
    ffi::OsString,
    io::{self, IsTerminal as _, Write as _},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{CommandFactory as _, Parser as _, error::ErrorKind};

mod args;
mod command;
mod commands;
mod keybindings;
mod settings;

#[cfg(unix)]
mod app;
#[cfg(unix)]
mod async_client;
#[cfg(unix)]
mod client;
#[cfg(unix)]
mod overview;
#[cfg(unix)]
mod remote;
#[cfg(unix)]
mod worker;

fn main() -> ExitCode {
    let raw: Vec<OsString> = env::args_os().collect();
    let requested_json = raw
        .iter()
        .any(|argument| argument == "--format=json" || argument == "-fjson")
        || raw.windows(2).any(|arguments| {
            (arguments[0] == "--format" || arguments[0] == "-f") && arguments[1] == "json"
        });
    let cli = match args::Cli::try_parse_from(raw) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            if requested_json {
                print_json_error("usage_error", &error.to_string(), None);
            } else {
                let _ = error.print();
            }
            return ExitCode::from(2);
        }
    };

    let command = cli.command.unwrap_or(args::Command::Open);
    let source_output = matches!(
        &command,
        args::Command::Config(args::ConfigArgs {
            action: None | Some(args::ConfigAction::Show { .. }) | Some(args::ConfigAction::Path)
        }) | args::Command::Completions { .. }
            | args::Command::Man
    );
    let format = cli.format.unwrap_or_else(|| {
        if io::stdout().is_terminal() || source_output {
            commands::OutputFormat::Human
        } else {
            commands::OutputFormat::Json
        }
    });
    let interactive = commands::InteractiveOptions {
        no_color: cli.no_color || env::var_os("NO_COLOR").is_some(),
        reduced_motion: cli.reduced_motion,
    };
    dispatch(command, format, interactive, cli.no_input)
}

fn dispatch(
    command: args::Command,
    format: commands::OutputFormat,
    interactive: commands::InteractiveOptions,
    no_input: bool,
) -> ExitCode {
    use args::Command;
    let interactive_command = matches!(
        &command,
        Command::Open
            | Command::Attach { .. }
            | Command::View { .. }
            | Command::Take { .. }
            | Command::New {
                no_attach: false,
                ..
            }
            | Command::Config(args::ConfigArgs {
                action: Some(args::ConfigAction::Keys)
            })
    );
    if interactive_command && format == commands::OutputFormat::Json {
        return usage_error(
            "interactive commands do not support JSON output",
            Some("omit '--format json' or add '--no-attach' to 'new'"),
            format,
        );
    }
    match command {
        Command::Open => finish(commands::open(interactive), format),
        Command::Attach { name } => finish(commands::attach_named(&name, interactive), format),
        Command::View { name } => finish(commands::view(&name, interactive), format),
        Command::Take { name, yes } => {
            finish(commands::take(&name, yes, no_input, interactive), format)
        }
        Command::New {
            name,
            path,
            no_attach,
        } => finish(
            commands::create(&name, path.as_deref(), no_attach, interactive, format),
            format,
        ),
        Command::Rename { old, new } => finish(commands::rename(&old, &new, format), format),
        Command::List => finish(commands::list(format), format),
        Command::Start => finish(commands::start(format), format),
        Command::Status => finish(commands::status(format), format),
        Command::Stop { name: None, .. } => finish(commands::stop(format), format),
        Command::Stop {
            name: Some(name),
            yes,
        } => finish(
            commands::delete_session(&name, yes, no_input, true, format),
            format,
        ),
        Command::Delete { name, yes } => finish(
            commands::delete_session(&name, yes, no_input, false, format),
            format,
        ),
        Command::Config(config) => config_command(config, format, interactive),
        Command::Doctor => finish(commands::doctor(format), format),
        Command::Completions { shell } => {
            let mut command = args::Cli::command();
            clap_complete::generate(shell, &mut command, "turtletap", &mut io::stdout());
            ExitCode::SUCCESS
        }
        Command::Man => {
            let manual = clap_mangen::Man::new(args::command());
            finish(
                manual.render(&mut io::stdout()),
                commands::OutputFormat::Human,
            )
        }
        Command::Serve { socket } => finish(commands::serve(socket), format),
        #[cfg(unix)]
        Command::ShellWorker {
            session: _,
            socket,
            state,
        } => finish(worker::run(socket, state), format),
        Command::LatencyProbe { nonce } => {
            finish(latency_probe(&nonce), commands::OutputFormat::Human)
        }
    }
}

fn config_command(
    config: args::ConfigArgs,
    format: commands::OutputFormat,
    interactive: commands::InteractiveOptions,
) -> ExitCode {
    use args::ConfigAction;
    let format = if matches!(&config.action, Some(ConfigAction::Path)) {
        commands::OutputFormat::Human
    } else {
        format
    };
    if matches!(config.action, Some(ConfigAction::Keys)) {
        if format == commands::OutputFormat::Json {
            return usage_error(
                "the keybinding editor does not support JSON output",
                Some("omit '--format json' to open the interactive editor"),
                format,
            );
        }
        return finish(keybindings::open(interactive), format);
    }
    if matches!(&config.action, None | Some(ConfigAction::Show { .. }))
        && format == commands::OutputFormat::Json
    {
        return usage_error(
            "'config show' emits KDL or TOML, not JSON",
            Some("use 'turtletap config show kdl' or 'turtletap config show toml'"),
            format,
        );
    }
    let arguments = match config.action {
        None => Vec::new(),
        Some(ConfigAction::Show { config_format }) => {
            let mut values = vec!["show".to_owned()];
            if let Some(format) = config_format {
                values.push(format.as_str().to_owned());
            }
            values
        }
        Some(ConfigAction::Path) => vec!["path".to_owned()],
        Some(ConfigAction::Check) => vec!["check".to_owned()],
        Some(ConfigAction::Init {
            config_format,
            activate,
        }) => {
            let mut values = vec!["init".to_owned(), config_format.as_str().to_owned()];
            if activate {
                values.push("--activate".to_owned());
            }
            values
        }
        Some(ConfigAction::Edit) => vec!["edit".to_owned()],
        Some(ConfigAction::Reload) => vec!["reload".to_owned()],
        Some(ConfigAction::Keys) => unreachable!("handled above"),
    };
    finish(settings::command(&arguments, format), format)
}

fn finish(result: io::Result<()>, format: commands::OutputFormat) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            usage_error(&error.to_string(), None, format)
        }
        Err(error) => {
            if format == commands::OutputFormat::Json {
                print_json_error(error_code(&error), &error.to_string(), suggestion(&error));
            } else {
                eprintln!("turtletap: {error}");
                if let Some(suggestion) = suggestion(&error) {
                    eprintln!("Try: {suggestion}");
                }
            }
            ExitCode::FAILURE
        }
    }
}

fn usage_error(
    message: &str,
    suggestion: Option<&str>,
    format: commands::OutputFormat,
) -> ExitCode {
    if format == commands::OutputFormat::Json {
        print_json_error("usage_error", message, suggestion);
    } else {
        eprintln!("turtletap: {message}");
        if let Some(suggestion) = suggestion {
            eprintln!("Try: {suggestion}");
        }
    }
    ExitCode::from(2)
}

fn error_code(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::AlreadyExists => "already_exists",
        io::ErrorKind::TimedOut => "timed_out",
        io::ErrorKind::Unsupported => "unsupported",
        _ => "runtime_error",
    }
}

fn suggestion(error: &io::Error) -> Option<&'static str> {
    let message = error.to_string();
    if message.contains("driven by client") {
        Some("'turtletap view <name>' to observe or 'turtletap take <name>' to replace the driver")
    } else if message.contains("no TurtleTap resident") {
        Some("'turtletap start'")
    } else if message.contains("interactive terminal") {
        Some("'turtletap start', 'turtletap list', or 'turtletap new --no-attach <name>'")
    } else {
        None
    }
}

fn print_json_error(code: &str, message: &str, suggestion: Option<&str>) {
    let value = serde_json::json!({
        "error": {
            "code": code,
            "message": message,
            "suggestion": suggestion,
        }
    });
    eprintln!("{value}");
}

fn latency_probe(nonce: &str) -> io::Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "TT_PROBE {nonce} {timestamp}")?;
    stdout.flush()
}
