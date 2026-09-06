mod app;
mod daemon;
mod render;

use std::env;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::process;
use std::time::Duration;

use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use wiiland_hid::{Interface, Monitor, MonitorMode};

use app::{Action, App, Selector, parse_selector, poll_interface};

const UI_HELP: &str = "UI commands:\n  q: Quit application\n  f: Freeze/Unfreeze screen\n  s: Refresh static values and recalibrate MotionPlus\n  k: Toggle key events\n  r: Toggle rumble motor (when writable)\n  a: Toggle accelerometer\n  i: Toggle IR camera\n  m: Toggle motion plus\n  n: Toggle normalization for motion plus\n  N: Toggle Nunchuk\n  c: Toggle Classic Controller\n  b: Toggle balance board\n  p: Toggle pro controller\n  g: Toggle guitar controller\n  d: Toggle drums controller\n  1-4: Toggle LEDs (when writable)\n";

fn write_help(mut output: impl Write, program: &str) {
    let _ = write!(
        output,
        "Usage:\n  {program} -h|--help\n  {program} list\n  {program} <positive-ordinal>\n  {program} /sys/path/to/device\nOptions:\n  --direct  Own hardware directly (stop wiilandd first)\n  --socket PATH  Use an explicit daemon socket\nDefault: observe the running daemon; q/f are available.\nDirect hardware UI commands:\n{UI_HELP}"
    );
}

fn main() {
    process::exit(run(env::args_os()));
}

pub fn run<I>(args: I) -> i32
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut raw = args.into_iter();
    let mut args = Vec::new();
    if let Some(program) = raw.next() {
        args.push(program);
    }
    let mut direct = false;
    let mut socket = None;
    while let Some(arg) = raw.next() {
        if arg == "--direct" {
            direct = true;
        } else if arg == "--socket" {
            let Some(path) = raw.next() else {
                eprintln!("wiiland-show: --socket requires a path");
                return 1;
            };
            socket = Some(PathBuf::from(path));
        } else {
            args.push(arg);
        }
    }
    if direct && socket.is_some() {
        eprintln!("wiiland-show: --direct and --socket conflict");
        return 1;
    }
    let program = args
        .first()
        .map(|s| s.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("wiiland-show"));
    if args.len() == 2 && (args[1] == "-h" || args[1] == "--help") {
        write_help(io::stdout().lock(), &program);
        return 0;
    }
    if args.len() != 2 {
        eprintln!("{program}: expected exactly one selector");
        write_help(io::stderr().lock(), &program);
        return 1;
    }
    let selector_arg = args[1].to_string_lossy().into_owned();
    if selector_arg == "list" {
        return if direct {
            list_devices(&program)
        } else {
            daemon::list(&program, socket)
        };
    }
    let selector = match parse_selector(&selector_arg) {
        Ok(value) => value,
        Err(reason) => {
            if reason == "selector ordinal is out of range" {
                eprintln!("{program}: selector ordinal is out of range: {selector_arg}");
            } else {
                eprintln!("{program}: {reason}: {selector_arg}");
            }
            return 1;
        }
    };
    if !direct {
        return daemon::run(&program, &selector_arg, socket);
    }
    let path = match selector {
        Selector::Path(path) => path.to_path_buf(),
        Selector::Ordinal(n) => match ordinal_path(n, &program) {
            Some(path) => path,
            None => return 1,
        },
    };
    let mut iface = match Interface::new(&path) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{program}: cannot open device '{selector_arg}': {error}");
            return 1;
        }
    };
    let shown = iface.syspath().to_string_lossy().into_owned();
    // This line intentionally precedes every terminal guard, including TTY and TERM checks.
    println!("Using Wii Remote: {shown}");
    let _ = io::stdout().flush();
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        eprintln!(
            "{program}: interactive UI requires a terminal on stdin; use '{program} list' for pipelines"
        );
        return 1;
    }
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 0 {
        eprintln!(
            "{program}: interactive UI requires a terminal on stdout; use '{program} list' for redirected output"
        );
        return 1;
    }
    let term = env::var("TERM").unwrap_or_default();
    if term.is_empty() || term == "dumb" {
        eprintln!(
            "{program}: interactive UI requires a usable TERM value (for example, TERM=xterm-256color)"
        );
        return 1;
    }
    match interactive(&mut iface) {
        Ok(()) => 0,
        Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => {
            eprintln!(
                "{program}: interactive UI requires a terminal at least 80 columns by 24 lines"
            );
            1
        }
        Err(error) => {
            eprintln!("{program}: interactive session failed: {error}");
            1
        }
    }
}

