<p align="center">
  <img src="res/wiiland-logo.svg" width="800" alt="WiiLand — Native Wii input for Linux">
</p>

<p align="center">
  Turn Nintendo Wii-family controllers into first-class Linux gamepads, pointers, and keyboards.
</p>

<p align="center">
  <a href="#quick-start"><strong>Quick start</strong></a> ·
  <a href="#configure">Configure</a> ·
  <a href="#diagnose">Diagnose</a> ·
  <a href="#development">Development</a> ·
  <a href="#documentation">Documentation</a>
</p>

---

WiiLand is a display-neutral Linux input stack for Wii Remotes, extensions,
Balance Boards, Wii U Pro Controllers, and compatible devices. It uses the
kernel's `hid-wiimote` driver and emits ordinary `uinput`/`evdev` devices—no
compositor API, XWayland dependency, or application-specific driver.

```text
Wii hardware → hid-wiimote → wiiland-hid
                              ├─ direct Rust app (owns Linux hardware) → typed events
                              └─ wiilandd (owns hardware) → uinput/evdev
                                               └─ private per-user socket → wiiland-ipc Client
```

| | |
|---|---|
| **Sessions** | Native Wayland and X.org |
| **Outputs** | `WiiLand Virtual Controller`, `WiiLand Virtual Desktop` |
| **Profiles** | `gamepad`, `desktop`, `both` |
| **Frontends** | `wiiland-config` (eframe/egui, Wayland/X11), `wiiland-show` (ratatui/crossterm) |
| **Driver path** | Linux `hid-wiimote` → Rust `wiiland-hid` → direct app or `wiilandd` |

## Rust integration paths

Choose one of these source-level Cargo integrations. A process either owns the
Linux HID interfaces directly or consumes the daemon's view; the two ownership
paths are not interchangeable.

### Direct hardware ownership: `wiiland-hid`

Add the workspace crate as a Cargo path dependency. The root `wiiland_hid`
facade exposes `Monitor`, `MonitorMode`, `Interface`, and owned typed event
values; it does not expose decoder or model implementation modules:

```toml
[dependencies]
wiiland-hid = { path = "../wiiland/crates/wiiland-hid" }
```

This process opens the kernel `hid-wiimote` interfaces itself, so it needs the
same Linux device permissions as the daemon. `Monitor::poll` and
`Interface::dispatch` return `io::Result`; handle `WouldBlock` when polling
without an event:

```rust
use std::io;
use wiiland_hid::{Event, EventKind, Interface, InterfaceMask, Monitor, MonitorMode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut monitor = Monitor::new(MonitorMode::Enumerate)?;
    let Some(path) = monitor.poll()? else {
        return Ok(());
    };
    let mut device = Interface::new(&path)?;
    device.open(InterfaceMask::CORE | InterfaceMask::ACCEL)?;

    loop {
        match device.dispatch() {
            Ok(Event { kind: EventKind::Accel(axis), .. }) => {
                println!("accel: {}, {}, {}", axis.x, axis.y, axis.z);
            }
            Ok(Event { kind: EventKind::Key(button), .. }) => {
                println!("button: {button:?}");
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error.into()),
        }
    }
}
```

### Daemon access: `wiiland-ipc`

For multi-process applications, let `wiilandd` own hardware and connect to its
versioned Unix-socket protocol through the source-level `wiiland_ipc::Client`
facade:

```toml
[dependencies]
wiiland-ipc = { path = "../wiiland/crates/wiiland-ipc" }
```

`Client::connect` performs the protocol Hello negotiation automatically.
`connect_default` uses
`$XDG_RUNTIME_DIR/wiiland/wiilandd.sock` only when `XDG_RUNTIME_DIR` is an
absolute path; otherwise use an explicit private socket path. The client
receives owned DTOs rather than Rust or libc layouts:

```rust
use wiiland_ipc::{Client, InputPayload, Notification};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = Client::connect_default()?;
    let status = client.status()?;
    println!("{}: {} device(s)", status.daemon_version, status.device_count);
    client.subscribe()?;

    loop {
        match client.next_event()? {
            Notification::Input {
                syspath,
                payload: InputPayload::Accel(axis),
                ..
            } => println!("{syspath}: accel {}, {}, {}", axis.x, axis.y, axis.z),
            Notification::DeviceAdded { device, .. } => {
                println!("added: {}", device.syspath);
            }
            Notification::DeviceRemoved { syspath, reason, .. } => {
                println!("removed: {syspath} ({reason:?})");
            }
            Notification::Input { .. } => {}
            _ => {}
        }
    }
}
```

