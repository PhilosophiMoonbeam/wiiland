//! Read-only daemon view; direct hardware controls remain in the explicit mode.
use crate::{TerminalGuard, app::App, render};
use crossterm::{
    event::{self, Event, KeyCode},
    terminal,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};
use wiiland_hid::{Axis3, Button, ButtonEvent, ButtonState, EventKind};
use wiiland_ipc::{Client, ClientError, InputPayload, Notification, Session, SessionEvent};

fn connect(socket: Option<&PathBuf>) -> Result<Client, ClientError> {
    let client = match socket {
        Some(path) => Client::connect(path)?,
        None => Client::connect_default()?,
    };
    client.set_read_timeout(Some(Duration::from_secs(2)))?;
    client.set_write_timeout(Some(Duration::from_secs(2)))?;
    Ok(client)
}

pub fn list(program: &str, socket: Option<PathBuf>) -> i32 {
    match connect(socket.as_ref()).and_then(|mut client| client.devices()) {
        Ok(devices) => {
            for (index, device) in devices.iter().enumerate() {
                println!("{}\t{}", index + 1, device.syspath);
            }
            0
        }
        Err(error) => {
            eprintln!("{program}: {error}; start wiilandd or use --direct");
            1
        }
    }
}

pub fn run(program: &str, selector: &str, socket: Option<PathBuf>) -> i32 {
    let result = (|| {
        let devices = connect(socket.as_ref())
            .and_then(|mut client| client.devices())
            .map_err(io::Error::other)?;
        let devices =
            wiiland_ipc::select_devices(devices, selector, true).map_err(io::Error::other)?;
        let path = devices[0].syspath.clone();
        println!("Using Wii Remote: {path}");
        io::stdout().flush()?;
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0
            || unsafe { libc::isatty(libc::STDOUT_FILENO) } == 0
        {
            return Err(io::Error::other(
                "interactive UI requires a terminal on stdin and stdout",
            ));
        }
        if !std::env::var("TERM").is_ok_and(|term| !term.is_empty() && term != "dumb") {
            return Err(io::Error::other(
                "interactive UI requires a usable TERM value",
            ));
        }
        let _guard = TerminalGuard::enter()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let mut app = App::default();
        let (width, height) = terminal::size()?;
        app.resize(width, height);
        if matches!(app.mode, crate::app::ViewMode::Error) {
            return Err(io::Error::other(
                "interactive UI requires a terminal at least 80 columns by 24 lines",
            ));
        }
        app.info("Daemon capture: q quit, f freeze; hardware controls require --direct");
        let session = Session::start(socket, path, true);
        loop {
            for _ in 0..256 {
                let Ok(event) = session.try_recv() else {
                    break;
                };
                match event {
                    SessionEvent::Connected { status, devices } => {
                        app.info(format!(
                            "Connected to wiilandd {} (pid {})",
                            status.daemon_version, status.pid
                        ));
                        if let Some(device) = devices.first() {
                            configure(&mut app, device.opened_interfaces);
                        }
                    }
                    SessionEvent::Notification(Notification::Input {
                        timestamp, payload, ..
                    }) => {
                        if let Some(kind) = input(payload) {
                            app.apply_event(&wiiland_hid::Event {
                                time: wiiland_hid::Timestamp {
                                    seconds: timestamp.seconds,
                                    microseconds: timestamp.micros,
                                },
                                kind,
                            });
                        }
                    }
                    SessionEvent::Notification(Notification::DeviceRemoved { .. }) => {
                        return Err(io::Error::other("Device disconnected"));
                    }
                    _ => {}
                }
            }
            if let Ok(result) = session.try_finish() {
                return result.map_err(io::Error::other);
            }
            terminal.draw(|frame| render::render(frame, &app))?;
            if event::poll(Duration::from_millis(16))? {
                match event::read()? {
                    Event::Key(key) if key.kind != event::KeyEventKind::Release => match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Char('f') => app.frozen = !app.frozen,
                        _ => app.info("Hardware controls require --direct; q quits and f freezes"),
                    },
                    Event::Resize(width, height) => app.resize(width, height),
                    _ => {}
                }
            }
        }
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{program}: {error}");
            1
        }
    }
}

fn configure(app: &mut App, bits: u32) {
    app.keys_enabled = bits & 1 != 0;
    app.accel_enabled = bits & 2 != 0;
    app.ir_enabled = bits & 4 != 0;
    app.motion_plus_enabled = bits & 0x100 != 0;
    app.nunchuk_enabled = bits & 0x200 != 0;
    app.classic_enabled = bits & 0x400 != 0;
    app.balance_enabled = bits & 0x800 != 0;
    app.pro_enabled = bits & 0x1000 != 0;
    app.drums_enabled = bits & 0x2000 != 0;
    app.guitar_enabled = bits & 0x4000 != 0;
    app.led_writable = [false; 4];
}
fn axis(v: wiiland_ipc::Axis3) -> Axis3 {
    Axis3 {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}
fn button(v: wiiland_ipc::ButtonEvent) -> Option<ButtonEvent> {
    Some(ButtonEvent {
        button: Button::from_code(v.code)?,
        state: match v.state {
            0 => ButtonState::Released,
            1 => ButtonState::Pressed,
            2 => ButtonState::Repeated,
            _ => return None,
        },
    })
}
fn input(payload: InputPayload) -> Option<EventKind> {
    Some(match payload {
        InputPayload::Key(v) => EventKind::Key(button(v)?),
        InputPayload::Accel(v) => EventKind::Accel(axis(v)),
        InputPayload::Ir(v) => EventKind::Ir(v.map(axis)),
        InputPayload::BalanceBoard(v) => EventKind::BalanceBoard(v.map(axis)),
        InputPayload::MotionPlus(v) => EventKind::MotionPlus(axis(v)),
        InputPayload::ProControllerKey(v) => EventKind::ProControllerKey(button(v)?),
        InputPayload::ProControllerMove(v) => EventKind::ProControllerMove(v.map(axis)),
        InputPayload::Watch => EventKind::Watch,
        InputPayload::ClassicControllerKey(v) => EventKind::ClassicControllerKey(button(v)?),
        InputPayload::ClassicControllerMove(v) => EventKind::ClassicControllerMove(v.map(axis)),
        InputPayload::NunchukKey(v) => EventKind::NunchukKey(button(v)?),
        InputPayload::NunchukMove(v) => EventKind::NunchukMove(v.map(axis)),
        InputPayload::DrumsKey(v) => EventKind::DrumsKey(button(v)?),
        InputPayload::DrumsMove(v) => EventKind::DrumsMove(v.map(axis)),
        InputPayload::GuitarKey(v) => EventKind::GuitarKey(button(v)?),
        InputPayload::GuitarMove(v) => EventKind::GuitarMove(v.map(axis)),
        InputPayload::Gone => EventKind::Gone,
        InputPayload::Unknown(v) => EventKind::Unknown(v),
        _ => return None,
    })
}
