[CmdletBinding()]
param(
    [string]$InstallDirectory = (Join-Path $env:ProgramFiles 'WiiLand'),
    [string]$UserSid
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'WiiLand.Windows.psm1') -Force

function Wait-WiiLandServiceState([string]$State, [int]$TimeoutSeconds = 30) {
    $limit = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $service = Get-WiiLandService
        if ($null -eq $service) { return $null }
        if ([string]$service.State -eq $State) { return $service }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $limit)
    throw "Timed out waiting for $((Get-WiiLandConstants).ServiceName) to reach state '$State'."
}

function Remove-WiiLandOwnedTree([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $all = @(Get-Item -LiteralPath $Path -Force) + @(Get-ChildItem -LiteralPath $Path -Force -Recurse)
    foreach ($item in $all) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Refusing to remove reparse point in WiiLand installation: $($item.FullName)" }
    }
    Remove-Item -LiteralPath $Path -Force -Recurse
}

Assert-WiiLandAdministrator
$install = Assert-WiiLandInstallDirectory $InstallDirectory
if (-not (Test-Path -LiteralPath $install -PathType Container)) { throw "WiiLand installation not found: $install" }
Assert-WiiLandOwnedContents $install
$state = Read-WiiLandState $install
if ($null -eq $state) { throw 'WiiLand ownership marker is missing; refusing to remove files or system resources.' }
if ([string]::IsNullOrWhiteSpace($UserSid)) { $UserSid = [string]$state.runUserSid }
if ([string]$state.runUserSid -cne $UserSid) { throw 'UserSid does not match the per-user startup entry recorded by this installation.' }

$service = Get-WiiLandService
if ($null -ne $service) {
    Assert-WiiLandServiceOwner $install $service
    Assert-WiiLandServiceSid
    if ([string]$service.State -notin @('Running', 'Stopped')) { throw "The $((Get-WiiLandConstants).ServiceName) service is transitioning; retry after it settles." }
}
$roots = @(Get-WiiLandRootDevices)
if ($roots.Count -gt 1) { throw 'Multiple ROOT\WIILANDVHID devnodes exist; refusing an ambiguous uninstall.' }
$rootInstanceId = [string]$state.rootDeviceInstanceId
$driverInfNames = @($state.driverOemInfs | ForEach-Object { [string]$_ })
$rollbackDriverInf = $null
if ($roots.Count -eq 1) {
    if ($roots[0].InstanceId -ine $rootInstanceId) {
        throw "The active ROOT\WIILANDVHID devnode ($($roots[0].InstanceId)) is not the instance recorded by WiiLand; refusing to remove it."
    }
    if ([string]$roots[0].Service -ine (Get-WiiLandConstants).DriverServiceName) {
        throw "The owned ROOT\WIILANDVHID devnode '$rootInstanceId' is not bound to WiiLandVhid; refusing removal."
    }
    if (-not (Test-WiiLandRollbackStateContract $state) -or -not (Test-WiiLandRollbackPackageFiles $install)) {
        throw 'The recorded installation does not prove a trusted gamepad-only v2/v3 broker and driver bundle; refusing an uninstall that cannot safely restore it.'
    }
    $rootBinding = Get-WiiLandRootDriverBinding $rootInstanceId
    if ($null -eq $rootBinding -or $driverInfNames -notcontains [string]$rootBinding.InfName) {
        throw "The recorded root binding '$rootInstanceId' is not proven by this installation state; refusing to enable it during rollback."
    }
    $rollbackDriverInf = [string]$rootBinding.InfName
}
if ($roots.Count -eq 0 -and -not [string]::IsNullOrWhiteSpace($rootInstanceId)) {
    throw "The recorded WiiLand root devnode '$rootInstanceId' is missing; refusing an uninstall that cannot safely restore it after a failure."
}
if ($null -ne $service -and [string]$service.State -eq 'Running' -and $roots.Count -ne 1) {
    throw 'WiiLandOutput is running without its recorded root devnode; refusing an unsafe uninstall.'
}
$runEntry = Get-WiiLandRunValue $UserSid
$runValue = if ($null -ne $runEntry) { $runEntry.Value } else { $null }
if ($null -ne $runEntry -and ([string]$runEntry.Kind -cne 'String' -or [string]$runEntry.Value -cne [string]$state.runCommand)) {
    throw "The per-user '$((Get-WiiLandConstants).RunValueName)' startup value has changed; refusing to remove or overwrite it."
}

$removedRun = $false
$removedRoot = $false
$deletedService = $false
$wasRunning = $null -ne $service -and [string]$service.State -eq 'Running'
$rootWasStarted = $false
if ($roots.Count -eq 1) { $rootWasStarted = [bool]$roots[0].Started }
if ($wasRunning -and -not $rootWasStarted) { throw 'The WiiLand broker is running while its recorded root devnode is not started; refusing an unsafe uninstall.' }

