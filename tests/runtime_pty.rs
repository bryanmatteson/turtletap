//! Real-terminal contract for the product-owned runtime boundary.

use std::{
    env,
    io::{Read, Write},
    rc::Rc,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use turtletap::{
    Event, Frame, KeyCode, Rect, RuntimeAction, RuntimeEvent, TerminalApplication, TerminalConfig,
    TerminalRuntime,
};

const CHILD_MODE: &str = "TURTLETAP_RUNTIME_PTY_CHILD";

struct ProductApplication {
    _thread_affine: Rc<()>,
    ticked: bool,
    resized: bool,
}

impl TerminalApplication for ProductApplication {
    type Exit = &'static str;

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let marker = if self.resized {
            "PRODUCT_RUNTIME_RESIZED"
        } else if self.ticked {
            "PRODUCT_RUNTIME_TICKED"
        } else {
            "PRODUCT_RUNTIME_ACTIVE"
        };
        frame.render_widget(marker, area);
    }

    fn handle(&mut self, event: RuntimeEvent) -> RuntimeAction<Self::Exit> {
        match event {
            RuntimeEvent::Terminal(Event::Key(key)) if key.code == KeyCode::Char('q') => {
                RuntimeAction::Exit("quit")
            }
            RuntimeEvent::Terminal(Event::Resize(..)) => {
                self.resized = true;
                RuntimeAction::Redraw
            }
            RuntimeEvent::Tick(_) if !self.ticked => {
                self.ticked = true;
                RuntimeAction::Redraw
            }
            _ => RuntimeAction::Ignored,
        }
    }
}

#[test]
fn product_runtime_handles_input_and_restores_a_real_pty() {
    if env::var_os(CHILD_MODE).is_some() {
        let mut application = ProductApplication {
            _thread_affine: Rc::new(()),
            ticked: false,
            resized: false,
        };
        let exit =
            TerminalRuntime::new(TerminalConfig::new().with_tick_rate(Duration::from_millis(10)))
                .run(&mut application)
                .expect("product runtime should complete");
        println!("PRODUCT_RUNTIME_RESTORED:{exit}");
        return;
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 8,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open runtime PTY");
    let mut command = CommandBuilder::new(env::current_exe().expect("find test executable"));
    command.arg("--exact");
    command.arg("product_runtime_handles_input_and_restores_a_real_pty");
    command.arg("--nocapture");
    command.env(CHILD_MODE, "1");
    command.env("TERM", "xterm-256color");

    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn runtime child");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let mut writer = pair.master.take_writer().expect("take PTY writer");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = [0; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });

    let mut output = Vec::new();
    let mut parser = vt100::Parser::new(8, 40, 0);
    wait_for_screen(
        &rx,
        &mut output,
        &mut parser,
        "PRODUCT_RUNTIME_ACTIVE",
        Duration::from_secs(5),
    );
    wait_for_screen(
        &rx,
        &mut output,
        &mut parser,
        "PRODUCT_RUNTIME_TICKED",
        Duration::from_secs(5),
    );
    pair.master
        .resize(PtySize {
            rows: 10,
            cols: 50,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize runtime PTY");
    parser.set_size(10, 50);
    wait_for_screen(
        &rx,
        &mut output,
        &mut parser,
        "PRODUCT_RUNTIME_RESIZED",
        Duration::from_secs(5),
    );
    writer.write_all(b"q").expect("send runtime exit key");
    writer.flush().expect("flush runtime exit key");
    wait_for_output(
        &rx,
        &mut output,
        "PRODUCT_RUNTIME_RESTORED:quit",
        Duration::from_secs(5),
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().expect("inspect runtime child").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "runtime child did not exit");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_screen(
    rx: &Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    parser: &mut vt100::Parser,
    expected: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let screen = parser.screen().contents();
        if screen.contains(expected) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {expected:?}; screen: {screen:?}; output: {}",
            String::from_utf8_lossy(output)
        );
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(chunk) => {
                parser.process(&chunk);
                output.extend_from_slice(&chunk);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "PTY closed before {expected:?}; screen: {:?}; output: {}",
                    parser.screen().contents(),
                    String::from_utf8_lossy(output)
                );
            }
        }
    }
}

fn wait_for_output(
    rx: &Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    expected: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        if String::from_utf8_lossy(output).contains(expected) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {expected:?}; output: {}",
            String::from_utf8_lossy(output)
        );
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(chunk) => output.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "PTY closed before {expected:?}; output: {}",
                    String::from_utf8_lossy(output)
                );
            }
        }
    }
}