The daemon endpoint is private to the user (`0700` parent directory and
`0600` socket), not a global system socket. `wiiland-ipc` is an `rlib`-only
workspace/build crate. Neither it nor `wiiland-hid` is an installed
development library: packaging emits no standalone client executable, header,
static/shared library, or pkg-config file. Neither integration path promises a
stable Rust ABI or a C ABI.


## Quick start

### 1. Build and install

WiiLand is a Linux-only Rust workspace. The repository pins the required
toolchain to Rust 1.98.0 in `rust-toolchain.toml`; Cargo uses resolver 3,
edition 2024, and workspace package version 2.0.0. A Linux host also needs
libudev development files and access to `/dev/uinput`; systemd, udev, and X.org
are needed only for the integrations being installed.

Build the daemon and both optional frontends:

```sh
cargo xtask build --release --features gui,tui,integrations
```

Install directly under `/usr` (or stage the exact same tree with `DESTDIR`):

```sh
sudo cargo xtask install --release \
  --prefix /usr --sysconfdir /etc \
  --features gui,tui,integrations

# Package-manager staging example:
cargo xtask install --release --destdir "$DESTDIR" \
  --prefix /usr --sysconfdir /etc \
  --features gui,tui,integrations
```

`cargo xtask install` uses the typed install manifest. It installs the selected
binaries, configuration, manual pages, documentation, and integration assets.
`--with-udev-rules-dir`, `--with-systemd-user-unit-dir`, and
`--with-xorg-conf-dir` each accept `auto`, `no`, or an absolute destination.
`DESTDIR` prefixes staged files only; generated service content contains the
logical (unstaged) paths.

After installing udev rules, reload them and reconnect the controller:

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger
```

### 2. Pair a controller

The desktop Bluetooth panel is the shortest route. For `bluetoothctl`, press the
controller's sync button, then:

```text
power on
agent on
default-agent
scan on
pair XX:XX:XX:XX:XX:XX
trust XX:XX:XX:XX:XX:XX
connect XX:XX:XX:XX:XX:XX
scan off
quit
```

Replace the address with the device shown by `scan on`.

### 3. Check the host

```sh
wiilandd --doctor
wiilandd --list
```

`--doctor` checks session detection, Bluetooth, configuration, and input/uinput
permissions. Fix reported permission failures before starting the daemon.

### 4. Run WiiLand

Choose one mode—do not run the foreground daemon and user service together.

```sh
# Fast foreground test
wiilandd --profile gamepad

# Normal user service
systemctl --user daemon-reload
systemctl --user enable --now wiilandd.service
```

With the eframe frontend installed:

```sh
wiiland-config
```

`wiiland-config` uses egui with native winit Wayland/X11 backends and application
ID `io.github.philosophimoonbeam.wiiland-config`; no toolkit-specific platform
override is required. The control center edits and validates configuration,
manages the user service, and captures live input and calibration samples through
the running daemon. Live capture keeps normal virtual input active; the explicit
direct-hardware diagnostic mode requires stopping the service first.
Its pearl and dusk themes carry the logo's sea-glass palette into focused settings
pages, with persistent save controls and an expandable activity log. Appearance
follows the system by default; you can also select either palette explicitly.

<p align="center">
  <img src="res/wiiland-control-center.png" width="800" alt="WiiLand Control Center in the pearl theme, showing labelled desktop button bindings and persistent save controls">
</p>

The optional `wiiland-show` diagnostic uses ratatui/crossterm and restores the
terminal on exit. It uses the running daemon by default:

```sh
wiiland-show list
wiiland-show 1
wiiland-show --socket /absolute/private/wiilandd.sock 1
```

Use `--direct` after stopping the daemon for hardware controls such as LEDs,
rumble, and normalization. In daemon mode, `q` quits and `f` freezes the display.
Both frontend capture paths require daemon protocol 1.1 or newer; existing
protocol 1.0 status and subscription clients remain supported.

## Configure

WiiLand loads configuration in this order:

1. `/etc/wiiland/wiilandd.conf`
2. `$XDG_CONFIG_HOME/wiiland/wiilandd.conf` when `XDG_CONFIG_HOME` is absolute;
   otherwise `$HOME/.config/wiiland/wiilandd.conf`

Later values win. `--config PATH` selects one file instead; `--no-config` uses
built-in defaults. The complete annotated sample is
[`res/wiilandd.conf`](res/wiilandd.conf).

Create a small user configuration:

```sh
config_dir="$HOME/.config/wiiland"
mkdir -p "$config_dir"
printf '%s\n' \
  'profile=both' \
  'pointer-speed=24' \
  'ir-speed=10' \
  > "$config_dir/wiilandd.conf"