fn list_devices(program: &str) -> i32 {
    let mut monitor = match Monitor::new(MonitorMode::Enumerate) {
        Ok(monitor) => monitor,
        Err(error) => {
            eprintln!("{program}: cannot create device monitor: {error}");
            return 1;
        }
    };
    let mut number = 0usize;
    loop {
        match monitor.poll() {
            Ok(Some(path)) => {
                number += 1;
                println!("{number}\t{}", path.display());
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("{program}: cannot enumerate devices: {error}");
                return 1;
            }
        }
    }
    0
}

fn ordinal_path(ordinal: usize, program: &str) -> Option<PathBuf> {
    let mut monitor = match Monitor::new(MonitorMode::Enumerate) {
        Ok(monitor) => monitor,
        Err(error) => {
            eprintln!("{program}: cannot create device monitor: {error}");
            return None;
        }
    };
    let mut number = 0usize;
    loop {
        match monitor.poll() {
            Ok(Some(path)) => {
                number += 1;
                if number == ordinal {
                    return Some(path);
                }
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("{program}: cannot enumerate devices: {error}");
                return None;
            }
        }
    }
    eprintln!("{program}: no device with ordinal {ordinal}");
    None
}

struct TerminalGuard {
    active: bool,
}
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, crossterm::cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self { active: true })
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
            self.active = false;
        }
    }
}

fn interactive(iface: &mut Interface) -> io::Result<()> {
    let guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let (width, height) = terminal::size()?;
    let mut app = App::default();
    app.resize(width, height);
    if matches!(app.mode, app::ViewMode::Error) {
        drop(terminal);
        drop(guard);
        return Err(io::Error::from_raw_os_error(libc::ENOSPC));
    }
    iface.watch(true)?;
    app.open_available(iface);
    let mut monitor = match Monitor::new(MonitorMode::Watch) {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            app.error(format!(
                "Cannot initialize hotplug watch descriptor: {error}"
            ));
            None
        }
    };
    loop {
        terminal.draw(|frame| render::render(frame, &app))?;
        wait_fd(Some(iface.as_fd()), 50)?;
        while poll_interface(iface, &mut app)? {}
        if monitor
            .as_ref()
            .is_some_and(|monitor| monitor.fd().is_some())
        {
            wait_fd(monitor.as_ref().and_then(Monitor::fd), 0)?;
            if let Some(monitor) = monitor.as_mut() {
                while monitor.poll()?.is_some() {
                    app.info("Watch event");
                    app.open_available(iface);
                }
            }
        }
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key, iface)? == Action::Quit {
                        return Ok(());
                    }
                }
                Event::Resize(w, h) => {
                    app.resize(w, h);
                    if matches!(app.mode, app::ViewMode::Error) {
                        app.error("Screen smaller than 80x24; no view");
                    } else if matches!(app.mode, app::ViewMode::Basic) {
                        app.info("Screen smaller than 160x48; limited view");
                    }
                }
                _ => {}
            }
        }
    }
}

fn wait_fd(fd: Option<BorrowedFd<'_>>, timeout_ms: i32) -> io::Result<()> {
    let Some(fd) = fd else {
        std::thread::sleep(Duration::from_millis(timeout_ms as u64));
        return Ok(());
    };
    let mut pollfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
