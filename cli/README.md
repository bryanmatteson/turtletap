# turtletap-cli

The TurtleTap terminal shell: a ready-to-use command shell with persistent,
reconnectable sessions, built on the
[`turtletap`](https://crates.io/crates/turtletap) library. The installed binary is
named `turtletap`.

```console
cargo install turtletap-cli
turtletap
```

Enter any command to run it through your login shell. Output is streamed into the
surface, command history is available with `Up` and `Down`, and additional lines
entered while a command is running are queued in order. Child commands are
non-interactive so TurtleTap remains the sole owner of terminal input; embed a PTY
as its own captured surface when a program needs interactive input.

The shell is resident on Unix. `Ctrl-D` restores the terminal and leaves the session
running; `turtletap attach` later restores its transcript, history, working
directory, added commands, and any output produced while detached. Sessions are
journaled and checkpointed, so they also recover after a resident restart.

```console
turtletap                 # start if needed and open the dashboard
turtletap new build       # create and attach to a named session
turtletap new build --no-attach # create without opening a TUI
turtletap rename build ci # rename durable state without recreating it
turtletap attach build    # attach as driver when available
turtletap view build      # observe without mutation authority
turtletap take build      # confirm, then take the fenced driver lease
turtletap list            # list sessions and attached clients
turtletap status          # inspect the resident
turtletap delete build    # confirm, then delete one session and its durable state
turtletap stop            # stop the leader; durable sessions remain
```

Non-interactive commands use human-readable output on a terminal and stable JSON
when stdout is redirected. Pass `--format human` or `--format json` to choose
explicitly. Destructive commands require a terminal confirmation or `--yes`;
`--no-input` makes a missing decision fail instead of prompting.

One terminal holds a session's driver lease while any number of terminals may view
it. A forced takeover increments the lease epoch, so buffered input from the former
driver cannot execute afterward. `turtletap start` starts the resident without
attaching. Set `TURTLETAP_SOCKET` to use an explicit local socket path and
`TURTLETAP_STATE_DIR` to override durable storage.

Bare `turtletap` always opens the dashboard, starting the resident when needed.
Interactive commands require both stdin and stdout to be terminals and validate
configuration before starting or mutating the resident.

## Overview and sessions

The dashboard is a master-detail overview pinned as the first screen. It lists every
resident session with its status, how long it has been idle, and a short preview of
recent output. Use `/` to filter, `Enter` to open the selected session, `v` to view,
`t` to take its driver lease, and `n`, `r`, or `x` to create, rename, or delete.

Each resident session is opened at most once as another screen in the rail beside the
overview; press `Esc` on an empty prompt or `Ctrl-\`` to open the action bar, then use `Alt-Left` /
`Alt-Right` to cycle or `Alt-1` … `Alt-9` to jump directly. Named
`attach`, `view`, `take`, and `new` commands open only their requested session.
In a session, `F2` releases the driver and `F3` asks before taking it from another
terminal. Background output adds a `+N` unread count to that row until it is
focused. `q` closes only the overview, `Ctrl-D` detaches the terminal, `x` deletes only
the selected session after confirmation, and `!` stops only the leader while preserving
all sessions. Open sessions reconcile resident metadata in the background: renames
update their title, deleted sessions close, and a forced driver takeover immediately
moves the old driver into view-only mode.

Those are defaults, not fixed keys. Every TurtleTap action shortcut—global shell
actions, shell-managed keys, leader suffixes, action-bar accelerators, session
actions, and dashboard actions—is configurable under `bindings`.

The action bar is TurtleTap's command palette. Type an action or session name to
filter it and press `Enter` to run the selection. `Esc` opens it only when the
command prompt is empty; with text present, `Esc` retains its input-editing
behavior. Configured action-bar accelerators are active only while the bar is open,
so their defaults never consume Alt input intended for the session.

## Command-input keys

Inside the command input, the shell reserves a few editing chords; a captured surface
receives all of these instead.

| Key | Command input |
| --- | --- |
| `Alt-Left` / `Alt-Right` | Move by word |
| `Cmd-Left` / `Cmd-Right` | Beginning / end of line |
| `Alt-Backspace` | Delete previous word |
| `Cmd-Backspace` | Delete to beginning of line |
| `Cmd-K` / `Ctrl-L` | Clear the transcript |
| Action bar `Alt-↑` / `Alt-↓` | Scroll transcript by a viewport |
| `PageUp` / `PageDown` | Scroll transcript by a viewport (alias) |
| `Ctrl-Home` / `Ctrl-End` | Oldest output / live tail |

The table shows the default action keys; resolved help reflects configuration
changes. Terminal editing fallbacks are normalized: `Alt-B` / `Alt-F` move by word.
`Ctrl-U` defaults to the configurable delete-to-start action. `Ctrl-L` defaults to
the configurable clear action and returns scrollback to the live tail; `Cmd-K` does the same when the
terminal forwards it. If the terminal consumes `Cmd-K` and clears its own display,
press `Ctrl-/` to clear and reconstruct TurtleTap's complete frame. The `Ctrl-_`
fallback handles terminals that encode the same control byte under that key name.

Configuring either navigation chord as a global screen binding intentionally gives the
shell binding precedence.

## Session commands

Commands added with `:add`, the working directory, exported variables, and aliases
survive detach and resident restarts:

```text
:add greet printf 'hello %s\n'
greet turtle
:commands
:remove greet
cd ./target
export PROFILE=release
alias ll='ls -la'
```

Use `:help` for the command list and `Tab` to complete built-ins and added commands.
Use `:queue` to inspect commands entered while work is running and
`:cancel <number|all>` to remove queued work. Pasted line breaks are preserved;
multiline input requires a second `Enter` before it executes. Child processes receive
closed stdin, so commands that need their own interactive terminal should run in a
separate PTY surface.
By default, `Ctrl-C` interrupts a running process and `Ctrl-D` on an empty prompt
detaches without stopping the session. `Esc` on an empty prompt opens the action bar.

## Transcript scrollback

Command screens follow live output at the bottom by default. Press `Esc` on an
empty prompt or use the configured palette key to open the action bar. The defaults
use action-bar `Alt-↑` / `Alt-↓` and session `PageUp` / `PageDown` to move by a
viewport, with `Ctrl-Home` / `Ctrl-End` for the oldest output and live tail. While history
is visible, new output increments the "newer lines" counter without moving the text
being read; returning to the bottom resumes follow mode. Mouse-wheel scrolling uses
the same model when `shell.mouse_capture` is enabled. Scroll position is per screen
and intentionally ephemeral, while the transcript itself remains durable.

## Settings and keybindings

TurtleTap reads both KDL and TOML. `TURTLETAP_CONFIG` has highest priority and its
path must end in `.kdl` or `.toml`. Without that override, `config init --activate`
records the selected local format. Existing installations without that marker retain
the legacy `config.kdl`, then `config.toml` lookup. Missing files are fine and
built-in defaults remain active.

```console
turtletap config           # show resolved settings in the active format
turtletap config show toml  # translate the resolved model to canonical TOML
turtletap config path      # print the active file path
turtletap config init      # create a commented KDL starter file
turtletap config init toml  # create a commented TOML starter file
turtletap config init toml --activate # select TOML when another candidate exists
turtletap config check     # validate syntax, key names, and conflicts
turtletap config keys      # detect keypresses in an interactive remapping UI
turtletap config edit      # edit with $VISUAL or $EDITOR, then validate
turtletap config reload    # validate; open TUIs pick up changes automatically
```

If the requested file already exists, `config init <format> --activate` selects
it without overwriting it.

The keybinding editor groups actions by interaction context and shows every active
mapping. Select an action, press Enter, then press the desired key combination.
TurtleTap checks the proposed replacement for conflicts and shows a before/after
review before writing anything. Enter confirms the atomic update; Esc discards it.
The editor preserves comments in both KDL and TOML and creates the active config
from the commented starter template when no file exists. `config edit` remains the
scriptable/manual alternative for bulk changes and disabling actions with empty lists.
While TurtleTap is already open, press `B` on the sessions dashboard to open the
same editor as another surface; the dashboard hint and contextual help show the
resolved key if `dashboard_keybindings` has been remapped.

Every top-level command and config action accepts `--help`; `turtletap help
<command>` is the equivalent discoverable form. Usage mistakes exit with status 2,
while runtime failures exit with status 1.

The canonical KDL shape uses properties for shell behavior and child nodes with
repeated arguments for binding lists. This abbreviated example shows one field from
each interaction context; `turtletap config show` prints the complete resolved set:

```kdl
shell mouse-capture=false direct-detach=true tick-rate-ms=100 chrome="rail" rail-width=24 rail-narrow=5 rail-min-content=48

theme {
    chrome "white"
    muted "dark-gray"
    selected foreground="black" background="cyan"
    accent "cyan"
    working "blue"
    attention "yellow"
    failed "red"
    complete "green"
}

bindings {
    leaders "ctrl-g"
    palette "ctrl-`" "ctrl-space" "ctrl-p"
    redraw "ctrl-/" "ctrl-_"
    shell-detach "ctrl-d"
    leader-detach "d"
    action-detach "alt-d"
    session-detach "ctrl-d"
    dashboard-close "q"
}
```

`tick-rate-ms` controls the idle background cadence. Command surfaces
temporarily poll at 5 ms while work is queued or running so streamed output
appears promptly without keeping an idle shell at that rate.

The equivalent TOML is:

```toml
[shell]
mouse_capture = false
direct_detach = true
tick_rate_ms = 100
chrome = "rail"
rail_width = 24
rail_narrow = 5
rail_min_content = 48