wiilandd --check-config
systemctl --user restart wiilandd.service
```

| Profile | Result |
|---|---|
| `gamepad` | One virtual controller per connected remote |
| `desktop` | Virtual pointer and keyboard controls |
| `both` | Gamepad and desktop devices together |

The sample configuration also covers IR tracking, screen calibration, desktop
button mapping, per-device rules, and motion aiming. Capture optional flat-surface
motion calibration with:

```sh
wiilandd --device N --calibrate-aim
```

Copy the emitted key/value lines into the user configuration. Each enabled
sensor requires its complete X/Y/Z triple.

## Diagnose

| Goal | Command |
|---|---|
| Host readiness | `wiilandd --doctor` |
| Connected remotes | `wiilandd --list` |
| Validate configuration | `wiilandd --check-config` |
| Show effective configuration | `wiilandd --dump-config` |
| Show virtual axis mapping | `wiilandd --axis-map` |
| Print hardware test matrix | `wiilandd --validation-checklist` |
| Run hardware-free checks | `wiilandd --self-test` |
| Gather a support report | `wiilandd-hardware-report` |

Trace a connected remote without creating virtual devices:

```sh
wiilandd --dry-run --trace-events --verbose --device N --profile both
```

Use a number from `wiilandd --list` or a `/sys` device path. For a shareable
report with a live trace:

```sh
wiilandd-hardware-report N
```

### Common failures

| Symptom | Action |
|---|---|
| No remote listed | Pair, trust, and connect it; confirm `hid-wiimote` is loaded |
| Input/uinput denied | Install the udev rules, reconnect, then rerun `--doctor` |
| Service will not start | Run `--check-config`, then inspect `journalctl --user -u wiilandd.service` |
| GUI window will not open | Confirm the active Wayland/X11 session and its eframe/winit runtime libraries |
| Duplicate raw pointer on X.org | Install the packaged `50-xorg-fix-wiiland.conf` policy |

## Development

The workspace contains the native Rust `wiiland-hid` hardware crate, the
source-level `wiiland-ipc` blocking Unix-socket client facade, pure
`wiiland-core` mapping/configuration logic, the `wiilandd` daemon and report
binary, the optional `wiiland-config` and `wiiland-show` applications, and the
developer-only `wiiland-dump` utility. Use the repository's Cargo alias for xtask:

```sh
cargo xtask build --features gui,tui,integrations
cargo xtask build --release --features gui,tui,integrations
cargo xtask check --all-features
cargo xtask docs
```

`wiiland-hid` deliberately targets the Linux `hid-wiimote` kernel interface,
including its udev/sysfs topology, evdev node identities and codes, force
feedback, LEDs, and `SYN_DROPPED` recovery. Its public root facade exposes
owned Rust values, not raw decoder/model modules. It does not provide a stable
Rust ABI or a C ABI; see the integration paths above for source-level Cargo
usage.

The normal workspace gates are:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

Create and verify the deterministic single-root source archive:

```sh
cargo xtask dist --output wiiland-2.tar.xz
cargo xtask verify-dist wiiland-2.tar.xz
```

`wiiland-dump` remains a non-installed EEPROM diagnostic and requires kernel
`debugfs`. Hardware-free daemon checks are exposed by `wiilandd --self-test`;
real-device reports should include kernel, distribution, Bluetooth adapter,
device type, session, and consumer results.

## Documentation

- [`doc/WIILAND`](doc/WIILAND) — architecture, configuration, operations, and validation
- [`doc/wiilandd.1`](doc/wiilandd.1) — daemon and command reference
- [`doc/wiiland-config.1`](doc/wiiland-config.1) — control center reference
- [`doc/wiiland.7`](doc/wiiland.7) — installed overview
- [`doc/ARCHITECTURE.md`](doc/ARCHITECTURE.md) — processing boundaries, capture leases, and IPC 1.1
- [`doc/DEVICES`](doc/DEVICES) and [`doc/PROTOCOL`](doc/PROTOCOL) — hardware model and archival protocol notes
- [`DEV`](DEV) — contributor build and packaging notes

Questions, bugs, and hardware reports belong in the
[issue tracker](https://github.com/PhilosophiMoonbeam/wiiland/issues).

## License and lineage

WiiLand is open source under the permissive terms in [`LICENSE`](LICENSE).
[`COPYING`](COPYING) preserves lineage, authorship, and contributor
acknowledgements.
