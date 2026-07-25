# TurtleTap

TurtleTap hosts multiple terminal surfaces in one attachable shell. A surface owns
its content and input policy. TurtleTap owns terminal setup, focus, chrome,
navigation, rendering, and detach.

Terminal ownership is available independently of the shell. A same-thread
`TerminalApplication` receives the complete viewport and raw terminal events
through `TerminalRuntime`; it does not need to be `Send`. `Shell` is an optional
`TerminalApplication` that adds multiplexing, chrome, bindings, and the action bar.

The workspace contains two crates:

- `turtletap`: the reusable shell and resident-session library.
- `turtletap-cli`: persistent command sessions backed by the library.

## Library

```toml
[dependencies]
turtletap = "0.2"
```

The default feature set provides `Shell`, `Surface`, tabs, the master-detail rail,
the action bar, configurable bindings, terminal restoration, background events, and
off-screen rendering. It depends on `crossterm` and `ratatui`.

| Feature | Public surface |
| --- | --- |
| `termosaic` | Semantic documents, themes, and retained Ratatui surface rendering |
| `resident` | Protocol types, framing, journals, election, host, sessions, effects |
| `async-shell` | Event-driven terminal and background surface polling |
| `tokio` | Tokio transport, blocking client, supervisor; includes `resident` and `async-shell` |

```toml
turtletap = { version = "0.2", features = ["tokio"] }
```

Termosaic 0.2.2 integrates at the surface boundary:

```toml
turtletap = { version = "0.2", features = ["termosaic"] }
```

`turtletap::termosaic::SurfaceRenderer` retains layout storage between frames,
reflows prepared semantic documents at the current content width, and paints
directly into the shell's Ratatui buffer. The complete runnable example is
`cargo run --example termosaic --features termosaic`.

## Surface example

```rust,no_run
use std::borrow::Cow;
use turtletap::{
    Frame, Rect, Shell, ShellConfig, Surface, SurfaceAction, SurfaceCommand, SurfaceEvent,
};

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

    fn commands(&self) -> Vec<SurfaceCommand> {
        vec![SurfaceCommand::new("log.refresh", "Refresh log")]
    }

    fn execute_command(&mut self, id: &str) -> SurfaceAction {
        match id {
            "log.refresh" => SurfaceAction::Consumed,
            _ => SurfaceAction::Ignored,
        }
    }
}

let mut shell = Shell::new(ShellConfig::new("Workbench"));
shell.add_surface(LogSurface);
let reason = shell.attach()?;
println!("{reason:?}");
# Ok::<(), std::io::Error>(())
```

## Product-owned application

Applications that already own navigation, chrome, commands, and focus can use the
terminal runtime directly. This path supports thread-affine state such as FFI
interpreters and `Rc` graphs.

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

let mut application = ProductApp;
TerminalRuntime::new(TerminalConfig::new()).run(&mut application)?;
# Ok::<(), std::io::Error>(())
```

`Shell::attach` restores raw mode and the alternate screen before returning. The
shell retains its surfaces and can attach again. Mouse capture is off by default, so
native terminal selection remains available. `ShellConfig::with_mouse_capture(true)`
enables surface mouse events.

`Chrome::Tabs` is the default. `Chrome::None` gives the active surface the complete
viewport while retaining shell behavior. `Chrome::rail()` keeps a persistent surface list and
narrows it to status markers when terminal width is constrained. Surface titles,
status, badges, and shortcuts feed the shell chrome and contextual help.

## Input contract

The shell applies configured bindings before delivering unhandled input to the
active surface. `InputPolicy::Captured` reserves shell navigation and leader chords
while forwarding ordinary input. `InputPolicy::Exclusive` forwards every key.

| Default | Shell-managed surface | Captured surface |
| --- | --- | --- |
| `Ctrl-D` | Detach | Forward |
| `Tab` / `Shift-Tab` | Next / previous surface | Forward |
| `Esc` on an eligible empty input | Open action bar | Surface-controlled |
| `Ctrl-\`` | Open action bar | Open action bar |
| `F5` | Clear and redraw | Clear and redraw |
| `?` | Contextual help | Forward |
| `Ctrl-G ?` | Contextual help | Contextual help |

The action bar ranks executable commands from the active surface, open surfaces,
and shell actions. Up and Down select a result; Enter runs it; `Alt-1` through
`Alt-9` select numbered surfaces. Other `Alt` accelerators navigate, scroll, close,
and detach only while the bar is open.

`ShellBindings` contains every remappable action. An empty list disables an action.
`BindingId::KEY_BINDINGS` is the stable configuration catalog. Validation rejects
unsupported modifier flags, duplicate bindings, context-local collisions, and plain
text keys in contexts that accept text.

```rust
use turtletap::{BindingId, KeyBinding, ShellConfig};

# fn configured_shell() -> Result<ShellConfig, Box<dyn std::error::Error>> {
let mut config = ShellConfig::new("Workbench");
let interrupt = "ctrl-x".parse::<KeyBinding>()?;
config
    .bindings
    .set_keys(BindingId::SessionInterrupt, vec![interrupt])?;
config.bindings.validate()?;
assert_eq!(interrupt.config_label()?, "ctrl-x");
# Ok(config)
# }
```

`KeyBinding` accepts terminal-friendly aliases such as `Cmd` and `Option`.
`config_label` emits portable `super` and `alt` labels.

## Resident sessions

`resident` is independent of terminal rendering. It provides:

- version-negotiated, length-prefixed framing with bounded frames;
- stable client, request, session, event, effect, and lease identities;
- driver fencing and request deduplication;
- checksummed journals, checkpoints, manifests, replay, and storage migration;
- leader election, reconnect cursors, bounded client queues, and graceful shutdown;
- durable at-least-once and at-most-once effects with deadlines and cancellation.

Applications implement `ResidentApplication` and `ResidentSession`. A transition
returns durable events and effect requests. The host persists the transition before
dispatching effects. Session reducers remain synchronous. Effects execute outside
the reducer and return through a completion transition.

At-least-once effects retain their `EffectId` across recovery. At-most-once effects
report an unknown outcome when execution may have started. Effects are sequential
within one session, concurrent across sessions, and bounded by
`max_concurrent_effects`.

`ResidentClient` retains attachment authority and event cursors across reconnects.
The blocking client adds timeouts, leader relaunch, and retry under the original
`RequestId`. The supervisor reuses a compatible leader, replaces an older leader,
or starts one under the endpoint lock.

The complete resident example runs with:

```console
cargo run --example resident --features tokio
```

The shell example runs with:

```console
cargo run --example demo
```

## Verification

The repository verifies the public surface at three levels:

- `tests/shell.rs` and `tests/bindings.rs`: rendering, navigation, lifecycle,
  configuration, and input contracts.
- `tests/resident_api.rs` and `tests/compatibility.rs`: public resident API, effects,
  recovery, protocol fixtures, and storage fixtures.
- `cli/tests/cli.rs` and `cli/tests/resident.rs`: installed command behavior, TUI
  interaction, worker recovery, reconnection, fencing, cleanup, and process groups.

```console
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo test --workspace --all-features --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc -p turtletap --all-features --no-deps
```
