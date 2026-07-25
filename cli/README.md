# turtletap-cli

`turtletap-cli` provides persistent, reconnectable command sessions in a terminal
dashboard. The installed binary is `turtletap`.

```console
cargo install turtletap-cli
turtletap
```

Persistent sessions require Unix. Each submitted line runs through the login shell
with closed stdin. Output streams into a durable transcript. Commands submitted
while another command runs enter a FIFO queue.

Detaching restores the host terminal without stopping the session. Reattachment
restores transcript, history, working directory, exported variables, aliases, added
commands, and output produced while detached. Journals and checkpoints recover the
same state after resident replacement.

## Commands

| Command | Result |
| --- | --- |
| `turtletap` or `turtletap open` | Start the resident when absent; open the dashboard |
| `turtletap new NAME [PATH]` | Create and attach as driver; start in PATH or the current directory |
| `turtletap new NAME [PATH] --no-attach` | Create without a TUI |
| `turtletap rename OLD NEW` | Rename durable session state |
| `turtletap attach NAME` | Attach as driver |
| `turtletap view NAME` | Attach without mutation authority |
| `turtletap take NAME` | Replace the driver after confirmation |
| `turtletap list` | List sessions and attachment state |
| `turtletap start` | Start the resident without attaching |
| `turtletap status` | Report resident and session state |
| `turtletap delete NAME` | Delete a session and its durable state after confirmation |
| `turtletap stop` | Stop the resident and preserve sessions |
| `turtletap doctor` | Report platform, terminal, configuration, and resident state |
| `turtletap completions SHELL` | Emit Bash, Elvish, Fish, PowerShell, or Zsh completions |
| `turtletap man` | Emit a roff manual |

`create` aliases `new`. `settings` aliases `config`.

One terminal owns the driver lease. Other terminals attach as viewers. Forced
takeover increments the lease epoch; input buffered by the former driver cannot
mutate the session afterward.

Interactive commands require terminal stdin and stdout. Non-interactive commands
emit human output to a terminal and JSON when stdout is redirected. `--format
human` and `--format json` select the format explicitly. JSON errors include a
stable code and message.

Destructive operations require terminal confirmation or `--yes`. `--no-input`
turns a missing decision into a usage error. Usage errors exit 2. Runtime errors
exit 1.

## Dashboard

The first screen lists resident sessions, attachment counts, last activity, and
application digest previews. Each open session occupies one keyed screen. Resident
metadata updates rename screens, close deleted sessions, and reflect driver changes.

| Default | Action |
| --- | --- |
| `Enter` | Open idle session as driver; open driven session as viewer |
| `/` | Filter sessions |
| `v` | View selected session |
| `t` | Take selected session after confirmation |
| `n` | Create a session |
| `r` | Rename selected session |
| `x` | Delete selected session after confirmation |
| `!` | Stop the resident and preserve sessions |
| `b` | Open the keybinding editor |
| `q` | Close the dashboard screen |

`Esc` on an empty command prompt and `Ctrl-\`` open the action bar. It ranks
commands for the active screen, open sessions, and shell actions. Up and Down
select a result; Enter runs it. Its `Alt` accelerators are active only while it is
open. `Ctrl-D` detaches the terminal.

In a session, `F2` releases the driver and `F3` requests a confirmed takeover.
Background output increments an unread badge until the session receives focus.

All action shortcuts are configurable. Text entry, Enter, Backspace, editing
arrows, Escape cancellation, and confirmation keys remain fixed interaction
grammar.

## Command input

The command surface provides history, multiline paste confirmation, queue
inspection, queue cancellation, completion, transcript scrollback, and process
interrupt.

| Default | Action |
| --- | --- |
| `Up` / `Down` | Command history |
| `Alt-B` / `Alt-F` | Move by word |
| `Ctrl-A` / `Ctrl-E` | Move to line boundary |
| `Ctrl-W` | Delete previous word |
| `Ctrl-U` | Delete to line start |
| `Ctrl-C` | Interrupt the active process group |
| `Ctrl-L` | Clear transcript and return to live output |
| `PageUp` / `PageDown` | Scroll one viewport |
| `Ctrl-Home` / `Ctrl-End` | Oldest output / live output |

`Alt-Left`/`Alt-Right`, `Cmd-Left`/`Cmd-Right`, `Alt-Backspace`,
`Cmd-Backspace`, and `Cmd-K` remain platform-friendly aliases.

Built-ins:

```text
:help
:queue
:cancel <number|all>
:add greet printf 'hello %s\n'
:commands
:remove greet
cd ./target
export PROFILE=release
unset PROFILE
alias ll='ls -la'
unalias ll
```

`Tab` completes built-ins and added commands. Pasted line breaks are retained and
require a second Enter before execution. Child commands receive closed stdin; an
interactive child belongs in a separate captured PTY surface.

Scroll position is per attached screen. New output does not move a screen that is
reading history; it increments the newer-lines counter. Returning to the bottom
resumes live follow.

## Configuration

TurtleTap reads KDL and TOML.

| Command | Result |
| --- | --- |
| `turtletap config` | Print resolved settings in the active format |
| `turtletap config show kdl` | Print canonical KDL |
| `turtletap config show toml` | Print canonical TOML |
| `turtletap config path` | Print only the active path |
| `turtletap config check` | Validate syntax, names, values, and binding conflicts |
| `turtletap config init [kdl\|toml]` | Create a commented starter file |
| `turtletap config init FORMAT --activate` | Select the format without overwriting an existing file |
| `turtletap config edit` | Run `$VISUAL` or `$EDITOR`, then validate |
| `turtletap config reload` | Validate the file and report its application point |
| `turtletap config keys` | Capture, review, validate, and atomically save one binding |

`TURTLETAP_CONFIG` selects an explicit `.kdl` or `.toml` file. Otherwise the active
format marker selects `config.kdl` or `config.toml`; without a marker, KDL wins when
present. Missing files use built-in defaults.

`turtletap config path` always writes one unquoted path to stdout. Its output is
stable in terminals, pipes, and command substitution; `--format` does not alter it.

Bindings are grouped by interaction context: `shell`, `leader`, `action`, `session`,
and `dashboard`. An empty list disables an action. Validation rejects duplicates,
unsupported modifiers, context-local collisions, and plain text keys in contexts
that accept text. Dashboard browse bindings permit plain keys.

Color values accept the named 16-color palette, `default`, `#rrggbb`, and
`indexed-0` through `indexed-255`. Status also uses text and symbols. `NO_COLOR` and
`--no-color` remove styling. `--reduced-motion` removes the title pulse.

Open dashboards watch the active configuration file. Valid changes update theme and
bindings. Invalid changes retain the active configuration and display the validation
error.

## Paths

- `TURTLETAP_SOCKET`: resident Unix socket.
- `TURTLETAP_STATE_DIR`: durable session storage.
- `TURTLETAP_CONFIG`: explicit configuration file.
- `XDG_CONFIG_HOME`: base directory for automatic configuration discovery.

## Verification

`cli/tests/cli.rs` executes every public command and configuration action, both
configuration formats, every completion target, output modes, confirmation rules,
aliases, exit codes, and the full non-interactive lifecycle.

`cli/tests/resident.rs` exercises the TUI through a pseudoterminal and verifies
detach, reattach, reconnect, concurrent viewers, forced takeover, deduplication,
journal recovery, checkpoint recovery, bounded framing, active-command recovery,
worker cleanup, and process-group termination.

Unit tests cover dashboard actions, key capture and review, command editing,
built-ins, queueing, scrollback, configuration parsing, live reload, activity
badges, and driver-role changes.
