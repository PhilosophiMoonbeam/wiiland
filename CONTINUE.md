# WiiLand Windows port — agent handoff

## Pause point and objective

The user asked to **pause** and leave this file for an entirely new agent session. Do not treat the port as finished or resume speculative implementation without a new instruction or the user's Windows hardware observations. The objective remains Linux support plus a sound Windows 11 port; Windows 10 is desirable only if it entails no compromise. The user has a Windows 11 machine and real Wii hardware, chose **local user-mode prototype validation only** rather than a production VID/signing arrangement, and agreed to run the diagnostic below and return evidence. No Windows hardware evidence has arrived yet.

Work on branch `windows11-port` in `PhilosophiMoonbeam/wiiland`. The last code commit at this handoff was `47fadce74659182f90bdfd64f1084caa98fb8660` (`Exercise repeated authenticated Windows IPC connections`), pushed to `origin/windows11-port`. The worktree was clean before this handoff file was written. Recheck the branch, worktree, and CI when resuming; this is a dated state, not proof that future commits have passed.

## Implemented, with precise limits

- Windows physical Wii Remote HID discovery, input report decoding, and Bluetooth pairing development path: `crates/wiiland-hid/src/windows.rs`, `windows_protocol.rs`, and `windows_pairing.rs`. The CLI includes `wiilandd --pair`, `--list`, and Windows diagnostics. No general pairing workflow has been qualified across controller models.
- Windows daemon lifecycle, dry-run, device processing, and per-device bounded output workers: `crates/wiilandd/src/windows_runtime.rs`, `windows_output.rs`, `windows_output_worker.rs`. Desktop keyboard/relative mouse use per-user `SendInput`; gamepad output goes through the separate privileged broker and VHF virtual HID driver. See `doc/WINDOWS.md` for security and functional boundaries. A successful dry-run does **not** exercise those output paths.
- Rust `WiiLandOutput` LocalSystem service and protocol-v3 broker: `crates/wiiland-output-service/src/windows/`; C KMDF/VHF virtual HID driver source and INF: `drivers/windows/wiiland-vhid/`. This tree does not contain a production-signed driver or authorized product HID identity. The driver source uses prototype `VendorID=0xFFFF`, `ProductID=0x0001`; the production installer rejects prototype VID and unsigned/test-signed distribution. The historical `/home/bbferko/repos/HID-Wiimote` and `windows-drivers-rs` were inputs to the original design discussion, **not** claims that this driver is written in Rust.
- Per-logon authenticated two-pipe daemon IPC bootstrap: `crates/wiiland-ipc/src/windows_bootstrap.rs`, `windows_transport.rs`, `crates/wiilandd/src/ipc/windows_pipe.rs`. The Windows named-pipe ACL requires `FILE_READ_ATTRIBUTES`: the exact granted mask is `0x00120083`, without `FILE_CREATE_PIPE_INSTANCE`. The service broker pipe is a **different** trust boundary. See `doc/DAEMON_PROTOCOL` and `doc/WINDOWS.md`; do not weaken authorization or silently introduce legacy fallback.
- Configuration and CLI adaptations, Windows install/uninstall/update scripts, release-manifest validation, and a Windows MSVC CI job. Packaging is a **development path**; do not claim a supported Windows release. Windows `wiiland-config` and `wiiland-show` are not included in the package. `doc/WINDOWS.md` is the authoritative statement of scope and release acceptance.

## Verification already observed

