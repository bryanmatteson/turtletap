# TurtleTap

TurtleTap is a reusable terminal shell for applications that need several long-lived,
session-like surfaces in one terminal. An agent conversation, embedded terminal,
questionnaire, approval flow, log stream, or debugger can all implement the same
small `Surface` trait.

TurtleTap owns the shell chrome and behavior:

- stable surface identity, focus, tabs, and a searchable command palette;
- clean attach/detach terminal lifecycle;
- direct `Ctrl-D` detach for shell-managed surfaces;
- direct `Alt` screen switching, plus a configurable `Ctrl-Space` leader that
  remains available when a surface captures input;
- background tick and resize delivery for inactive agents and PTYs;
- change-driven redraws that stay quiet while surfaces are idle;
- contextual help and status without requiring each host to reinvent navigation;
- deterministic off-screen rendering for tests.

It does **not** define what an agent, session, or questionnaire is. Those remain
host-domain objects behind `Surface` implementations.

## Add it

```toml
[dependencies]
turtletap = "0.1"
```

The package and Rust crate are both `turtletap`.

## Minimal host

```rust,no_run
use std::borrow::Cow;

use turtletap::{
    InputPolicy, Shell, ShellConfig, Surface, SurfaceAction, SurfaceEvent,
    SurfaceStatus,
};

struct AgentSurface;

impl Surface for AgentSurface {
    fn title(&self) -> Cow<'_, str> {
        "planner".into()
    }

    fn status(&self) -> SurfaceStatus {
        SurfaceStatus::Working
    }

    fn input_policy(&self) -> InputPolicy {
        InputPolicy::Shell
    }

    fn render(&mut self, frame: &mut turtletap::Frame<'_>, area: turtletap::Rect) {
        frame.render_widget("Agent output goes here", area);
    }

    fn handle(&mut self, _event: SurfaceEvent) -> SurfaceAction {
        SurfaceAction::Ignored
    }
}

let mut shell = Shell::new(ShellConfig::new("Koda"));
shell.add_surface(AgentSurface);
let reason = shell.attach()?;
println!("shell detached: {reason:?}");
# Ok::<(), std::io::Error>(())
```

`Shell::attach` borrows the shell. When the user detaches, the alternate screen and
raw mode are restored and the caller receives control with every surface still in
memory. A host can attach the same shell again. Keeping it alive after the host
process exits is deliberately a supervisor concern, not a terminal-rendering trick.

Native terminal text selection is available by default: drag across text and use
your terminal's normal copy command. TurtleTap does not capture mouse events unless
the host explicitly opts in with `ShellConfig::with_mouse_capture(true)`. Mouse-aware
surfaces can enable capture; most terminals then provide an override modifier such
as Shift or Option for native selection.

## Navigation contract

| Key | Shell-managed surface | Captured surface |
| --- | --- | --- |
| `Alt-Left` / `Alt-Right` | Previous / next surface | Previous / next surface |
| `Alt-1` … `Alt-9` | Jump to numbered surface | Jump to numbered surface |
| `Ctrl-PageUp` / `Ctrl-PageDown` | Previous / next surface | Previous / next surface |
| `PageUp` / `PageDown` | Surface scrollback | Surface scrollback |
| `Ctrl-Home` / `Ctrl-End` | Oldest output / live tail | Oldest output / live tail |
| `Ctrl-D` | Detach | Delivered to the surface |
| `Tab` / `Shift-Tab` | Next / previous surface | Delivered to the surface |
| `Ctrl-P` | Open command palette | Open command palette |
| `?` | Contextual help | Delivered to the surface |
| `Ctrl-Space d` | Detach | Detach |
| `Ctrl-Space s` | Open command palette | Open command palette |
| `Ctrl-Space n` / `Ctrl-Space p` | Next / previous | Next / previous |
| `Ctrl-Space 1` … `Ctrl-Space 9` | Jump to numbered surface | Jump to numbered surface |
| `Ctrl-Space x` | Close active surface | Close active surface |
| `Ctrl-Space ?` | Contextual help | Contextual help |

