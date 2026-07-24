//! Typed command-line grammar and generated documentation surfaces.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::commands::OutputFormat;

#[derive(Debug, Parser)]
#[command(
    name = "turtletap",
    version,
    about = "Persistent, reconnectable terminal command sessions",
    long_about = "TurtleTap hosts durable command sessions in a terminal dashboard.\n\
                  Persistent sessions currently require Unix. Child commands are non-interactive;\n\
                  stdin is closed so TurtleTap remains the sole owner of terminal input.",
    after_help = "Environment:\n  \
                  TURTLETAP_CONFIG     Explicit .kdl or .toml configuration\n  \
                  TURTLETAP_SOCKET     Explicit resident socket path\n  \
                  TURTLETAP_STATE_DIR  Explicit durable state directory\n  \
                  NO_COLOR             Disable color styling\n\n\
                  In-session commands:\n  \
                  :help, :queue, :cancel, :add, :remove, :commands,\n  \
                  cd, export, unset, alias, unalias\n\n\
                  Run `turtletap doctor` to inspect resolved paths and terminal support."
)]
pub(crate) struct Cli {
    /// Override human/JSON output for non-interactive commands.
    #[arg(short = 'f', long, global = true, value_enum)]
    pub(crate) format: Option<OutputFormat>,

    /// Disable terminal color styling.
    #[arg(long, global = true)]
    pub(crate) no_color: bool,

    /// Disable ambient animation.
    #[arg(long, global = true)]
    pub(crate) reduced_motion: bool,

    /// Never prompt; fail unless a required decision is supplied by a flag.
    #[arg(long, global = true)]
    pub(crate) no_input: bool,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Open the dashboard, starting the resident when needed.
    Open,
    /// Attach to a resident session as its driver.
    Attach {
        /// Session name.
        #[arg(default_value = "default")]
        name: String,
    },
    /// Observe a resident session without mutation authority.
    View { name: String },
    /// Replace the current driver and attach with mutation authority.
    Take {
        name: String,
        /// Confirm replacement without prompting.
        #[arg(short, long)]
        yes: bool,
    },
    /// Create a durable named session.
    #[command(name = "new", alias = "create")]
    New {
        name: String,
        /// Create the session without opening the TUI.
        #[arg(long)]
        no_attach: bool,
    },
    /// Rename a durable session.
    Rename {
        #[arg(value_name = "OLD-NAME")]
        old: String,
        #[arg(value_name = "NEW-NAME")]
        new: String,
    },
    /// List resident sessions.
    List,
    /// Start the resident without attaching.
    Start,
    /// Show resident health and session status.
    Status,
    /// Stop the resident leader while preserving sessions.
    Stop {
        /// Deprecated compatibility form; deletes this session after confirmation.
        #[arg(hide = true)]
        name: Option<String>,
        /// Confirm deletion without prompting when NAME is supplied.
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete a session and all of its durable state.
    Delete {
        name: String,
        /// Confirm deletion without prompting.
        #[arg(short, long)]
        yes: bool,
    },
    /// Show or manage KDL/TOML settings.
    #[command(alias = "settings")]
    Config(ConfigArgs),
    /// Inspect terminal support, resolved paths, configuration, and resident health.
    Doctor,
    /// Generate shell completion definitions.
    Completions {
        #[arg(value_enum, value_name = "FORMAT")]
        shell: Shell,
    },
    /// Generate a roff manual page on stdout.
    Man,
    #[command(name = "__serve", hide = true)]
    Serve { socket: PathBuf },
    #[cfg(unix)]
    #[command(name = "__shell-worker", hide = true)]
    ShellWorker {
        session: String,
        socket: PathBuf,
        state: PathBuf,
    },
    #[command(name = "__latency_probe", hide = true)]
    LatencyProbe { nonce: String },
}

#[derive(Debug, Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    pub(crate) action: Option<ConfigAction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum ConfigFormat {
    Kdl,
    Toml,
}

impl ConfigFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Kdl => "kdl",
            Self::Toml => "toml",
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigAction {
    /// Print resolved settings, optionally translating formats.
    Show {
        #[arg(value_enum)]
        config_format: Option<ConfigFormat>,
    },
    /// Print the active configuration path.
    Path,
    /// Validate the active configuration.
    Check,
    /// Create a commented starter configuration.
    Init {
        #[arg(value_enum, value_name = "FORMAT", default_value_t = ConfigFormat::Kdl)]
        config_format: ConfigFormat,
        /// Make this format active when another candidate exists.
        #[arg(long)]
        activate: bool,
    },
    /// Open the active configuration in $VISUAL or $EDITOR and validate it on return.
    Edit,
    /// Validate settings and explain how they take effect.
    Reload,
    /// Interactively remap shortcuts by pressing the desired keys.
    Keys,
}

pub(crate) fn command() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}
