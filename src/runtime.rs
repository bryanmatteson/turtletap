use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crossterm::event::{self, Event};
use ratatui::{Frame, layout::Rect};

use crate::terminal::TerminalSession;

static ATTACHED: AtomicBool = AtomicBool::new(false);

/// Terminal mechanics shared by product-owned applications and TurtleTap's shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TerminalConfig {
    /// Periodic update frequency while no terminal input arrives.
    pub tick_rate: Duration,
    /// Whether terminal mouse events are captured.
    pub mouse_capture: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            tick_rate: Duration::from_millis(100),
            mouse_capture: false,
        }
    }
}

impl TerminalConfig {
    /// Creates terminal-friendly defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tick_rate: Duration::from_millis(100),
            mouse_capture: false,
        }
    }

    /// Replaces the idle tick rate.
    #[must_use]
    pub const fn with_tick_rate(mut self, tick_rate: Duration) -> Self {
        self.tick_rate = tick_rate;
        self
    }

    /// Enables or disables mouse capture.
    #[must_use]
    pub const fn with_mouse_capture(mut self, enabled: bool) -> Self {
        self.mouse_capture = enabled;
        self
    }
}

/// Events delivered by the terminal runtime without shell interpretation.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RuntimeEvent {
    /// A raw Crossterm terminal event.
    Terminal(Event),
    /// A periodic application update opportunity.
    Tick(Duration),
}

/// One application's requested runtime transition.
#[non_exhaustive]
pub enum RuntimeAction<E> {
    /// Visible state did not change.
    Ignored,
    /// Redraw the application.
    Redraw,
    /// Clear Ratatui's prior frame and redraw the application.
    ClearAndRedraw,
    /// Restore the terminal and return this application-defined outcome.
    Exit(E),
}

/// A same-thread terminal application.
///
/// This contract deliberately has no `Send` bound. FFI interpreters, arenas,
/// `Rc` state, and other thread-affine products can use the runtime directly.
pub trait TerminalApplication {
    /// Value returned when the application exits.
    type Exit;

    /// Draws into the complete terminal viewport.
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect);

    /// Handles raw input, resize, focus, paste, or a periodic tick.
    fn handle(&mut self, event: RuntimeEvent) -> RuntimeAction<Self::Exit>;

    /// Requests an application-specific tick interval.
    fn poll_interval(&self) -> Option<Duration> {
        None
    }
}

/// Runs same-thread terminal applications with panic-safe terminal restoration.
pub struct TerminalRuntime {
    config: TerminalConfig,
}

impl TerminalRuntime {
    /// Creates a runtime from terminal-only configuration.
    #[must_use]
    pub const fn new(config: TerminalConfig) -> Self {
        Self { config }
    }

    /// Attaches one application until it returns [`RuntimeAction::Exit`].
    pub fn run<A: TerminalApplication>(&mut self, application: &mut A) -> io::Result<A::Exit> {
        let _lease = AttachLease::acquire()?;
        let mut session = TerminalSession::enter(self.config.mouse_capture)?;
        let mut previous_tick = Instant::now();
        let mut dirty = true;
        let mut clear = false;

        loop {
            if clear {
                session.clear_frame()?;
                clear = false;
                dirty = true;
            }
            if dirty {
                session
                    .terminal()
                    .draw(|frame| application.render(frame, frame.area()))?;
                dirty = false;
            }

            let tick_rate = effective_tick_rate(self.config.tick_rate, application.poll_interval());
            let elapsed = previous_tick.elapsed();
            let action = if elapsed >= tick_rate {
                previous_tick = Instant::now();
                application.handle(RuntimeEvent::Tick(elapsed))
            } else {
                let timeout = tick_rate.saturating_sub(elapsed);
                if event::poll(timeout)? {
                    application.handle(RuntimeEvent::Terminal(event::read()?))
                } else {
                    let elapsed = previous_tick.elapsed();
                    previous_tick = Instant::now();
                    application.handle(RuntimeEvent::Tick(elapsed))
                }
            };

            match action {
                RuntimeAction::Ignored => {}
                RuntimeAction::Redraw => dirty = true,
                RuntimeAction::ClearAndRedraw => clear = true,
                RuntimeAction::Exit(outcome) => return Ok(outcome),
            }
        }
    }
}

fn effective_tick_rate(configured: Duration, requested: Option<Duration>) -> Duration {
    requested
        .unwrap_or(configured)
        .min(configured)
        .max(Duration::from_millis(1))
}

#[derive(Debug)]
pub(crate) struct AttachLease;

impl AttachLease {
    pub(crate) fn acquire() -> io::Result<Self> {
        ATTACHED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "already_attached: another TurtleTap runtime owns terminal input",
                )
            })
    }
}

impl Drop for AttachLease {
    fn drop(&mut self) {
        ATTACHED.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, sync::Mutex};

    use super::*;

    static LEASE_TEST: Mutex<()> = Mutex::new(());

    struct LocalApplication {
        _thread_affine: Rc<()>,
    }

    impl TerminalApplication for LocalApplication {
        type Exit = ();

        fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect) {}

        fn handle(&mut self, _event: RuntimeEvent) -> RuntimeAction<Self::Exit> {
            RuntimeAction::Exit(())
        }
    }

    #[test]
    fn terminal_application_accepts_non_send_state() {
        let application = LocalApplication {
            _thread_affine: Rc::new(()),
        };
        fn accepts_application(_: &impl TerminalApplication<Exit = ()>) {}
        accepts_application(&application);
    }

    #[test]
    fn runtime_lease_rejects_overlap_and_recovers_after_drop() {
        let _serial = LEASE_TEST
            .lock()
            .expect("lease test lock should be healthy");
        let first = AttachLease::acquire().expect("first runtime should acquire input");
        let error = AttachLease::acquire().expect_err("overlapping runtime must fail");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("already_attached"));

        drop(first);

        AttachLease::acquire().expect("released input should be reusable");
    }

    #[test]
    fn runtime_lease_is_released_during_panic_unwinding() {
        let _serial = LEASE_TEST
            .lock()
            .expect("lease test lock should be healthy");
        let panic = std::panic::catch_unwind(|| {
            let _lease = AttachLease::acquire().expect("runtime should acquire input");
            panic!("injected application panic");
        });
        assert!(panic.is_err());

        AttachLease::acquire().expect("unwinding should release terminal ownership");
    }

    #[test]
    fn application_ticks_can_accelerate_but_not_slow_the_runtime() {
        let configured = Duration::from_millis(100);

        assert_eq!(effective_tick_rate(configured, None), configured);
        assert_eq!(
            effective_tick_rate(configured, Some(Duration::from_millis(20))),
            Duration::from_millis(20)
        );
        assert_eq!(
            effective_tick_rate(configured, Some(Duration::from_secs(1))),
            configured
        );
        assert_eq!(
            effective_tick_rate(Duration::ZERO, Some(Duration::ZERO)),
            Duration::from_millis(1)
        );
    }
}