[theme]
chrome = "white"
muted = "dark-gray"
accent = "cyan"
working = "blue"
attention = "yellow"
failed = "red"
complete = "green"

[theme.selected]
foreground = "black"
background = "cyan"

[bindings]
leaders = ["ctrl-g"]
palette = ["ctrl-`", "ctrl-space", "ctrl-p"]
redraw = ["ctrl-/", "ctrl-_"]
shell_detach = ["ctrl-d"]
leader_detach = ["d"]
action_detach = ["alt-d"]
session_detach = ["ctrl-d"]
dashboard_close = ["q"]
```

`shell.chrome` is `"rail"` (the master-detail default) or `"tabs"`. The `rail_*`
dimensions set its full width, its narrow marker-only width, and the minimum content
width below which it narrows.

Binding lists accept `ctrl`, `alt`/`option`, `shift`, and `super`/`cmd` modifiers;
named arrows, navigation keys, `space`, `tab`, `escape`, and `F1` through `F24` are
supported. Modifier-only digit groups also accept `none` for unmodified `1` through
`9`. An empty list disables that action. Conflicting bindings fail fast within the
interaction context where both keys would be active; separate contexts can reuse a
key. The shell footer, action bar, dashboard, session status, and help overlay show
the resolved chords.

Typing, Enter, Backspace, arrow-key editing/navigation, Esc cancellation and
empty-prompt action-bar entry, and Y/N confirmations are fixed interaction grammar.
They are intentionally not action bindings.

Theme colors accept the named 16-color terminal palette, `default`, `#rrggbb`, and
`indexed-0` through `indexed-255`. Status meaning remains visible through text and
symbols even when colors are customized. Open dashboards watch the active settings
file and apply valid theme and binding changes; invalid edits leave the previous
settings active and show a notice. `NO_COLOR` or `--no-color` removes color, and
`--reduced-motion` disables the ambient title pulse.

Run `turtletap doctor` to inspect platform support, terminal interactivity, resolved
paths, configuration validity, and resident health. Generate integration artifacts
with `turtletap completions <bash|elvish|fish|powershell|zsh>` and
`turtletap man`.
