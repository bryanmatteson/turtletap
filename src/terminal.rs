use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
};

pub(crate) type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

pub(crate) struct TerminalSession {
    terminal: TuiTerminal,
    mouse_capture: bool,
    keyboard_enhancement: bool,
}

impl TerminalSession {
    pub(crate) fn enter(mouse_capture: bool) -> io::Result<Self> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();
        let enter_result = if mouse_capture {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)
        } else {
            execute!(stdout, EnterAlternateScreen, Hide)
        };

        if let Err(error) = enter_result {
            restore_terminal(&mut stdout, mouse_capture, false);
            let _ = disable_raw_mode();
            return Err(error);
        }

        // Ask compatible terminals to preserve modifiers on punctuation keys.
        // Without extended reporting, terminals such as iTerm2 can deliver
        // Ctrl-` as an ordinary backtick, which is indistinguishable from
        // shell input. Unsupported terminals safely ignore this sequence.
        let keyboard_enhancement = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                restore_terminal(&mut stdout, mouse_capture, keyboard_enhancement);
                let _ = disable_raw_mode();
                return Err(error);
            }
        };

        Ok(Self {
            terminal,
            mouse_capture,
            keyboard_enhancement,
        })
    }

    pub(crate) fn terminal(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }

    /// Clears the visible viewport and invalidates Ratatui's previous frame.
    pub(crate) fn clear_frame(&mut self) -> io::Result<()> {
        clear_frame(&mut self.terminal)
    }
}

fn clear_frame<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let area = terminal.size()?.into();
    terminal.resize(area)
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let backend = self.terminal.backend_mut();
        restore_terminal(backend, self.mouse_capture, self.keyboard_enhancement);
        let _ = self.terminal.show_cursor();
    }
}

fn restore_terminal(writer: &mut impl Write, mouse_capture: bool, keyboard_enhancement: bool) {
    // Attempt each restoration independently so one failed write does not
    // prevent the remaining terminal state from being repaired.
    let _ = execute!(writer, Show);
    if mouse_capture {
        let _ = execute!(writer, DisableMouseCapture);
    }
    if keyboard_enhancement {
        let _ = execute!(writer, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(writer, LeaveAlternateScreen);
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;

    #[derive(Default)]
    struct FailFirstWrite {
        writes: usize,
        output: Vec<u8>,
    }

    impl Write for FailFirstWrite {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes == 1 {
                return Err(io::Error::other("injected write failure"));
            }
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn restoration_continues_after_an_individual_write_fails() {
        let mut writer = FailFirstWrite::default();

        restore_terminal(&mut writer, true, true);

        assert!(writer.writes >= 4);
        assert!(!writer.output.is_empty());
    }

    #[test]
    fn explicit_clear_invalidates_and_restores_the_complete_frame() {
        let backend = TestBackend::new(5, 1);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        terminal
            .draw(|frame| frame.render_widget("live", frame.area()))
            .expect("initial frame should draw");

        clear_frame(&mut terminal).expect("terminal should clear without querying input");
        terminal.backend().assert_buffer_lines(["     "]);

        terminal
            .draw(|frame| frame.render_widget("live", frame.area()))
            .expect("cleared frame should redraw");

        terminal.backend().assert_buffer_lines(["live "]);
    }
}
