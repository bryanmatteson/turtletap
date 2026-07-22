use std::io::{self, Stdout, Write};

use crossterm::{
    cursor::{Hide, Show},
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub(crate) type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

pub(crate) struct TerminalSession {
    terminal: TuiTerminal,
    mouse_capture: bool,
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
            restore_terminal(&mut stdout, mouse_capture);
            let _ = disable_raw_mode();
            return Err(error);
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                restore_terminal(&mut stdout, mouse_capture);
                let _ = disable_raw_mode();
                return Err(error);
            }
        };

        Ok(Self {
            terminal,
            mouse_capture,
        })
    }

    pub(crate) fn terminal(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let backend = self.terminal.backend_mut();
        restore_terminal(backend, self.mouse_capture);
        let _ = self.terminal.show_cursor();
    }
}

fn restore_terminal(writer: &mut impl Write, mouse_capture: bool) {
    // Attempt each restoration independently so one failed write does not
    // prevent the remaining terminal state from being repaired.
    let _ = execute!(writer, Show);
    if mouse_capture {
        let _ = execute!(writer, DisableMouseCapture);
    }
    let _ = execute!(writer, LeaveAlternateScreen);
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
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

        restore_terminal(&mut writer, true);

        assert!(writer.writes >= 3);
        assert!(!writer.output.is_empty());
    }
}
