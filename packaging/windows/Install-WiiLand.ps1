[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$PackageRoot,
    [string]$InstallDirectory = (Join-Path $env:ProgramFiles 'WiiLand'),
    [string]$UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'WiiLand.Windows.psm1') -Force

function Wait-WiiLandServiceState([string]$State, [int]$TimeoutSeconds = 30) {
    $limit = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $service = Get-WiiLandService
        if ($null -eq $service) { return $null }
        if ([string]$service.State -ceq $State) { return $service }
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

function Restore-WiiLandRunValue([string]$Sid, [object]$Value) {
    if ($null -eq $Value) { Remove-WiiLandRunValue $Sid }
    else { Set-WiiLandRunValue $Sid ([string]$Value) }
}

function Wait-WiiLandRootBinding([string]$InstanceId, [int]$TimeoutSeconds = 30) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $binding = Get-WiiLandRootDriverBinding $InstanceId
        if ($null -ne $binding) { return $binding }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    return $null
}


Assert-WiiLandAdministrator
$platform = Get-WiiLandPlatform
Assert-WiiLandProductionBoot
$release = Get-WiiLandRelease $PackageRoot
if ($release.Manifest.architecture -cne $platform.Architecture) {
    throw "Package architecture '$($release.Manifest.architecture)' does not match this $($platform.Architecture) Windows installation."
}
$install = Assert-WiiLandInstallDirectory $InstallDirectory
$package = Get-WiiLandCanonicalPath $release.Root
if ($package -ieq $install -or $package.StartsWith($install + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'PackageRoot must not be inside InstallDirectory.'
}

$state = $null
$oldService = Get-WiiLandService
$oldServiceWasRunning = $false
if (Test-Path -LiteralPath $install) {
    $children = @(Get-ChildItem -LiteralPath $install -Force)
    $state = Read-WiiLandState $install
    if ($null -eq $state -and $children.Count -gt 0) { throw "InstallDirectory already contains unowned files: $install" }
    if ($null -ne $state) { Assert-WiiLandOwnedContents $install }
}
if ($null -ne $oldService) {
    Assert-WiiLandServiceOwner $install $oldService
    if ($null -eq $state) { throw "The $((Get-WiiLandConstants).ServiceName) service exists without WiiLand's ownership marker; refusing to adopt it." }
    if ([string]$oldService.State -notin @('Running', 'Stopped')) { throw "The $((Get-WiiLandConstants).ServiceName) service is transitioning; retry after it settles." }
    Assert-WiiLandServiceConfiguration
    $oldServiceWasRunning = [string]$oldService.State -eq 'Running'
}

$rootDevices = @(Get-WiiLandRootDevices)
if ($rootDevices.Count -gt 1) { throw "Found multiple $((Get-WiiLandConstants).RootHardwareId) devnodes; refusing an ambiguous update." }
$rootId = $null
$priorRootWasStarted = $false
if ($rootDevices.Count -eq 1) {
    $rootId = [string]$rootDevices[0].InstanceId
    if ($null -eq $state -or [string]$state.rootDeviceInstanceId -ine $rootId) {
        throw "Found an unowned $((Get-WiiLandConstants).RootHardwareId) devnode ($rootId); refusing to modify it."
    }
    if ([string]$rootDevices[0].Service -ine (Get-WiiLandConstants).DriverServiceName) {
        throw "The owned ROOT\WIILANDVHID devnode '$rootId' is not bound to WiiLandVhid; refusing an update."
    }
    $priorRootWasStarted = [bool]$rootDevices[0].Started
} elseif ($null -ne $state -and -not [string]::IsNullOrWhiteSpace([string]$state.rootDeviceInstanceId)) {
    throw "The recorded WiiLand root devnode '$($state.rootDeviceInstanceId)' is missing; refusing an update without a recoverable root binding."
}
if ($null -ne $state -and [string]$state.runUserSid -ine $UserSid) { throw 'An update must use the same Windows user SID as the existing WiiLand autostart entry.' }
$runCommand = '"' + (Join-Path $install 'wiilandd.exe') + '"'
$oldRunEntry = Get-WiiLandRunValue $UserSid
$oldRunValue = if ($null -ne $oldRunEntry) { $oldRunEntry.Value } else { $null }
if ($null -ne $oldRunEntry -and ([string]$oldRunEntry.Kind -cne 'String' -or $null -eq $state -or [string]$oldRunEntry.Value -cne [string]$state.runCommand)) {
    throw "The per-user '$((Get-WiiLandConstants).RunValueName)' startup value is not owned by this installation; refusing to overwrite it."
}

$knownDriverInfs = @()
if ($null -ne $state) { $knownDriverInfs = @($state.driverOemInfs | ForEach-Object { [string]$_ }) }
$priorRollbackDriverInf = $null
if ($null -ne $state) {
    if ($rootDevices.Count -ne 1 -or $knownDriverInfs.Count -eq 0 -or
        -not (Test-WiiLandRollbackStateContract $state) -or -not (Test-WiiLandRollbackPackageFiles $install)) {
        throw 'The owned installation does not prove a restorable gamepad-only v2/v3 broker and trusted v2 driver package; refusing an update that cannot roll back safely.'
    }
    $priorBinding = Get-WiiLandRootDriverBinding $rootId
    if ($null -eq $priorBinding -or $knownDriverInfs -notcontains [string]$priorBinding.InfName) {
        throw "The original WiiLand root binding '$rootId' is not verifiably owned by its recorded driver packages; refusing the update."
    }
    $priorRollbackDriverInf = [string]$priorBinding.InfName
    if ($oldServiceWasRunning -and -not $priorRootWasStarted) {
        throw 'The WiiLand broker is running while its recorded root devnode is not started; refusing an unsafe update.'
    }
}

$driverStoreOutput = Invoke-WiiLandPnpUtil -Arguments @('/enum-drivers')
$driverStoreText = $driverStoreOutput -join "`n"
$driverStoreNamesBefore = @([regex]::Matches($driverStoreText, '(?i)\boem\d+\.inf\b') | ForEach-Object { $_.Value.ToLowerInvariant() } | Select-Object -Unique)
if ($null -eq $state -and $driverStoreText -match '(?i)wiiland-vhid\.inf') {
    throw 'A WiiLand driver package is already in the driver store without an installation marker; remove it through its owning installation first.'
}

$installId = if ($null -ne $state) { [string]$state.installId } else { [guid]::NewGuid().ToString() }
$stage = Join-Path (Split-Path -Parent $install) ('.WiiLand-stage-' + [guid]::NewGuid().ToString('N'))
$backup = Join-Path (Split-Path -Parent $install) ('.WiiLand-backup-' + [guid]::NewGuid().ToString('N'))
$hadDirectory = Test-Path -LiteralPath $install
$completed = $false
$directorySwapped = $false
$serviceCreated = $false
$rootCreated = $false
$runChanged = $false
$newDriverInf = $null
$driverInstallAttempted = $false
$statePath = Join-Path $install (Get-WiiLandConstants).StateFileName

function New-WiiLandStateObject([string]$DeviceId, [string[]]$DriverInfs) {
    [ordered]@{
        formatVersion = 1
        outputContract = [string]$release.Manifest.outputContract
        brokerProtocolVersion = [int]$release.Manifest.brokerProtocolVersion
        brokerPipeName = [string]$release.Manifest.brokerPipeName
        reportRefreshMaxIntervalMs = [int]$release.Manifest.reportRefreshMaxIntervalMs
        reportDeadlineMs = [int]$release.Manifest.reportDeadlineMs
        heartbeatMaxIntervalMs = [int]$release.Manifest.heartbeatMaxIntervalMs
        idleDeadlineMs = [int]$release.Manifest.idleDeadlineMs
        driverAbiVersion = [int]$release.Manifest.driverAbiVersion
        reportLayoutVersion = [int]$release.Manifest.reportLayoutVersion
        reportIds = @([int]$release.Manifest.gamepadReportId, [int]$release.Manifest.supplementalAxesReportId)
        installId = $installId
        installDirectory = $install
        releaseVersion = [string]$release.Manifest.releaseVersion
        architecture = [string]$release.Manifest.architecture
        serviceName = (Get-WiiLandConstants).ServiceName
        rootHardwareId = (Get-WiiLandConstants).RootHardwareId
        hidVendorId = $release.VendorId
        hidProductId = $release.ProductId
        rootDeviceInstanceId = $DeviceId
        driverOemInfs = @($DriverInfs | Select-Object -Unique)
        runCommand = $runCommand
        runUserSid = $UserSid
        updatedUtc = [DateTime]::UtcNow.ToString('o')
    }
}

try {
    New-Item -ItemType Directory -Path $stage -ErrorAction Stop | Out-Null
    foreach ($name in @('wiilandd.exe', 'wiiland-output-service.exe', 'wiiland-vhid.inf', 'wiiland-vhid.sys', 'wiiland-vhid.cat')) {
        Copy-Item -LiteralPath (Join-Path $release.Root $name) -Destination (Join-Path $stage $name)
    }
    $initialState = New-WiiLandStateObject $rootId $knownDriverInfs
    Write-WiiLandState (Join-Path $stage (Get-WiiLandConstants).StateFileName) $initialState

    if ($oldServiceWasRunning) {
        Invoke-WiiLandSc -Arguments @('stop', (Get-WiiLandConstants).ServiceName) | Out-Null
        Wait-WiiLandServiceState 'Stopped' | Out-Null
    }
    if ($hadDirectory) {
        Move-Item -LiteralPath $install -Destination $backup
        $directorySwapped = $true
    }
    Move-Item -LiteralPath $stage -Destination $install
    $directorySwapped = $true
    $statePath = Join-Path $install (Get-WiiLandConstants).StateFileName

    $driverInstallAttempted = $true
    $pnputilOutput = Invoke-WiiLandPnpUtil -Arguments @('/add-driver', (Join-Path $install 'wiiland-vhid.inf'), '/install')
    $pnpText = $pnputilOutput -join "`n"
    $published = [regex]::Matches($pnpText, '(?i)\boem\d+\.inf\b')
    if ($published.Count -eq 0) { throw 'PnPUtil did not report the published WiiLand driver INF name; refusing an installation that cannot be safely uninstalled.' }
    $newDriverInf = $published[$published.Count - 1].Value.ToLowerInvariant()
    $allDriverInfs = @($knownDriverInfs + $newDriverInf | Select-Object -Unique)
    Write-WiiLandState $statePath (New-WiiLandStateObject $rootId $allDriverInfs)

    if ($null -eq $rootId) {
        $afterDriver = @(Get-WiiLandRootDevices)
        if ($afterDriver.Count -ne 0) { throw 'A ROOT\WIILANDVHID devnode appeared during installation and is not owned by this transaction.' }
        $rootId = New-WiiLandRootDevice
        $rootCreated = $true
        Write-WiiLandState $statePath (New-WiiLandStateObject $rootId $allDriverInfs)
    }
    Invoke-WiiLandPnpUtil -Arguments @('/scan-devices') | Out-Null
    $deviceReady = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $currentRoots = @(Get-WiiLandRootDevices)
        if ($currentRoots.Count -eq 1 -and [string]$currentRoots[0].InstanceId -ieq $rootId -and
            [string]$currentRoots[0].Service -ieq (Get-WiiLandConstants).DriverServiceName -and $currentRoots[0].Started -eq $true) {
            $deviceReady = $true
            break
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $deviceReady) { throw 'The installed ROOT\WIILANDVHID device did not start with the WiiLand KMDF driver.' }
    $activeBinding = Wait-WiiLandRootBinding $rootId
    if ($null -eq $activeBinding) { throw "No signed-driver binding became available for '$rootId'; refusing to start the WiiLand broker." }
    if ([string]$activeBinding.InfName -ine $newDriverInf) {
        throw "The ROOT\WIILANDVHID device is bound to '$($activeBinding.InfName)', not the published driver package '$newDriverInf'; refusing a successful update."
    }



    $servicePath = '"' + (Join-Path $install 'wiiland-output-service.exe') + '"'
    if ($null -eq (Get-WiiLandService)) {
        Invoke-WiiLandSc -Arguments @('create', (Get-WiiLandConstants).ServiceName, 'binPath=', $servicePath, 'type=', 'own', 'start=', 'auto', 'obj=', 'LocalSystem', 'DisplayName=', 'WiiLand Output Broker') | Out-Null
        $serviceCreated = $true
    } else {
        Invoke-WiiLandSc -Arguments @('config', (Get-WiiLandConstants).ServiceName, 'binPath=', $servicePath, 'type=', 'own', 'start=', 'auto', 'obj=', 'LocalSystem') | Out-Null
    }
    Invoke-WiiLandSc -Arguments @('sidtype', (Get-WiiLandConstants).ServiceName, 'unrestricted') | Out-Null
    Assert-WiiLandServiceOwner $install (Get-WiiLandService)
    Assert-WiiLandServiceSid
    Invoke-WiiLandSc -Arguments @('start', (Get-WiiLandConstants).ServiceName) | Out-Null
    $service = Wait-WiiLandServiceState 'Running'
    Assert-WiiLandServiceOwner $install $service
    Assert-WiiLandServiceSid

    Set-WiiLandRunValue $UserSid $runCommand
    $runChanged = $true
    $finalState = New-WiiLandStateObject $rootId $allDriverInfs
    Write-WiiLandState $statePath $finalState
    $completed = $true
} catch {
    $failure = $_
    $rollbackIssues = @()
    try {
        $currentService = Get-WiiLandService
        if ($null -ne $currentService) {
            Assert-WiiLandServiceOwner $install $currentService
            if ([string]$currentService.State -eq 'Running') {
                Invoke-WiiLandSc -Arguments @('stop', (Get-WiiLandConstants).ServiceName) | Out-Null
                Wait-WiiLandServiceState 'Stopped' | Out-Null
            }
            if ($serviceCreated) { Invoke-WiiLandSc -Arguments @('delete', (Get-WiiLandConstants).ServiceName) | Out-Null }
        }
        if ($runChanged) {
            try { Restore-WiiLandRunValue $UserSid $oldRunValue }
            catch { $rollbackIssues += "Could not restore the per-user startup value: $($_.Exception.Message)" }
        }
        if ($driverInstallAttempted -and $null -eq $newDriverInf) {
            try {
                $enumText = (Invoke-WiiLandPnpUtil -Arguments @('/enum-drivers')) -join "`n"
                $candidates = @()
                foreach ($block in ($enumText -split '(?:\r?\n){2,}')) {
                    if ($block -notmatch '(?i)wiiland-vhid\.inf') { continue }
                    foreach ($match in [regex]::Matches($block, '(?i)\boem\d+\.inf\b')) {
                        $candidate = $match.Value.ToLowerInvariant()
                        if ($driverStoreNamesBefore -notcontains $candidate) { $candidates += $candidate }
                    }
                }
                $candidates = @($candidates | Select-Object -Unique)
                if ($candidates.Count -eq 1) { $newDriverInf = $candidates[0] }
                elseif ($candidates.Count -gt 1) { $rollbackIssues += 'PnPUtil created multiple unidentifiable WiiLand driver packages; they were left inactive for manual cleanup.' }
            } catch { $rollbackIssues += "PnPUtil could not identify a package created during rollback: $($_.Exception.Message)" }
        }

        if ($hadDirectory -and (Test-Path -LiteralPath $backup)) {
            if (Test-Path -LiteralPath $install) { Remove-WiiLandOwnedTree $install }
            Move-Item -LiteralPath $backup -Destination $install
        } elseif (-not $hadDirectory -and $directorySwapped -and (Test-Path -LiteralPath $install)) {
            Remove-WiiLandOwnedTree $install
        }
        $statePath = Join-Path $install (Get-WiiLandConstants).StateFileName

        if ($rootCreated -and $null -ne $rootId) {
            Remove-WiiLandRootDevice $rootId
            $rootId = $null
        }
        if ($null -ne $priorRollbackDriverInf) {
            $restoredState = Read-WiiLandState $install
            if ([string]$restoredState.installId -cne [string]$state.installId -or
                [string]$restoredState.rootDeviceInstanceId -ine $rootId -or
                -not (Test-WiiLandRollbackStateContract $restoredState) -or
                -not (Test-WiiLandRollbackPackageFiles $install) -or
                (@($restoredState.driverOemInfs) -notcontains $priorRollbackDriverInf)) {
                throw 'The restored previous installation no longer proves ownership of the original trusted broker/driver bundle.'
            }
            Invoke-WiiLandPnpUtil -Arguments @('/add-driver', (Join-Path $install 'wiiland-vhid.inf'), '/install') -ExpectedDriverInf $priorRollbackDriverInf -ExpectedRootInstanceId $rootId | Out-Null
            if (-not $priorRootWasStarted) {
                $currentRoots = @(Get-WiiLandRootDevices)
                if ($currentRoots.Count -eq 1 -and [string]$currentRoots[0].InstanceId -ieq $rootId -and $currentRoots[0].Started) {
                    Invoke-WiiLandPnpUtil -Arguments @('/disable-device', $rootId) | Out-Null
                    Wait-WiiLandRootDeviceState $rootId $false
                }
            }
            $restoredRoots = @(Get-WiiLandRootDevices)
            if ($restoredRoots.Count -ne 1 -or [string]$restoredRoots[0].InstanceId -ine $rootId -or
                [string]$restoredRoots[0].Service -ine (Get-WiiLandConstants).DriverServiceName -or
                ([bool]$restoredRoots[0].Started) -ne $priorRootWasStarted) {
                throw 'Rollback did not restore the original owned WiiLand root devnode and its prior started state.'
            }
            $restoredBinding = Get-WiiLandRootDriverBinding $rootId
            if ($null -eq $restoredBinding -or [string]$restoredBinding.InfName -ine $priorRollbackDriverInf) {
                throw "Rollback did not restore the original trusted driver binding '$priorRollbackDriverInf'."
            }
        }
        if ($null -ne $newDriverInf -and $knownDriverInfs -notcontains $newDriverInf) {
            try { Invoke-WiiLandPnpUtil -Arguments @('/delete-driver', $newDriverInf) | Out-Null }
            catch { $rollbackIssues += "The inactive new driver package '$newDriverInf' could not be removed: $($_.Exception.Message)" }
        }
        if ($rollbackIssues.Count -eq 0 -and $oldServiceWasRunning -and $null -ne $priorRollbackDriverInf) {
            Invoke-WiiLandSc -Arguments @('start', (Get-WiiLandConstants).ServiceName) | Out-Null
            Wait-WiiLandServiceState 'Running' | Out-Null
        }
        if (Test-Path -LiteralPath $stage) { Remove-WiiLandOwnedTree $stage }
    } catch {
        $rollbackIssues += $_.Exception.Message
    }
    if ($rollbackIssues.Count -gt 0) {
        $serviceStoppedForRecovery = $true
        try {
            $recoveryService = Get-WiiLandService
            if ($null -ne $recoveryService) {
                Assert-WiiLandServiceOwner $install $recoveryService
                if ([string]$recoveryService.State -eq 'Running') {
                    Invoke-WiiLandSc -Arguments @('stop', (Get-WiiLandConstants).ServiceName) | Out-Null
                    Wait-WiiLandServiceState 'Stopped' | Out-Null
                }
            }
        } catch {
            $rollbackIssues += "Could not leave the service stopped for recovery: $($_.Exception.Message)"
            $serviceStoppedForRecovery = $false
        }
        if ($serviceStoppedForRecovery -and $null -ne $rootId) {
            try {
                $recoveryRoots = @(Get-WiiLandRootDevices)
                if ($recoveryRoots.Count -gt 1) { throw 'Multiple root devnodes prevent safe recovery disable.' }
                if ($recoveryRoots.Count -eq 1) {
                    if ([string]$recoveryRoots[0].InstanceId -ine $rootId) { throw "Root '$($recoveryRoots[0].InstanceId)' is not the recorded transaction root '$rootId'." }
                    if ($recoveryRoots[0].Started) {
                        Invoke-WiiLandPnpUtil -Arguments @('/disable-device', $rootId) | Out-Null
                        Wait-WiiLandRootDeviceState $rootId $false
                    }
                }
            } catch { $rollbackIssues += "Could not safely disable the recorded root for recovery: $($_.Exception.Message)" }
        }
    }
    if ($rollbackIssues.Count -gt 0) {
        $details = $rollbackIssues -join ' '
        throw "WiiLand update failed: $($failure.Exception.Message) Automatic rollback was incomplete: $details Preserve any remaining '$backup' and '$stage' directories. Keep WiiLandOutput stopped and do not enable an unverified root driver; reboot Windows and rerun the installer after resolving the ownership/binding problem, or complete manual recovery."
    }
    throw $failure
} finally {
    if ($completed) {
        if (Test-Path -LiteralPath $backup) { Remove-WiiLandOwnedTree $backup }
        if (Test-Path -LiteralPath $stage) { Remove-WiiLandOwnedTree $stage }
    }
}

Write-Host "WiiLand $($release.Manifest.releaseVersion) installed for $($platform.Caption) (build $($platform.Build), $($platform.Architecture))."
Write-Host "WiiLandOutput is running with an unrestricted service SID; the ROOT\WIILANDVHID driver device is started."
Write-Host "The per-user WiiLandDaemon Run entry is set for $UserSid. User configuration files are not changed."
