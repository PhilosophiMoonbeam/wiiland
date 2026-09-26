# Local, non-elevated Windows 11 user-mode validation. Never installs a driver or service.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [string]$Device = '1',
    [ValidateRange(10, 300)][int]$DurationSeconds = 60
)

$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$evidence = [IO.Path]::GetFullPath($OutputDirectory)
if (-not [Environment]::Is64BitProcess -or [Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Run this script in 64-bit PowerShell on Windows 11.'
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script in a normal non-elevated user session, not an elevated administrator console.'
}
if (Test-Path -LiteralPath $evidence) { throw "Evidence directory already exists: $evidence" }
if (-not (Test-Path -LiteralPath (Join-Path $root 'Cargo.toml') -PathType Leaf)) {
    throw "Cannot find WiiLand source checkout at $root"
}
New-Item -ItemType Directory -Path $evidence -ErrorAction Stop | Out-Null
$summary = [ordered]@{
    scope = 'Windows user-mode dry-run only; no VHF, broker, desktop output, installation, or production qualification'
    startedUtc = [DateTime]::UtcNow.ToString('o')
    sourceDirectory = $root
    deviceSelector = $Device
    durationSeconds = $DurationSeconds
    stages = [ordered]@{ build = 'NOT TESTED'; diagnostics = 'NOT TESTED'; hid = 'NOT TESTED'; ipc = 'NOT TESTED'; input = 'NOT TESTED'; cleanup = 'NOT TESTED' }
    virtualOutput = 'NOT TESTED'
    releaseQualification = 'NOT TESTED'
    error = $null
}
$daemonProcess = $null
$ownLogonSid = $null
$verifiedPid = $false
$failed = $false

function Invoke-Captured([string]$Program, [string[]]$CommandArguments, [string]$Name, [int]$TimeoutMilliseconds) {
    $settings = @{
        FilePath = $Program
        WorkingDirectory = $root
        PassThru = $true
        NoNewWindow = $true
        RedirectStandardOutput = (Join-Path $evidence "$Name.stdout")
        RedirectStandardError = (Join-Path $evidence "$Name.stderr")
    }
    if ($CommandArguments.Count -gt 0) { $settings.ArgumentList = $CommandArguments }
    $child = Start-Process @settings
    try {
        if (-not $child.WaitForExit($TimeoutMilliseconds)) {
            Stop-Process -Id $child.Id -Force -ErrorAction SilentlyContinue
            throw "$Name exceeded its $TimeoutMilliseconds ms limit; see its logs."
        }
        return $child.ExitCode
    }
    finally { $child.Dispose() }
}

try {
    Set-Location -LiteralPath $root
    # An executable hash in summary.json identifies what this checkout built.
    $os = Get-CimInstance -ClassName Win32_OperatingSystem
    $summary.windows = [ordered]@{ caption = $os.Caption; build = $os.BuildNumber; architecture = $os.OSArchitecture; powershell = $PSVersionTable.PSVersion.ToString() }
    if ([int]$os.BuildNumber -lt 22000 -or $os.ProductType -ne 1) { throw 'This runner requires Windows 11 client.' }
    $summary.rustHost = ((& rustc -vV) | Select-String '^host: ').ToString()
    if ($LASTEXITCODE -ne 0 -or $summary.rustHost -notmatch 'host: x86_64-pc-windows-msvc') {
        throw 'The x86_64-pc-windows-msvc Rust toolchain is required.'
    }
    $targetText = & cargo metadata --no-deps --format-version 1 --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed.' }
    $target = ($targetText | ConvertFrom-Json).target_directory
    $daemon = Join-Path $target 'debug\wiilandd.exe'
    $status = Join-Path $target 'debug\examples\status.exe'
    $cargo = (Get-Command cargo -ErrorAction Stop).Source
    if ((Invoke-Captured $cargo @('build', '--locked', '-p', 'wiilandd', '--bin', 'wiilandd') 'build-daemon' 900000) -ne 0) {
        throw 'Building the Windows daemon failed; see build-daemon.stderr.'
    }
    if ((Invoke-Captured $cargo @('build', '--locked', '-p', 'wiiland-ipc', '--example', 'status') 'build-status' 900000) -ne 0) {
        throw 'Building the Windows IPC status client failed; see build-status.stderr.'
    }
    $summary.binaries = @(
        [ordered]@{ path = $daemon; sha256 = (Get-FileHash -LiteralPath $daemon -Algorithm SHA256).Hash },
        [ordered]@{ path = $status; sha256 = (Get-FileHash -LiteralPath $status -Algorithm SHA256).Hash }
    )
    $summary.stages.build = 'PASS'

    # Refuse to adopt a daemon or claim success from a pre-existing same-logon endpoint.
    if ((Invoke-Captured $status @() 'preexisting-ipc' 8000) -eq 0) {
        throw 'A daemon IPC endpoint already responds; stop your existing daemon before this test.'
    }
    foreach ($arguments in @(
        @('--version'),
        @('--no-config', '--doctor'),
        @('--no-config', '--check-config'),
        @('--no-config', '--self-test'),
        @('--no-config', '--list', '--verbose')
    )) {
        $label = ($arguments -join '_').Replace('-', '')
        if ((Invoke-Captured $daemon $arguments $label 30000) -ne 0) {
            throw "Diagnostic command failed: $($arguments -join ' '); see $label.stderr."
        }
    }
    $summary.stages.diagnostics = 'PASS (metadata and mapping only; output installation not probed)'
    $listed = Get-Content -LiteralPath (Join-Path $evidence 'noconfig_list_verbose.stdout') -Raw
    if ($listed -match 'No Wii Remote devices found') {
        throw 'No Wii Remote HID identity found. Pair the intended device explicitly, then rerun in a new evidence directory.'
    }

    Write-Host "For the next $DurationSeconds seconds, press and release A, then tilt the Wii Remote. Leave it connected until the capture ends."
    $daemonOut = Join-Path $evidence 'daemon-trace.stdout'
    $daemonErr = Join-Path $evidence 'daemon-trace.stderr'
    $daemonProcess = Start-Process -FilePath $daemon -ArgumentList @('--no-config', '--dry-run', '--profile', 'gamepad', '--device', $Device, '--trace-events=all') -WorkingDirectory $root -PassThru -NoNewWindow -RedirectStandardOutput $daemonOut -RedirectStandardError $daemonErr
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $daemonProcess.Refresh()
        if ($daemonProcess.HasExited) { throw "Selected HID daemon exited early ($($daemonProcess.ExitCode)); see daemon-trace.stderr." }
        $attempt = Join-Path $evidence 'status-attempt.stdout'
        if ((Invoke-Captured $status @() 'status-attempt' 8000) -eq 0) {
            $text = Get-Content -LiteralPath $attempt -Raw
            if ($text -match "(?m)^pid=$($daemonProcess.Id)\r?$") {
                $verifiedPid = $true
                if ($text -match '(?m)^device_count=1\r?$' -and $text -match '(?m)^dry_run=true\r?$') {
                    $pipeLine = ($text -split "`n" | Where-Object { $_ -match '^socket_path=' } | Select-Object -First 1).Trim()
                    if ($pipeLine -match '^socket_path=\\\\\.\\pipe\\WiiLand\.(S-1-5-5-\d+-\d+)\.daemon$') {
                        $ownLogonSid = $Matches[1]
                        Copy-Item -LiteralPath $attempt -Destination (Join-Path $evidence 'status-opened.stdout')
                        break
                    }
                }
            }
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $ownLogonSid) { throw 'The owned daemon did not report one opened HID device and authenticated current-logon IPC within 30 seconds.' }
    $summary.stages.hid = 'PASS (opened selected physical HID identity)'
    $summary.stages.ipc = 'PASS (owned PID, dry_run=true, one device, current-logon endpoint)'
    Start-Sleep -Seconds $DurationSeconds
    $daemonProcess.Refresh()
    if ($daemonProcess.HasExited) { throw "Daemon exited during input capture ($($daemonProcess.ExitCode))." }
    if ((Invoke-Captured $status @() 'status-after' 8000) -ne 0) {
        throw 'IPC status failed after the input capture.'
    }
    $after = Get-Content -LiteralPath (Join-Path $evidence 'status-after.stdout') -Raw
    if ($after -notmatch "(?m)^pid=$($daemonProcess.Id)\r?$" -or $after -notmatch '(?m)^device_count=1\r?$' -or $after -notmatch '(?m)^dry_run=true\r?$') {
        throw 'Daemon PID, opened device, or dry-run status changed during capture.'
    }
    $trace = Get-Content -LiteralPath $daemonOut -Raw
    if ($trace -notmatch 'type=0 key=4 state=1' -or $trace -notmatch 'type=0 key=4 state=0') {
        throw 'The trace lacks a complete A-button press and release.'
    }
    $motion = @([regex]::Matches($trace, 'type=1 abs0=(-?\d+,-?\d+,-?\d+)') | ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique)
    if ($motion.Count -lt 2) { throw 'The trace lacks changing accelerometer samples after tilting.' }
    $summary.stages.input = 'PASS (A press/release and changing accelerometer samples)'
}
catch {
    $failed = $true
    $summary.error = $_.Exception.Message
    Write-Error -Message $summary.error -ErrorAction Continue
}
finally {
    if ($null -ne $daemonProcess) {
        $daemonProcess.Refresh()
        if (-not $daemonProcess.HasExited) {
            try {
                if ($verifiedPid -and $ownLogonSid) {
                    $stopEvent = [Threading.EventWaitHandle]::OpenExisting("Local\WiiLandDaemonStop.$ownLogonSid")
                    try { [void]$stopEvent.Set() } finally { $stopEvent.Dispose() }
                    if (-not $daemonProcess.WaitForExit(10000)) { throw 'Daemon did not stop within 10 seconds.' }
                    $summary.stages.cleanup = 'PASS (owned daemon stopped through its existing event)'
                } else { throw 'No verified owned IPC identity for graceful stop.' }
            }
            catch {
                $summary.stages.cleanup = "FAIL (forced stop of owned PID $($daemonProcess.Id): $($_.Exception.Message))"
                $failed = $true
                Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue
                [void]$daemonProcess.WaitForExit(5000)
            }
        } else {
            $summary.stages.cleanup = "PASS (owned daemon already exited with code $($daemonProcess.ExitCode))"
        }
        $daemonProcess.Dispose()
    }
    $summary.completedUtc = [DateTime]::UtcNow.ToString('o')
    $summary | ConvertTo-Json -Depth 7 | Out-File -LiteralPath (Join-Path $evidence 'summary.json') -Encoding UTF8
    Write-Host "Evidence: $evidence"
}
if ($failed) { exit 1 }
Write-Host 'User-mode hardware observation passed. Virtual output and release qualification remain NOT TESTED.'
