# Windows support and packaging

## Status and target

Windows packaging is a development path, not a supported WiiLand release. The repository does not provide a production-ready Windows bundle or completed Windows end-to-end hardware validation. The checked-in virtual HID driver source still sets `VendorID=0xFFFF` and `ProductID=0x0001`; `0xFFFF` is an unassigned prototype VID, not a WiiLand production allocation. Neither that identity nor an unsigned/test-signed build is suitable for distribution.

The technical target is **Windows 10 22H2 (build 19045)** and **Windows 11** on the architecture named by each package (`x64` or `ARM64`). Windows 10 build 19045 is the driver's declared minimum, not a claim that this branch has passed runtime qualification. The installer requires Windows 10 build 19045 or later and an architecture-matched package.

## Windows output boundary

The v3 privileged output path consists of the `WiiLandOutput` LocalSystem broker and the VHF driver. Each gamepad lease exposes only two generic-HID collections: a gamepad (report ID 1) and supplemental axes (report ID 2). These children are created on demand, so no HID children are expected before the per-user daemon obtains a lease. The broker pipe is `\\.\pipe\WiiLandOutput.v3` and the broker protocol is version 3, with no v2 pipe or protocol fallback; the VHF driver ABI and report layout remain version 2. There is no VHF keyboard or mouse collection, and the privileged broker does not inject desktop keyboard or mouse input.

This is standard **generic HID**, not XInput. Windows applications that consume generic HID may recognize a valid released gamepad; applications that require Xbox/XInput devices are not promised to recognize it. WiiLand does not install an XInput shim or create an Xbox device.

The broker pipe is restricted to the active, unlocked console user's logon SID and session, but it does not authenticate a particular client process. Any process running under that same active logon that can connect to the pipe can request gamepad output; the privileged channel is not a general desktop-input API. The broker accepts up to 32 simultaneous connections and up to 32 concurrent gamepad leases globally; leases are not limited to one per connection. Clients heartbeat at least every 500 ms while otherwise idle. The separate 2000 ms idle deadline closes a connection that receives no request.

For each leased gamepad, `wiilandd` enqueues a full-state refresh of report IDs 1 and 2 every 250 ms. Each device has its own bounded output worker so a delayed broker request cannot block input or refreshes for other devices. Queueing or a stalled broker can delay publication beyond 500 ms; if either report ID misses its independent 2000 ms deadline, the broker closes the owning connection and cleans up all its leases. Heartbeats establish connection activity, not report freshness.

Lock, logoff, console disconnect, session change, or suspend/resume advances the session epoch and revokes all clients and leases from the prior epoch; returning to the same session does not resurrect them. The service retains its v3 named-pipe server instances for its entire lifetime, including when no console session is authorized, and reuses them across clients and session transitions. Retaining the pipe namespace does not retain gamepad identities: revocation destroys the VHF children, and no stable child identity across leases is promised. A same-logon process can still exhaust the connection or lease limits, and another local process can squat on the predictable pipe name before the service starts or while it is stopped, but not in gaps between connections. Timely broker publication does not prove fresh physical-controller input: if the controller stops producing events while the daemon retains its last sample, the daemon can keep publishing that cached input state.

Desktop keyboard and relative mouse actions use `SendInput` from the per-user `wiilandd` process, not the broker or VHF driver. That process must run in an interactive, non-elevated, non-UIAccess, below-high-integrity user session; the runtime rejects elevated, high-integrity, UIAccess, and session-0 tokens. Windows UIPI can block `SendInput` from affecting higher-integrity windows or secure desktops. There is no privileged desktop-injection path or promise that input can control elevated applications.

## Windows daemon IPC trust boundary