At commit `47fadce74659182f90bdfd64f1084caa98fb8660`, GitHub Actions run [36271298886](https://github.com/PhilosophiMoonbeam/wiiland/actions/runs/36271298886) completed with conclusion `success` (queried through `gh run view ... --json conclusion,status,headSha,url`). `.github/workflows/ci.yml` runs locked Windows MSVC workspace check, strict clippy, workspace tests, and an **actual** Windows daemon `--no-config --dry-run` plus two successive authenticated IPC status connections. The Windows smoke checks daemon PID, `device_count=0`, dry-run, and current-logon pipe. The Linux CI job includes workspace gates and CLI/IPC/UI smokes. CI does **not** pair a Wii Remote, decode live physical input, run the VHF driver or broker with a live lease, inject desktop input, validate signing/installation, or run on Windows 10. Earlier local checks are not a substitute for these missing observations.

A prior Windows CI failure was a daemon stack overflow (large fixed slot array, now heap-backed). Another was `CreateFileW` error 5 against a narrow named-pipe ACL; a Windows runner A/B probe established that adding `FILE_READ_ATTRIBUTES` fixes the open, and the subsequent actual two-connection smoke passed. Avoid re-diagnosing those resolved issues absent new evidence.

## The one immediate external dependency

The user needs to run `packaging/windows/Test-WiiLandUserMode.ps1` on their Windows 11 machine with an actual Wii Remote. From a checkout of `windows11-port` and an **ordinary, interactive, non-elevated 64-bit PowerShell** session with the `x86_64-pc-windows-msvc` Rust toolchain:

```powershell
.\packaging\windows\Test-WiiLandUserMode.ps1 -OutputDirectory "$env:USERPROFILE\Desktop\WiiLand-validation-1" -Device 1 -DurationSeconds 60
```

Choose a **new** output directory on each attempt. Press and release A and tilt the controller during capture. Stop any pre-existing same-logon daemon first. If `--list` finds no device, consult `doc/WINDOWS.md` § “Local Windows 11 user-mode hardware check” for the explicit `--pair` inquiry/address workflow; do not assume generic Bluetooth Settings pairing works. This script builds the daemon and IPC status example locally and records `summary.json`, build/diagnostic logs, IPC status, trace/error logs, and executable hashes. It checks selected physical HID open, owned PID, current-logon IPC, dry-run, A press/release, and changing accelerometer readings. It **does not** install or test VHF, broker output, `SendInput`, or driver signing. If a controller lacks an accelerometer, this particular input check cannot pass.

Ask for `summary.json`, status outputs, relevant trace/error excerpts, controller model and attachments, and what the user saw. Redact Bluetooth addresses, HID paths, Windows logon SIDs, usernames, and filesystem paths consistently. Investigate **the first failing stage** before changing code. Preserve a deterministic regression test for a real decoded/transport bug when feasible; rerun Windows-target gates and the changed-path smoke. The Linux host in the prior session could not access this Windows machine; no hardware result should be assumed.

## Trajectory after hardware evidence

1. Fix directly observed Windows 11 pairing/HID/daemon defects without weakening the logon-token trust boundary or pretending the dry-run proves output. Keep Linux gates green.
2. For actual gamepad and desktop-output qualification, arrange a separate real Windows test with an authorized HID identity, production-appropriate signed driver/package, and approved installation process; do not ship/install the current prototype identity as a release. Verify broker/VHF lease reports and generic-HID consumers, and per-user `SendInput` within UIPI limits. The user has **not** supplied the identity/signing prerequisites.
3. Only after Windows 11 works end to end, consider Windows 10 22H2 (build 19045) runtime verification **if it introduces no compromise**. The installer minimum in `doc/WINDOWS.md` is not evidence of Windows 10 qualification.
4. Before advertising Windows support, complete the release-owner acceptance list in `doc/WINDOWS.md` § “Production release acceptance” (identity, signing, install/update/rollback/uninstall, hardware/consumer tests, session/security boundaries, documentation). No CI-green or installer-exits-zero shortcut substitutes for those checks.

The previous assistant already sent the user the checkout and diagnostic instructions. While paused, do not repeat requests without new input. On resume, prioritize the user's new observations, re-read `doc/WINDOWS.md`, and follow the repository's `graft map` / `graft ask --source` context-graph rules before opening source files. Do not mark the overarching Windows goal complete from the current CI result.
