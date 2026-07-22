# TurtleTap

TurtleTap is a reusable terminal shell for applications that need several long-lived,
session-like surfaces in one terminal. An agent conversation, embedded terminal,
questionnaire, approval flow, log stream, or debugger can all implement the same
small `Surface` trait.

TurtleTap owns the shell chrome and behavior:

- stable surface identity, focus, tabs, and a searchable-ready switcher boundary;
- clean attach/detach terminal lifecycle;
- direct `Ctrl-D` detach for shell-managed surfaces;
- a `Ctrl-G` leader that remains available when a surface captures input;
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

## Navigation contract

| Key | Shell-managed surface | Captured surface |
| --- | --- | --- |
| `Ctrl-D` | Detach | Delivered to the surface |
| `Tab` / `Shift-Tab` | Next / previous surface | Delivered to the surface |
| `Ctrl-P` | Open surface switcher | Delivered to the surface |
| `?` | Contextual help | Delivered to the surface |
| `Ctrl-G d` | Detach | Detach |
| `Ctrl-G s` | Open surface switcher | Open surface switcher |
| `Ctrl-G n` / `Ctrl-G p` | Next / previous | Next / previous |
| `Ctrl-G x` | Close active surface | Close active surface |
| `Ctrl-G ?` | Contextual help | Contextual help |

Use `InputPolicy::Captured` for an embedded PTY or editor. TurtleTap never steals
`Ctrl-D` from a captured child; the leader chord is the escape hatch back to shell
chrome.

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

One terminal holds a session's driver lease while any number of terminals may view
it. A forced takeover increments the lease epoch, so buffered input from the former
driver cannot execute afterward. `turtletap start` starts the resident without
attaching. Set `TURTLETAP_SOCKET` to use an explicit local socket path and
`TURTLETAP_STATE_DIR` to override durable storage.

The dashboard is itself a tab. Use `/` to filter, `Enter` to open the selected
session, `v` to view, `t` to take its driver lease, and `n`, `r`, or `x` to create,
rename, or delete. Each opened session becomes another tab; `Ctrl-G s` switches
between them. In a session tab, `F2` releases the driver and `F3` takes it. `q`
closes only the dashboard tab, `Ctrl-D` detaches the terminal, `x` deletes only the
selected session after confirmation, and `!` stops only the leader while preserving
all sessions.

## Resident applications

The public `turtletap::resident` module is independent of terminal rendering. It
provides stable client, session, request, event, and lease identities; deterministic
driver and deduplication state; bounded length-prefixed framing; versioned handshake
types; checksummed journals; leader locking; a reusable `ResidentHost`; and runtime
transport contracts.

Applications implement `ResidentApplication` and `ResidentSession`. A transition
returns durable events plus follow-on effects: the host journals events and writes a
checkpoint before it runs those effects. That ordering makes accepted requests
idempotent across disconnects and process death. Storage carries independent host and
application versions, replays journal records newer than the checkpoint, and can
reconstruct a corrupt checkpoint from the session manifest plus replayable events.
The fixture corpus under `tests/fixtures/` freezes protocol v1 and host storage v0/v1
for compatibility review.

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