This section describes the per-user `wiilandd` JSON IPC transport, not the
`WiiLandOutput` privileged broker pipe above. On Windows, daemon IPC uses a
two-pipe bootstrap before carrying the existing JSON IPC 1.1 protocol. The
transport version and fixed-record handshake are specified in
[`DAEMON_PROTOCOL`](DAEMON_PROTOCOL#1a-windows-named-pipe-bootstrap).

The daemon reserves one single-instance rendezvous pipe,
`\\.\pipe\WiiLand.<logon SID>.daemon`, and holds that server handle for its
lifetime. It reuses the instance for successive handshakes with
`DisconnectNamedPipe` and `ConnectNamedPipe`. Each client creates a separate,
single-instance return pipe named
`\\.\pipe\WiiLand.<logon SID>.return.<32 lowercase hex chars>`; the hex suffix
encodes a fresh 16-byte random identifier and is routing material, not a
secret. The client announces that identifier on the rendezvous pipe. The
daemon authenticates the rendezvous writer, sends a fresh operating-system
random challenge over the return pipe, and the client authenticates that
return-pipe writer before echoing the challenge on the same rendezvous
connection. The daemon verifies that echo and sends readiness on the return
pipe. Only readiness status zero admits the connection. The client keeps its
return-pipe handle open and the daemon retains its connected return-pipe handle
for that application connection. Application JSON travels only on the return
pipe; there is no legacy direct-JSON or one-pipe fallback.

Both endpoints use an explicit logon-SID access-control entry granting mask
`0x00120083` (read and write data, read attributes, read-control, and synchronize).
The read-attributes right is required for Windows to open these named pipes;
the mask deliberately excludes `FILE_CREATE_PIPE_INSTANCE`. An Owner Rights
deny entry blocks
`WRITE_DAC` and `WRITE_OWNER`. Pipe instances use
`FILE_FLAG_FIRST_PIPE_INSTANCE`, a maximum of one instance, overlapped I/O, and
`PIPE_REJECT_REMOTE_CLIENTS`. These DACL and pipe-creation settings constrain
access and instance creation; they do not authenticate the peer.

After receiving the appropriate bootstrap record, each side checks the actual
connected pipe writer: it calls `ImpersonateNamedPipeClient`, then
`OpenThreadToken(TOKEN_QUERY, OpenAsSelf=TRUE)`, and requires exact equality of
both `TokenUser` and `TokenLogonSid` with its own values. A matching account
SID from a different logon is insufficient. Pipe opens request non-privileged
Identification SQOS. Failure to impersonate, query, or compare denies the
connection; failure of checked `RevertToSelf` is fatal. Identity failures do
not fall back to owner- or DACL-only acceptance.

One absolute two-second deadline, starting at the beginning of the attempt,
covers the entire rendezvous/return-pipe handshake, including return-pipe
creation, pipe opens/connections, and all record exchanges; it does not restart
for each step. A failed or partial attempt is aborted, with pending I/O
canceled and drained before its resources are released. The overlapped
handshake does not block the reactor or existing clients. Successful bootstrap
preserves the JSON IPC 1.1 contract and up-to-64-application-client capacity.

Authorization is per logon token, not per process. A hostile process sharing
the same user and logon token is within this trust boundary: the protocol does
not provide strong same-account process isolation and cannot distinguish that
process from another process with the same token.

There is also an endpoint-availability limitation before startup: while the
daemon is not running, Windows does not reserve its rendezvous name. Another
process can claim that name first and prevent the daemon from creating its
required first pipe instance; the daemon fails to claim that endpoint rather
than sharing or taking it over. A same-logon process that owns a pipe can also
satisfy the token-identity check, so clients cannot use this handshake to
distinguish it from the daemon. A pre-start squatter can therefore deny
availability, and same-logon process identity is not authenticated.

## Current user-facing limits

There is not yet a supported, end-to-end Windows user workflow:

- A development CLI pairing path exists as `wiilandd --pair`, but it is not release-qualified for all Wii Remote models. There is no Windows pairing GUI or validated general manual-pairing recipe; do not assume Windows Bluetooth Settings can pair and operate every model.
- The daemon now has a Windows runtime and named-pipe transport, but neither is release-qualified. The installer writes the daemon's per-user logon startup entry; it does not launch the daemon as that user or prove pairing, input, or output works.
- `wiiland-config` and `wiiland-show` are not included in this package. Their Windows UI, process-control, diagnostic, and device-control flows are not supported by these scripts. Linux-only commands, device paths, systemd operations, and diagnostics must not be presented as Windows instructions.
- Windows HID report handling remains under development and is not qualified end to end. Known scope limits include report combinations that require cycling modes, no full-resolution IR `0x3e`/`0x3f`, and no promise for undocumented/third-party extension formats or direct Wii U Pro HID behavior.

These are release blockers where they affect advertised functionality. The scripts make no claim that pairing, controller input, a GUI, or daemon runtime works merely because installation succeeds.

## Local Windows 11 user-mode hardware check

`packaging/windows/Test-WiiLandUserMode.ps1` is a **development diagnostic**, not the installer or a Windows-support certification. Run it from the same current source checkout on the Windows 11 machine with the Wii Remote and an x64 MSVC Rust toolchain installed. Use an ordinary, interactive, non-elevated PowerShell session; do not change execution policy or Windows code-integrity settings for this check. The script builds `wiilandd.exe` and the existing Rust IPC status client locally, then runs a bounded, output-suppressed physical-HID capture. It does not install a service or driver, emit virtual input, or modify WiiLand configuration. An existing per-logon daemon must be stopped separately before starting this check.

If `--list` finds no Wii Remote HID identity, pairing is a **separate, explicit** operation. Build the daemon first with `cargo build --locked -p wiilandd --bin wiilandd`, then use `.\target\debug\wiilandd.exe` (or Cargo's configured target directory). Put the intended controller into its pairing mode and, if its Bluetooth address is not known, use `.\target\debug\wiilandd.exe --no-config --pair --device 0` to print inquiry results; that discovery command is expected to exit nonzero without pairing. Inquiry can list unrelated Bluetooth devices. Verify the controller address before running `.\target\debug\wiilandd.exe --no-config --pair --device AA:BB:CC:DD:EE:FF --pairing-method sync` (add `--radio <address|ordinal>` when multiple radios exist). Use `--pairing-method 1+2` only for the corresponding button method. Pairing persists after the diagnostic; it is never undone automatically. Then run `.\target\debug\wiilandd.exe --no-config --list --verbose` to identify the physical HID device's one-based ordinal. Do not confuse the Bluetooth inquiry ordinal/address with this HID ordinal.

From that checkout, use a **new** evidence directory for each attempt:

```powershell
.\packaging\windows\Test-WiiLandUserMode.ps1 -OutputDirectory "$env:USERPROFILE\Desktop\WiiLand-validation-1" -Device 1 -DurationSeconds 60
```

During capture press and release **A**, then deliberately tilt the controller. The diagnostic requires the selected physical HID device to open, a live daemon status over the current-logon named pipe with the launched PID and `dry_run=true`, a complete A-button press/release, and changing accelerometer samples. It retains build/diagnostic output, before/after IPC status, separate trace/error logs, binary hashes, and `summary.json` whether it passes or fails; cleanup signals only its own verified daemon's existing stop event or force-stops only its own child on error. Exit 0 covers **only** those user-mode observations. `--doctor` and `--check-config` do not probe the output driver. If the controller has no accelerometer, this specific check cannot pass and its trace must be assessed separately rather than treated as equivalent evidence.

To check disconnect/reconnect, record the first capture, deliberately disconnect the controller, reconnect it, re-run `--list --verbose`, then invoke the script again with a different new evidence directory. Selected-device daemon mode exits when its device goes away; it does not promise automatic reconnection. Share `summary.json`, the status output, relevant trace excerpts, and the observed controller model/buttons/attachments for diagnosis. Bluetooth addresses, HID paths, Windows logon SIDs, usernames, and filesystem paths in logs may identify the machine or user; redact them consistently before sharing. No script result proves VHF/broker output, `SendInput`, signed-driver install, Windows 10 compatibility, or release readiness. This runner has not been executed on a Windows host in this development environment.

## Release bundle

`packaging/windows/Install-WiiLand.ps1` consumes a prebuilt release bundle; it does not build Rust binaries, compile the KMDF driver, or sign files. The bundle root must contain these direct-child files:

| File | Purpose |
|---|---|
| `wiilandd.exe` | Per-user daemon launched at logon without extra arguments |
| `wiiland-output-service.exe` | The `WiiLandOutput` Windows service executable |
| `wiiland-vhid.inf` | Signed driver package INF for `ROOT\WIILANDVHID` |
| `wiiland-vhid.sys` | KMDF/VHF driver binary referenced by the INF |
| `wiiland-vhid.cat` | Trusted signed package catalog |
| `wiiland-release.json` | Release metadata and SHA-256 inventory |

`wiiland-release.json` uses `formatVersion: 2`.

`formatVersion: 2` is the release-manifest schema version, not the broker protocol version: the output protocol is v3 while the driver ABI and report layout remain v2. The manifest must specify:

- `releaseVersion`, package `architecture` (`x64` or `ARM64`), and the exact `rootHardwareId` `ROOT\WIILANDVHID`;
- `hidVendorId` and `hidProductId` as 16-bit decimal or `0x` hexadecimal values, plus `identityAuthorization.authorized: true` and a non-empty `identityAuthorization.reference` to the real VID/PID allocation/approval record;
- the exact v3 output fields: `outputContract: "gamepad-only-v3"`, `brokerProtocolVersion: 3`, `brokerPipeName: "\\\\.\\pipe\\WiiLandOutput.v3"`, `heartbeatMaxIntervalMs: 500`, `idleDeadlineMs: 2000`, `reportRefreshMaxIntervalMs: 500`, `reportDeadlineMs: 2000`, `driverAbiVersion: 2`, `reportLayoutVersion: 2`, `gamepadReportId: 1`, `supplementalAxesReportId: 2`, and `desktopInput: "per-user-sendinput"`;
- a `files` object with a SHA-256 hex digest for each of the five payload files listed above.

The installer rejects format-v1 manifests and any output-contract mismatch. It also rejects VID zero, prototype VID `0xFFFF`, and PID zero. After activation it requires one started root devnode bound to the newly published driver INF and refuses to start the broker if HID children are already present before any gamepad lease. Zero pre-lease children is normal; VHF children are created on demand. The output contract is v3 while the driver ABI and report layout remain version 2. This checks the active driver package binding, not the descriptor or hardware behavior of a live lease. The authorization field and reference are a release-approval gate, not a USB-IF lookup or proof of authorization; the release owner must verify the referenced allocation before producing a production bundle.

The CAT must have a valid, trusted Authenticode signature. `pnputil /add-driver ... /install` then validates and stages the INF package, including its SYS/CAT relationship, on the target Windows host. The installer also refuses a boot configuration with test signing or disabled code-integrity checks. The release owner must separately verify that the signature is a production Microsoft driver signature, not a test-certificate signature. The source tree contains no private signing key or test certificate.

## Install and update

Use a 64-bit, elevated PowerShell session on Windows 10 22H2 or Windows 11. The selected user's registry hive must be loaded. With UAC elevation as another administrator, pass the target user's SID explicitly:

```powershell
.\packaging\windows\Install-WiiLand.ps1 `
  -PackageRoot 'C:\staging\WiiLand' `
  -UserSid 'S-1-5-21-...'
```

The install path is intentionally fixed at `%ProgramFiles%\WiiLand`; the script refuses alternate or user-writable service locations. Before changing system state it checks the Windows build and architecture, production boot settings, v3 release manifest and payload hashes, CAT trust, service ownership, registry value ownership, and exact install directory contents. Installation then:

1. stages the new payload beside the current directory and stops only an owned `WiiLandOutput` broker service;
2. on update, disables the existing root devnode and verifies that its device stack and old `WiiLandVhid` driver have stopped before package activation; if the driver remains loaded, installation stops and requests a reboot instead of activating a possibly stale stack;
3. adds the signed package for the new release and, on the successful-update path, re-enables an existing root only after its active binding matches that package; on first install, root creation is permitted only after package activation (PnPUtil driver-store installation alone does not create the device). Failed updates follow the ownership-checked rollback rules below;
4. rescans and requires exactly one started root using `WiiLandVhid`; the root's active `InfName` must match the newly published package, and there must be no HID children before the broker starts (leases create the gamepad/axes children later);
5. creates or updates only the own-process `WiiLandOutput` service with a quoted absolute executable path, `LocalSystem` account, automatic start, and `SERVICE_SID_TYPE_UNRESTRICTED` (`ServiceSidType=1`), then verifies that it is running only after the expected v3 output contract and driver binding checks pass;
6. sets the `WiiLandDaemon` value in the selected user's `HKU\<SID>\Software\Microsoft\Windows\CurrentVersion\Run` key to the quoted daemon path.

The installer does **not** require a Wii Remote to be paired or connected. It verifies Windows-side installation (root driver start and service binding to the new signed package, broker service start), not live VHF report descriptors, controller pairing, daemon input, UI behavior, or an end-to-end hardware test. The per-user Run entry starts the daemon at the next logon; it is not proof that the Windows daemon runtime works on that host.

Update reuses the existing installation identity and root devnode. It changes only a service whose SCM image path and account match this installation, a startup value whose type and text match its own recorded value, driver packages recorded in its ownership state, and its own known files. It refuses to overwrite unexpected files, reparse points, or user-changed service/startup configuration. User configuration is outside the installation directory and is not copied, replaced, migrated, or removed by update.

For ordinary install failures, rollback may restore the previous installation as a unit only when its ownership state proves a known `gamepad-only-v2` or `gamepad-only-v3` contract, the corresponding broker protocol version and pipe, the applicable heartbeat/idle/report timing fields, driver ABI/report layout version 2, report IDs `[1, 2]`, and a recorded root binding to a package in the owned INF list. For a v3 install, state validation requires exact equality for the complete broker/output-contract field set above, including the pipe and all timing fields. A verified rollback restores the matching owned files, service, state, package, and root binding together; it is not a runtime fallback from the v3 broker to protocol v2. An unmarked legacy v1 state or unverifiable binding is never re-enabled. If safe restoration cannot be completed, rollback fails closed, keeps the broker stopped and root disabled, and reports when manual recovery/reboot is required. A v3 install starts the broker only after its exact v3 state markers and expected package binding pass, with no pre-lease HID children.

## Uninstall

Run from an elevated PowerShell session while the selected user's profile is loaded:

```powershell
.\packaging\windows\Uninstall-WiiLand.ps1
```

If running under another administrator account, provide the SID recorded by the installation:

```powershell
.\packaging\windows\Uninstall-WiiLand.ps1 -UserSid 'S-1-5-21-...'
```

Uninstall requires the ownership state in `%ProgramFiles%\WiiLand`. It stops/deletes only `WiiLandOutput` when its service image path, account, and service SID match WiiLand's recorded installation; removes only the recorded `ROOT\WIILANDVHID` instance; deletes only the recorded `oem*.inf` packages and never requests forced/device-wide removal; removes the daemon Run value only when it is still the exact WiiLand `REG_SZ` value; and removes only the known installation files. If a driver package is still referenced by another device, Windows may refuse its deletion; the script warns and leaves it in the driver store.

The scripts do not install or remove files in `%ProgramData%` or `%LocalAppData%`. Windows configuration locations used by the Rust config layer are `%ProgramData%\wiiland\wiilandd.conf` and `%LocalAppData%\wiiland\wiilandd.conf`; both remain untouched by install, update, and uninstall.

## Production release acceptance

The installer deliberately does not require pairing or runtime reports as bundle inputs; an administrator must be able to install before pairing any controller. Installation checks the actual target Windows host and driver/service activation, while **release qualification remains a separate, mandatory release-owner gate**. Do not call a Windows build production-ready until the release owner has verified all of the following on real Windows systems:

- an authorized, unique VID/PID allocation, and live leases that expose only the v3 gamepad and supplemental-axis collections (report IDs 1 and 2), with both child identities matching the approved VID/PID and no keyboard/mouse collection;
- a Microsoft-trusted production-signed SYS/CAT package accepted with normal Windows Code Integrity settings (not test signing);
- install, update, rollback, uninstall, service-SID access, and daemon startup on Windows 10 22H2 and Windows 11;
- actual controller pairing and supported gamepad/axis behavior with representative Wii hardware and target generic-HID consumers, plus desktop `SendInput` behavior under UIPI and its elevated/UIAccess/high-integrity restrictions;
- the same-logon named-pipe threat boundary and the absence of privileged keyboard/mouse output;
- accurate user-facing pairing, CLI, and UI limitations for that release.

Keep the operating-system builds, driver-signing result, observed HID identity, paired controller models, and input/output observations as release evidence. Those records are not installer prerequisites. Until this acceptance is complete, describe Windows as in development rather than supported, and do not distribute the prototype driver identity.

## References

- [PnPUtil command syntax](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/pnputil-command-syntax)
- [Windows service SID information](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_sid_info)