Use `InputPolicy::Captured` for an embedded PTY or editor. TurtleTap never steals
`Ctrl-D` from a captured child; the leader chord is the escape hatch back to shell
chrome.

Navigation is configurable. `Ctrl-G` remains a secondary leader by default for
backward compatibility, but the footer and contextual help teach the first active
binding rather than hardcoding either chord.

Run the included demo with:

```console
cargo run --example demo
```

## Command shell

Installing the crate also installs a ready-to-use TurtleTap shell:

```console
cargo install turtletap
turtletap
```

Enter any command to run it through your login shell. Output is streamed into the
surface, command history is available with `Up` and `Down`, and additional lines
entered while a command is running are queued in order. Child commands are
non-interactive so TurtleTap remains the sole owner of terminal input; embed a PTY
as its own captured `Surface` when a program needs interactive input.

The standalone shell is resident on Unix. `Ctrl-D` restores the terminal and leaves
the session running; `turtletap attach` later restores its transcript, history,
working directory, added commands, and any output produced while detached. Sessions
are journaled and checkpointed, so they also recover after a resident restart.

```console
turtletap                 # open the searchable resident dashboard
turtletap new build       # create and attach to a named session
turtletap rename build ci # rename durable state without recreating it
turtletap attach build    # attach as driver when available
turtletap view build      # observe without mutation authority
turtletap take build      # explicitly take the fenced driver lease
turtletap list            # list sessions and attached clients
turtletap status          # inspect the resident
turtletap stop build      # stop and delete one session
turtletap stop            # stop the leader; durable sessions remain
```

`list` and `status` default to human-readable output. Pass `--format json` for a
stable machine-readable report suitable for scripts and other tools.

One terminal holds a session's driver lease while any number of terminals may view
it. A forced takeover increments the lease epoch, so buffered input from the former
driver cannot execute afterward. `turtletap start` starts the resident without
attaching. Set `TURTLETAP_SOCKET` to use an explicit local socket path and
`TURTLETAP_STATE_DIR` to override durable storage.

The dashboard is itself screen 1. Use `/` to filter, `Enter` to open the selected
session, `v` to view, `t` to take its driver lease, and `n`, `r`, or `x` to create,
rename, or delete. Each opened session becomes another numbered screen; use
`Alt-Left` / `Alt-Right` to cycle or `Alt-1` … `Alt-9` to jump directly. Named
`attach`, `view`, `take`, and `new` commands open the dashboard alongside the
requested session, so another session is always one switch away. In a session tab,
`F2` releases the driver and `F3` takes it. Background output adds a `+N` unread
count to that screen until it is focused. When the tab row is too narrow, `‹` and
`›` mark hidden screens while the active screen stays visible. `q` closes only the
dashboard tab, `Ctrl-D` detaches the terminal, `x` deletes only the selected session
after confirmation, and `!` stops only the leader while preserving all sessions.

## Settings and keybindings

TurtleTap reads both KDL and TOML. `TURTLETAP_CONFIG` has highest priority and its
path must end in `.kdl` or `.toml`. Without that override,
`~/.config/turtletap/config.kdl` wins when present, then `config.toml`; missing
files are fine and built-in defaults remain active.

```console
turtletap config          # show resolved settings in the active format
turtletap config show toml # translate the resolved model to canonical TOML
turtletap config path     # print the active file path
turtletap config init     # create a commented KDL starter file
turtletap config init toml # create a commented TOML starter file
turtletap config check    # validate syntax, key names, and conflicts
```

Every top-level command and config action accepts `--help`; `turtletap help
<command>` is the equivalent discoverable form. Usage mistakes exit with status 2,
while runtime failures exit with status 1.

The canonical KDL shape uses properties for shell behavior and child nodes with
repeated arguments for binding lists:

```kdl
shell mouse-capture=false direct-detach=true tick-rate-ms=100

bindings {
    leaders "ctrl-space" "ctrl-g"
    palette "ctrl-p"
    next-screen "alt-right" "ctrl-pagedown"
    previous-screen "alt-left" "ctrl-pageup"
    jump-modifiers "alt"
}
```

The equivalent TOML is:

```toml
[shell]
mouse_capture = false
direct_detach = true
tick_rate_ms = 100

[bindings]
leaders = ["ctrl-space", "ctrl-g"]
palette = ["ctrl-p"]
next_screen = ["alt-right", "ctrl-pagedown"]
previous_screen = ["alt-left", "ctrl-pageup"]
jump_modifiers = ["alt"]
```

Binding lists accept `ctrl`, `alt`/`option`, `shift`, and `super`/`cmd` modifiers;
named arrows, navigation keys, `space`, `tab`, `escape`, and `F1` through `F24`
are supported. An empty list disables that action. Conflicting bindings fail fast
with the two setting names involved, and the shell footer, palette, and help overlay
always show the resolved primary chords.

## Transcript scrollback

Command screens follow live output at the bottom by default. `PageUp` and
`PageDown` move by a viewport, while `Ctrl-Home` and `Ctrl-End` jump to the oldest
output and live tail. While history is visible, new output increments the
"newer lines" counter without moving the text being read; returning to the bottom
resumes follow mode. Mouse-wheel scrolling uses the same model when
`shell.mouse_capture` is enabled. Scroll position is per screen and intentionally
ephemeral, while the transcript itself remains durable.

## Resident applications

The public `turtletap::resident` module is independent of terminal rendering. It
provides stable client, session, request, event, and lease identities; deterministic
driver and deduplication state; bounded length-prefixed framing; versioned handshake
types; checksummed journals; leader locking; a reusable `ResidentHost`; and runtime
transport contracts.

Applications implement `ResidentApplication` and `ResidentSession`. A transition
returns durable events plus follow-on effects: the host journals events and writes a
checkpoint before it runs those effects. Session reducers remain synchronous and
deterministic. External effects execute asynchronously without borrowing the actor,
then return through its mailbox for a synchronous completion transition.

Every effect has a durable `EffectId`. At-least-once effects are redriven after leader
recovery with the same identity and an incremented attempt number, allowing downstream
idempotency. At-most-once effects are never repeated after execution may have begun;
their completion instead reports `effect_outcome_unknown`. Effects run sequentially
within a session and concurrently across sessions, bounded by the host's
`max_concurrent_effects` setting. `EffectContext` carries a cooperative cancellation
signal that fires when its deadline expires, its session is deleted, or the leader
shuts down. Effects inherit the host deadline unless an `EffectRequest` overrides it.
This ordering makes accepted requests and their effect outbox recoverable across
disconnects and process death.
Storage carries independent host and application versions, replays journal records
newer than the checkpoint, and can reconstruct a corrupt checkpoint from the session
manifest plus replayable transitions. The fixture corpus under `tests/fixtures/`
freezes protocol v1 and host storage v0/v1/v2 for compatibility review.

`ResidentHost` owns election, registration, request routing, bounded client queues,
driver fencing, deduplication, persistence, reconnect cursors, and graceful shutdown.
The application owns only its command, event, snapshot, checkpoint state, and effect
types. Koda's `koda-console::resident` module is the first external implementation of
this public boundary.

Tokio is the default production adapter under `resident::runtime::tokio`. Tokio
sockets, channels, timers, and task handles do not appear in the protocol or leader
core, so another runtime can implement the `Transport`, `Connection`, `Listener`,
`Clock`, `Spawner`, and `ProcessSpawner` contracts without changing application or
wire types.

Commands added with `:add` live until the resident session is stopped:

```text
:add greet printf 'hello %s\n'
greet turtle
:commands
:remove greet
```

Use `:help` for the command list, `Tab` to complete built-ins and added commands,
`Ctrl-C` to interrupt a running process, and `Ctrl-D` on an empty prompt to detach
without stopping the session.
