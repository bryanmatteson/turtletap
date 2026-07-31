<div align="center">

# 🐢 TurtleTap

**Terminal sessions that carry their shell with them.**

[![CI](https://github.com/bryanmatteson/turtletap/actions/workflows/ci.yml/badge.svg)](https://github.com/bryanmatteson/turtletap/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/turtletap.svg)](https://crates.io/crates/turtletap)
[![docs.rs](https://img.shields.io/docsrs/turtletap)](https://docs.rs/turtletap)
[![MSRV](https://img.shields.io/badge/rustc-1.88+-blue.svg)](https://blog.rust-lang.org/2025/06/26/Rust-1.88.0.html)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](https://github.com/bryanmatteson/turtletap/blob/main/LICENSE)

*A reusable terminal shell for Rust — and a CLI that keeps your commands
running after you close the lid.*

</div>

---

A turtle never loses its shell. TurtleTap gives your terminal programs the
same deal: a shell they live inside, attach to, detach from, and come back
to — with everything exactly where they left it.

It ships as two crates:

- **`turtletap`** — a library. Multiplexed surfaces, tabs, an action bar,
  configurable keybindings, and durable crash-recoverable sessions, all
  embeddable in your own binary. Think *tmux as a library*, minus the tmux.
- **`turtletap-cli`** — a binary (installed as `turtletap`). Persistent,
  reconnectable command sessions in a dashboard, built entirely on the
  library.

## The CLI in thirty seconds

```console
cargo install turtletap-cli
turtletap new deploy
```

Kick off the long migration. Close the laptop. Drop the SSH connection.
Get coffee.

```console
turtletap attach deploy
```

Everything is still there: the transcript, every line of output produced
while you were gone, your command history, working directory, exported
variables, and aliases. Sessions survive terminal death, disconnects, and
even replacement of the background resident itself — journals and
checkpoints restore the same state on recovery.

A few things the CLI does that you'd otherwise assemble from three tools
and a prayer:

- **One driver, many viewers.** One terminal holds the driver lease; others
  attach read-only with `turtletap view`. Forced takeover (`turtletap take`)
  fences the old driver so its buffered input can never mutate the session.
- **A queue, not a pile.** Commands submitted while one is running enter a
  FIFO queue you can inspect and cancel.
- **Scrollback that respects you.** New output never yanks a screen that's
  reading history — it counts unread lines until you return to live follow.
- **Human or JSON output**, stable exit codes, shell completions for five
  shells, and a `doctor` command for when things get weird.

The full command, dashboard, keybinding, and configuration reference lives
in [cli/README.md](cli/README.md). Persistent sessions require Unix.

## The library in one screen

Implement `Surface` — three required methods — and the shell handles raw
mode, the alternate screen, focus, tabs, navigation, contextual help, and
clean terminal restoration:

```rust,no_run
use std::borrow::Cow;
use turtletap::{Frame, Rect, Shell, ShellConfig, Surface, SurfaceAction, SurfaceEvent};

struct LogSurface;

impl Surface for LogSurface {
    fn title(&self) -> Cow<'_, str> {
        "build".into()
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget("build output", area);
    }

    fn handle(&mut self, _event: SurfaceEvent) -> SurfaceAction {
        SurfaceAction::Ignored
    }
}

fn main() -> std::io::Result<()> {
    let mut shell = Shell::new(ShellConfig::new("Workbench"));
    shell.add_surface(LogSurface);
    let reason = shell.attach()?; // raw mode, event loop, restoration: handled
    println!("detached: {reason:?}");
    Ok(())
}
```

`Shell::attach` returns with the terminal restored, and the shell keeps its
surfaces — call `attach` again to go back in. Try the interactive demo:

```console
cargo run --example demo
```

### Don't want the shell? Take just the turtle.

If your application already owns navigation, chrome, and focus, use
`TerminalRuntime` directly. It gives a same-thread `TerminalApplication` the
whole viewport and raw events — no `Send` bound, so thread-affine state like
FFI interpreters and `Rc` graphs is fine:

```rust,no_run
use turtletap::{
    Event, Frame, Rect, RuntimeAction, RuntimeEvent, TerminalApplication,
    TerminalConfig, TerminalRuntime,
};

struct ProductApp;

impl TerminalApplication for ProductApp {
    type Exit = ();

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget("product-owned interface", area);
    }

    fn handle(&mut self, event: RuntimeEvent) -> RuntimeAction<Self::Exit> {
        match event {
            RuntimeEvent::Terminal(Event::Key(key))
                if key.code == turtletap::KeyCode::Char('q') =>
            {
                RuntimeAction::Exit(())
            }
            RuntimeEvent::Terminal(Event::Resize(..)) | RuntimeEvent::Tick(_) => {
                RuntimeAction::Redraw
            }
            _ => RuntimeAction::Ignored,
        }
    }
}

fn main() -> std::io::Result<()> {
    TerminalRuntime::new(TerminalConfig::new()).run(&mut ProductApp)
}
```

## How it fits together

```text
┌────────────────────────────── your terminal ──────────────────────────────┐
│ TerminalRuntime — raw mode · alternate screen · input · resize · ticks    │
│ ┌────────────────────────────── Shell ──────────────────────────────────┐ │
│ │  chrome (tabs / rail / none) · action bar · bindings · focus · help   │ │
│ │  ┌───────────┐   ┌───────────┐   ┌───────────┐                        │ │
│ │  │ Surface A │   │ Surface B │   │ Surface C │   ←  your code         │ │
│ │  └───────────┘   └───────────┘   └───────────┘                        │ │
│ └───────────────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────┬──────────────────────────────────────┘
                          attach ⇅ detach, at will
┌────────────────────────────────────┴──────────────────────────────────────┐
│ Resident host (optional) — journals · checkpoints · leader election ·     │
│ durable effects · reconnect cursors — sessions outlive the terminal       │
└───────────────────────────────────────────────────────────────────────────┘
```

Every layer is usable without the ones above it. The default feature set is
just `Shell` + `Surface` on `crossterm` and `ratatui`; everything else is
opt-in:

```toml
[dependencies]
turtletap = "0.3"                                          # shell + surfaces
turtletap = { version = "0.3", features = ["tokio"] }      # + resident sessions
turtletap = { version = "0.3", features = ["termosaic"] }  # + semantic documents
```

| Feature | Adds |
| --- | --- |
| *(default)* | `Shell`, `Surface`, tabs, master–detail rail, action bar, bindings, terminal restoration, background events, off-screen rendering |
| `termosaic` | [Termosaic](https://crates.io/crates/termosaic) semantic documents with retained Ratatui rendering, re-exported as `turtletap::termosaic` and `turtletap::layout` — no separate dependency needed |
| `resident` | Runtime-neutral session building blocks: protocol, framing, journals, election, host, effects |
| `async-shell` | Event-driven terminal and background surface polling |
| `tokio` | Tokio transport, blocking client, supervisor (implies `resident` + `async-shell`) |

## Slow is smooth, smooth is durable

The `resident` layer is what lets sessions shrug off crashes, and it takes
correctness personally:

- **Write-ahead everything.** Checksummed journals, checkpoints, manifests,
  replay, and storage migration. A transition is persisted *before* its
  effects dispatch.
- **Effects with real semantics.** Durable at-least-once and at-most-once
  effects with deadlines and cancellation. At-least-once effects keep their
  `EffectId` across recovery; at-most-once effects honestly report an
  unknown outcome when execution may have started.
- **Fencing, not hoping.** Driver leases with epochs, request
  deduplication, version-negotiated bounded framing, reconnect cursors,
  leader election, and graceful shutdown.
- **Sequential where it matters.** Effects run sequentially within a
  session, concurrently across sessions, bounded by
  `max_concurrent_effects`.

Your application implements `ResidentApplication` and `ResidentSession`
with plain synchronous reducers; the host does the durability. See it run:

```console
cargo run --example resident --features tokio
```

## Keys that behave

The shell applies configured bindings first, then hands unhandled input to
the active surface. Surfaces pick a policy: `InputPolicy::Captured` keeps
shell navigation and leader chords while forwarding ordinary keys;
`InputPolicy::Exclusive` gets everything. Highlights of the defaults:
`Ctrl-D` detaches, `Tab`/`Shift-Tab` cycle surfaces, `` Ctrl-` `` opens a
ranked action bar over every executable command, and `?` opens contextual
help.

Every action is remappable through `ShellBindings`, with validation that
rejects duplicates, unsupported modifiers, and collisions before they can
ruin your evening:

```rust
use turtletap::{BindingId, KeyBinding, ShellConfig};

fn configured_shell() -> Result<ShellConfig, Box<dyn std::error::Error>> {
    let mut config = ShellConfig::new("Workbench");
    let interrupt = "ctrl-x".parse::<KeyBinding>()?;
    config
        .bindings
        .set_keys(BindingId::SessionInterrupt, vec![interrupt])?;
    config.bindings.validate()?;
    assert_eq!(interrupt.config_label()?, "ctrl-x");
    Ok(config)
}
```

`KeyBinding` speaks terminal-friendly aliases like `Cmd` and `Option` and
emits portable `super`/`alt` labels for config files.

## Built like it means it

The workspace forbids `unsafe`, denies `unwrap`, and warns on missing docs.
The public surface is verified at three levels:

- **Library** — rendering, navigation, lifecycle, bindings, and input
  contracts ([tests/shell.rs](tests/shell.rs),
  [tests/bindings.rs](tests/bindings.rs)); resident API, effects, recovery,
  and protocol/storage fixtures ([tests/resident_api.rs](tests/resident_api.rs),
  [tests/compatibility.rs](tests/compatibility.rs)).
- **CLI** — every public command, both config formats, output modes, and
  exit codes ([cli/tests/cli.rs](cli/tests/cli.rs)).
- **End-to-end** — the TUI driven through a real pseudoterminal: detach,
  reattach, concurrent viewers, forced takeover, journal and checkpoint
  recovery, worker cleanup, and process-group termination
  ([cli/tests/resident.rs](cli/tests/resident.rs)).

```console
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo test --workspace --all-features --all-targets
```

## License

MIT — see [LICENSE](https://github.com/bryanmatteson/turtletap/blob/main/LICENSE).

*Slow and steady wins the race. Especially when the race crashes halfway
through and has to recover from a checkpoint.*