try {
    if ($wasRunning) {
        Invoke-WiiLandSc -Arguments @('stop', (Get-WiiLandConstants).ServiceName) | Out-Null
        Wait-WiiLandServiceState 'Stopped' | Out-Null
    }
    if ($roots.Count -eq 1) {
        Remove-WiiLandRootDevice $rootInstanceId
        $removedRoot = $true
    }
    if ($null -ne $service) {
        Invoke-WiiLandSc -Arguments @('delete', (Get-WiiLandConstants).ServiceName) | Out-Null
        $deletedService = $true
    }
    if ($null -ne $runValue) {
        Remove-WiiLandRunValue $UserSid
        $removedRun = $true
    }
    Remove-WiiLandOwnedTree $install
    foreach ($oemInf in $driverInfNames) {
        try {
            Invoke-WiiLandPnpUtil -Arguments @('/delete-driver', $oemInf) | Out-Null
        } catch {
            Write-Warning "Windows kept $oemInf in the driver store (it may still be used by another device). No force or device-wide removal was requested."
        }
    }
} catch {
    $failure = $_
    try {
        if ($removedRun) { Set-WiiLandRunValue $UserSid ([string]$state.runCommand) }
        if ($removedRoot) {
            Invoke-WiiLandPnpUtil -Arguments @('/add-driver', (Join-Path $install 'wiiland-vhid.inf'), '/install') -ExpectedDriverInf $rollbackDriverInf -ExpectedRootInstanceId $rootInstanceId -AllowMissingRollbackRoot | Out-Null
            $replacementId = New-WiiLandRootDevice -AllowV2Rollback
            $state.rootDeviceInstanceId = $replacementId
            Write-WiiLandState (Join-Path $install (Get-WiiLandConstants).StateFileName) $state
            Invoke-WiiLandPnpUtil -Arguments @('/scan-devices') | Out-Null
            $restoredDevice = $false
            $restoreDeadline = [DateTime]::UtcNow.AddSeconds(30)
            do {
                $restoredRoots = @(Get-WiiLandRootDevices)
                if ($restoredRoots.Count -eq 1 -and [string]$restoredRoots[0].InstanceId -ieq $replacementId -and
                    [string]$restoredRoots[0].Service -ieq (Get-WiiLandConstants).DriverServiceName -and $restoredRoots[0].Started -eq $true) {
                    $restoredDevice = $true
                    break
                }
                Start-Sleep -Milliseconds 500
            } while ([DateTime]::UtcNow -lt $restoreDeadline)
            if (-not $restoredDevice) { throw 'Rollback could not restart the previous WiiLand root device; keep the broker stopped and complete manual recovery.' }
            $restoredBinding = Get-WiiLandRootDriverBinding $replacementId
            if ($null -eq $restoredBinding -or [string]$restoredBinding.InfName -ine $rollbackDriverInf) {
                throw "Rollback did not bind the replacement root to the exact previous trusted package '$rollbackDriverInf'."
            }
            if (-not $rootWasStarted) {
                Invoke-WiiLandPnpUtil -Arguments @('/disable-device', $replacementId) | Out-Null
                Wait-WiiLandRootDeviceState $replacementId $false
            }
            if ($deletedService) {
                $servicePath = '"' + (Join-Path $install 'wiiland-output-service.exe') + '"'
                Invoke-WiiLandSc -Arguments @('create', (Get-WiiLandConstants).ServiceName, 'binPath=', $servicePath, 'type=', 'own', 'start=', 'auto', 'obj=', 'LocalSystem', 'DisplayName=', 'WiiLand Output Broker') | Out-Null
                Invoke-WiiLandSc -Arguments @('sidtype', (Get-WiiLandConstants).ServiceName, 'unrestricted') | Out-Null
            }
            if ($wasRunning) {
                Assert-WiiLandServiceOwner $install (Get-WiiLandService)
                Invoke-WiiLandSc -Arguments @('start', (Get-WiiLandConstants).ServiceName) | Out-Null
                Wait-WiiLandServiceState 'Running' | Out-Null
            }
        } elseif ($wasRunning -and $null -ne (Get-WiiLandService) -and [string](Get-WiiLandService).State -ne 'Running') {
            Invoke-WiiLandPnpUtil -Arguments @('/add-driver', (Join-Path $install 'wiiland-vhid.inf'), '/install') -ExpectedDriverInf $rollbackDriverInf -ExpectedRootInstanceId $rootInstanceId | Out-Null
            if (-not $rootWasStarted) {
                Invoke-WiiLandPnpUtil -Arguments @('/disable-device', $rootInstanceId) | Out-Null
                Wait-WiiLandRootDeviceState $rootInstanceId $false
            }
            $restoredBinding = Get-WiiLandRootDriverBinding $rootInstanceId
            if ($null -eq $restoredBinding -or [string]$restoredBinding.InfName -ine $rollbackDriverInf) {
                throw "Rollback did not restore the original trusted package binding '$rollbackDriverInf'."
            }
            Invoke-WiiLandSc -Arguments @('start', (Get-WiiLandConstants).ServiceName) | Out-Null
            Wait-WiiLandServiceState 'Running' | Out-Null
        }
    } catch { Write-Warning "Automatic uninstall rollback was incomplete: $($_.Exception.Message) Keep WiiLandOutput stopped; do not enable an unverified root driver. Reboot Windows and complete manual recovery before using WiiLand again." }
    throw $failure
}

Write-Host 'WiiLand system binaries, WiiLandOutput service, owned ROOT\WIILANDVHID devnode, and this user startup entry were removed.'
Write-Host 'User and shared configuration files were preserved. Driver packages still referenced by other devices were left in the driver store.'
