# TurtleTap

TurtleTap is a reusable terminal shell for applications that need several long-lived,
session-like surfaces in one terminal. An agent conversation, embedded terminal,
questionnaire, approval flow, log stream, or debugger can all implement the same
small `Surface` trait.

TurtleTap owns the shell chrome and behavior:

- stable surface identity, focus, tabs or a master-detail rail, and a searchable action bar;
- clean attach/detach terminal lifecycle;
- configurable action shortcuts for navigation, detach, leader sequences,
  action-bar accelerators, and host-defined actions;
- an action bar opened by `Esc` on an empty prompt or `Ctrl-\``, whose Alt
  accelerators never steal shell input;
- background tick and resize delivery for inactive agents and PTYs;
- incremental redraws with only a compact liveness pulse while surfaces are idle;
- contextual help and status without requiring each host to reinvent navigation;
- deterministic off-screen rendering for tests.

It does **not** define what an agent, session, or questionnaire is. Those remain
host-domain objects behind `Surface` implementations.

Installing a ready-to-use shell built on this library is a separate crate,
[`turtletap-cli`](https://crates.io/crates/turtletap-cli).

## Add it

```toml
[dependencies]
turtletap = "0.2"
```

The default build depends only on `crossterm` and `ratatui` — just the
`Shell`/`Surface` half. Reconnectable, durable sessions live behind features:

| Feature | Adds | Pulls in |
| --- | --- | --- |
| _(default)_ | `Shell`, `Surface`, chrome, terminal lifecycle | crossterm, ratatui |
| `resident` | runtime-neutral resident core: protocol, journal, host, election | serde, uuid, semver, … |
| `tokio` | Tokio transport adapters, the blocking client, and the supervisor | tokio |

```toml
turtletap = { version = "0.2", features = ["tokio"] }
```

## Host example

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

## Chrome

`ShellConfig::with_chrome` chooses how the surface list is presented. `Chrome::Tabs`
(the default) is a horizontal strip above the active surface, suited to a handful of
long-lived surfaces. `Chrome::rail()` is a persistent vertical list beside the active
surface — master-detail — that keeps the list scannable as it grows and narrows to a
marker-only rail on tight terminals rather than reverting to tabs. Surfaces annotate
their rail row with `Surface::badge`.

## Navigation contract

These are the built-in defaults. The shell interprets configured global actions and
delivers everything else to the active
surface. A captured surface (`InputPolicy::Captured`, for an embedded PTY or editor)
receives ordinary input while the shell still reserves screen navigation and the
leader chord as the escape hatch back to chrome.

| Key | Shell-managed surface | Captured surface |
| --- | --- | --- |
| `Ctrl-D` | Detach | Delivered to the surface |
| `Tab` / `Shift-Tab` | Next / previous surface | Delivered to the surface |
| `Esc` on an empty prompt | Open TurtleTap action bar | Surface-controlled |
| `Ctrl-\`` | Open TurtleTap action bar | Open TurtleTap action bar |
| `Ctrl-/` | Clear and redraw terminal frame | Clear and redraw terminal frame |
| `?` | Contextual help | Delivered to the surface |
| Action bar `Alt-→` / `Alt-←` | Next / previous | Next / previous |
| Action bar `Alt-↓` / `Alt-↑` | Scroll down / up | Scroll down / up |
| Action bar `Alt-1` … `Alt-9` | Jump to numbered surface | Jump to numbered surface |
| Action bar `Alt-X` / `Alt-D` | Close surface / detach | Close surface / detach |
| `Ctrl-G ?` | Contextual help | Contextual help |

TurtleTap never steals the default `Ctrl-D` from a captured child. Command surfaces
explicitly opt into the fixed empty-prompt `Esc` action-bar fallback.

Every action binding in `ShellBindings` is configurable, and an empty list disables
that action. The footer, action bar, and contextual help show the resolved bindings.
Text entry, Enter, Backspace, arrow-key editing/navigation, Esc cancellation and
empty-prompt action-bar entry, and Y/N confirmations remain stable interaction
grammar rather than remappable actions.

Run the included demo with:

```console
cargo run --example demo
```

## Resident applications

The `turtletap::resident` module (feature `resident`) is independent of terminal
rendering. It provides stable client, session, request, event, and lease identities;
deterministic driver and deduplication state; bounded length-prefixed framing;
versioned handshake types; checksummed journals; leader locking; a reusable
`ResidentHost`; and runtime transport contracts.

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
types.

### Transport, blocking client, and supervision (feature `tokio`)

Tokio is the default production adapter under `resident::runtime::tokio`. Tokio
sockets, channels, timers, and task handles do not appear in the protocol or leader
core, so another runtime can implement the `Transport`, `Connection`, `Listener`,
`Clock`, `Spawner`, and `ProcessSpawner` contracts without changing application or
wire types.

`ResidentClient` is asynchronous. It retains stable identity plus every attachment's
authority and highest received event cursor, so reconnecting restores all subscribed
session IDs without replaying already received events. `resident::blocking::Client`
wraps that behavior in a current-thread runtime for terminal-driven callers and
adds timeouts, leader relaunch, and retry of an ambiguous request under its original
`RequestId` for the leader to deduplicate.
`resident::supervisor` owns the endpoint conventions and the start-up election —
`ensure_leader` reuses a running leader, replaces one older than the caller, or wins
the lock and spawns a new one, while the caller keeps only the product-specific detail
of how its resident process is launched.

A complete, runnable application is in
[`examples/resident.rs`](examples/resident.rs):

```console
cargo run --example resident --features tokio
```
