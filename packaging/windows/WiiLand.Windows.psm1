Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:ServiceName = 'WiiLandOutput'
$script:DriverServiceName = 'WiiLandVhid'
$script:RootHardwareId = 'ROOT\WIILANDVHID'
$script:StateFileName = 'install-state.json'
$script:RunValueName = 'WiiLandDaemon'
$script:BrokerProtocolVersion = 3
$script:BrokerPipeName = '\\.\pipe\WiiLandOutput.v3'
$script:DriverAbiVersion = 2
$script:ReportLayoutVersion = 2
$script:BrokerReportRefreshMaxIntervalMs = 500
$script:BrokerReportDeadlineMs = 2000
$script:BrokerHeartbeatMaxIntervalMs = 500
$script:BrokerIdleDeadlineMs = 2000
$script:OutputContract = 'gamepad-only-v3'
$script:GamepadReportId = 1
$script:SupplementalAxesReportId = 2
$script:ExpectedActivatedDriverInf = $null

$nativeSource = @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace WiiLandPackaging {
    public static class Native {
        const uint DIGCF_ALLCLASSES = 0x00000004;
        const uint DIGCF_PRESENT = 0x00000002;
        const uint DICD_GENERATE_ID = 0x00000001;
        const uint SPDRP_HARDWAREID = 0x00000001;
        const uint SPDRP_SERVICE = 0x00000004;
        const uint DIF_REMOVE = 0x00000005;
        const uint DIF_REGISTERDEVICE = 0x00000019;
        const uint DI_REMOVEDEVICE_GLOBAL = 0x00000001;
        const uint DN_STARTED = 0x00000008;
        const int ERROR_INSUFFICIENT_BUFFER = 122;
        const uint DEVPROP_TYPE_STRING = 0x00000012;
        static readonly Guid ParentPropertyFormat = new Guid("4340a6c5-93fa-4706-972c-7b648008a5a7");
        [StructLayout(LayoutKind.Sequential)]
        struct DevPropKey {
            public Guid FormatId;
            public uint PropertyId;
        }
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiGetDeviceProperty(IntPtr Set, ref DeviceInfoData Data, ref DevPropKey Key, out uint PropertyType, byte[] Buffer, uint BufferSize, out uint RequiredSize, uint Flags);
        static readonly Guid SystemClass = new Guid("4d36e97d-e325-11ce-bfc1-08002be10318");

        [StructLayout(LayoutKind.Sequential)]
        struct DeviceInfoData {
            public uint Size;
            public Guid ClassGuid;
            public uint DevInst;
            public IntPtr Reserved;
        }
        [StructLayout(LayoutKind.Sequential)]
        struct ClassInstallHeader {
            public uint Size;
            public uint InstallFunction;
        }
        [StructLayout(LayoutKind.Sequential)]
        struct RemoveDeviceParams {
            public ClassInstallHeader Header;
            public uint Scope;
            public uint HardwareProfile;
        }

        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern IntPtr SetupDiGetClassDevs(IntPtr ClassGuid, string Enumerator, IntPtr Parent, uint Flags);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiEnumDeviceInfo(IntPtr Set, uint Index, ref DeviceInfoData Data);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiCreateDeviceInfo(IntPtr Set, string Name, ref Guid ClassGuid, string Description, IntPtr Parent, uint Flags, ref DeviceInfoData Data);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern IntPtr SetupDiCreateDeviceInfoList(ref Guid ClassGuid, IntPtr Parent);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiDestroyDeviceInfoList(IntPtr Set);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiSetDeviceRegistryProperty(IntPtr Set, ref DeviceInfoData Data, uint Property, byte[] Buffer, uint Size);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiGetDeviceRegistryProperty(IntPtr Set, ref DeviceInfoData Data, uint Property, out uint RegType, byte[] Buffer, uint BufferSize, out uint RequiredSize);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiGetDeviceInstanceId(IntPtr Set, ref DeviceInfoData Data, StringBuilder Id, int Size, out int RequiredSize);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiOpenDeviceInfo(IntPtr Set, string InstanceId, IntPtr Parent, uint Flags, ref DeviceInfoData Data);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiCallClassInstaller(uint Function, IntPtr Set, ref DeviceInfoData Data);
        [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool SetupDiSetClassInstallParams(IntPtr Set, ref DeviceInfoData Data, ref RemoveDeviceParams Params, uint Size);
        [DllImport("cfgmgr32.dll", CharSet=CharSet.Unicode)]
        static extern int CM_Get_DevNode_Status(out uint Status, out uint Problem, uint DevInst, uint Flags);

        static DeviceInfoData NewData() {
            DeviceInfoData data = new DeviceInfoData();
            data.Size = (uint)Marshal.SizeOf(typeof(DeviceInfoData));
            return data;
        }
        static string InstanceId(IntPtr set, ref DeviceInfoData data) {
            int needed;
            SetupDiGetDeviceInstanceId(set, ref data, null, 0, out needed);
            int error = Marshal.GetLastWin32Error();
            if (needed <= 1 && error != ERROR_INSUFFICIENT_BUFFER) throw new Win32Exception(error);
            StringBuilder id = new StringBuilder(needed);
            if (!SetupDiGetDeviceInstanceId(set, ref data, id, id.Capacity, out needed)) throw new Win32Exception(Marshal.GetLastWin32Error());
            return id.ToString();
        }
        static string[] MultiStringProperty(IntPtr set, ref DeviceInfoData data, uint property) {
            uint type, needed;
            SetupDiGetDeviceRegistryProperty(set, ref data, property, out type, null, 0, out needed);
            int error = Marshal.GetLastWin32Error();
            if (needed == 0) return new string[0];
            if (error != ERROR_INSUFFICIENT_BUFFER) throw new Win32Exception(error);
            byte[] bytes = new byte[needed];
            if (!SetupDiGetDeviceRegistryProperty(set, ref data, property, out type, bytes, (uint)bytes.Length, out needed)) throw new Win32Exception(Marshal.GetLastWin32Error());
            return Encoding.Unicode.GetString(bytes).TrimEnd('\0').Split(new char[] { '\0' }, StringSplitOptions.RemoveEmptyEntries);
        }
        static string StringProperty(IntPtr set, ref DeviceInfoData data, uint property) {
            uint type, needed;
            SetupDiGetDeviceRegistryProperty(set, ref data, property, out type, null, 0, out needed);
            int error = Marshal.GetLastWin32Error();
            if (needed == 0) return String.Empty;
            if (error != ERROR_INSUFFICIENT_BUFFER) throw new Win32Exception(error);
            byte[] bytes = new byte[needed];
            if (!SetupDiGetDeviceRegistryProperty(set, ref data, property, out type, bytes, (uint)bytes.Length, out needed)) throw new Win32Exception(Marshal.GetLastWin32Error());
            return Encoding.Unicode.GetString(bytes).TrimEnd('\0');
        }
        static string ParentInstanceId(IntPtr set, ref DeviceInfoData data) {
            DevPropKey key = new DevPropKey();
            key.FormatId = ParentPropertyFormat;
            key.PropertyId = 8;
            uint type, needed;
            bool first = SetupDiGetDeviceProperty(set, ref data, ref key, out type, null, 0, out needed, 0);
            int error = Marshal.GetLastWin32Error();
            if (needed == 0 || (!first && error != ERROR_INSUFFICIENT_BUFFER) || type != DEVPROP_TYPE_STRING) return String.Empty;
            byte[] bytes = new byte[needed];
            if (!SetupDiGetDeviceProperty(set, ref data, ref key, out type, bytes, (uint)bytes.Length, out needed, 0)) return String.Empty;
            return Encoding.Unicode.GetString(bytes).TrimEnd('\0');
        }
        static bool Started(DeviceInfoData data) {
            uint status, problem;
            return CM_Get_DevNode_Status(out status, out problem, data.DevInst, 0) == 0 && (status & DN_STARTED) != 0 && problem == 0;
        }
        static IntPtr RootSet() {
            IntPtr set = SetupDiGetClassDevs(IntPtr.Zero, "ROOT", IntPtr.Zero, DIGCF_ALLCLASSES);
            if (set == new IntPtr(-1)) throw new Win32Exception(Marshal.GetLastWin32Error());
            return set;
        }
        public static string[] FindRootDevices(string hardwareId) {
            List<string> result = new List<string>();
            IntPtr set = RootSet();
            try {
                for (uint i = 0; ; i++) {
                    DeviceInfoData data = NewData();
                    if (!SetupDiEnumDeviceInfo(set, i, ref data)) {
                        int error = Marshal.GetLastWin32Error();
                        if (error == 259) break;
                        throw new Win32Exception(error);
                    }
                    string[] ids = MultiStringProperty(set, ref data, SPDRP_HARDWAREID);
                    bool match = false;
                    foreach (string id in ids) if (String.Equals(id, hardwareId, StringComparison.OrdinalIgnoreCase)) match = true;
                    if (match) result.Add(InstanceId(set, ref data));
                }
            } finally { SetupDiDestroyDeviceInfoList(set); }
            return result.ToArray();
        }
        public static string[] FindHidChildren(string parentInstanceId) {
            List<string> result = new List<string>();
            IntPtr set = SetupDiGetClassDevs(IntPtr.Zero, "HID", IntPtr.Zero, DIGCF_ALLCLASSES | DIGCF_PRESENT);
            if (set == new IntPtr(-1)) throw new Win32Exception(Marshal.GetLastWin32Error());
            try {
                for (uint i = 0; ; i++) {
                    DeviceInfoData data = NewData();
                    if (!SetupDiEnumDeviceInfo(set, i, ref data)) {
                        int error = Marshal.GetLastWin32Error();
                        if (error == 259) break;
                        throw new Win32Exception(error);
                    }
                    string id = InstanceId(set, ref data);
                    if (!id.StartsWith("HID\\", StringComparison.OrdinalIgnoreCase)) continue;
                    if (String.Equals(ParentInstanceId(set, ref data), parentInstanceId, StringComparison.OrdinalIgnoreCase)) result.Add(id);
                }
            } finally { SetupDiDestroyDeviceInfoList(set); }
            return result.ToArray();
        }
        public static string GetDeviceService(string instanceId) {
            IntPtr set = RootSet();
            try {
                DeviceInfoData data = NewData();
                if (!SetupDiOpenDeviceInfo(set, instanceId, IntPtr.Zero, 0, ref data)) return String.Empty;
                return StringProperty(set, ref data, SPDRP_SERVICE);
            } finally { SetupDiDestroyDeviceInfoList(set); }
        }
        public static bool IsDeviceStarted(string instanceId) {
            IntPtr set = RootSet();
            try {
                DeviceInfoData data = NewData();
                if (!SetupDiOpenDeviceInfo(set, instanceId, IntPtr.Zero, 0, ref data)) return false;
                return Started(data);
            } finally { SetupDiDestroyDeviceInfoList(set); }
        }
        public static string CreateRootDevice(string hardwareId, string description) {
            Guid classGuid = SystemClass;
            IntPtr set = SetupDiCreateDeviceInfoList(ref classGuid, IntPtr.Zero);
            if (set == new IntPtr(-1)) throw new Win32Exception(Marshal.GetLastWin32Error());
            try {
                DeviceInfoData data = NewData();
                string deviceName = hardwareId.Substring("ROOT\\".Length);
                if (!SetupDiCreateDeviceInfo(set, deviceName, ref classGuid, description, IntPtr.Zero, DICD_GENERATE_ID, ref data)) throw new Win32Exception(Marshal.GetLastWin32Error());
                byte[] ids = Encoding.Unicode.GetBytes(hardwareId + "\0\0");
                if (!SetupDiSetDeviceRegistryProperty(set, ref data, SPDRP_HARDWAREID, ids, (uint)ids.Length)) throw new Win32Exception(Marshal.GetLastWin32Error());
                if (!SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, ref data)) throw new Win32Exception(Marshal.GetLastWin32Error());
                return InstanceId(set, ref data);
            } finally { SetupDiDestroyDeviceInfoList(set); }
        }
        public static void RemoveRootDevice(string instanceId, string hardwareId) {
            IntPtr set = RootSet();
            try {
                DeviceInfoData data = NewData();
                if (!SetupDiOpenDeviceInfo(set, instanceId, IntPtr.Zero, 0, ref data)) {
                    int error = Marshal.GetLastWin32Error();
                    if (error == 1168) return;
                    throw new Win32Exception(error);
                }
                string[] ids = MultiStringProperty(set, ref data, SPDRP_HARDWAREID);
                bool match = false;
                foreach (string id in ids) if (String.Equals(id, hardwareId, StringComparison.OrdinalIgnoreCase)) match = true;
                if (!match || !instanceId.StartsWith("ROOT\\WIILANDVHID\\", StringComparison.OrdinalIgnoreCase)) throw new InvalidOperationException("Refusing to remove a device that is not the recorded WiiLand root devnode.");
                RemoveDeviceParams parameters = new RemoveDeviceParams();
                parameters.Header.Size = (uint)Marshal.SizeOf(typeof(ClassInstallHeader));
                parameters.Header.InstallFunction = DIF_REMOVE;
                parameters.Scope = DI_REMOVEDEVICE_GLOBAL;
                parameters.HardwareProfile = 0;
                if (!SetupDiSetClassInstallParams(set, ref data, ref parameters, (uint)Marshal.SizeOf(typeof(RemoveDeviceParams)))) throw new Win32Exception(Marshal.GetLastWin32Error());
                if (!SetupDiCallClassInstaller(DIF_REMOVE, set, ref data)) throw new Win32Exception(Marshal.GetLastWin32Error());
            } finally { SetupDiDestroyDeviceInfoList(set); }
        }
    }
}
'@
if (-not ('WiiLandPackaging.Native' -as [type])) {
    Add-Type -TypeDefinition $nativeSource -Language CSharp
}

function Get-WiiLandConstants {
    [pscustomobject]@{
        ServiceName = $script:ServiceName
        DriverServiceName = $script:DriverServiceName
        RootHardwareId = $script:RootHardwareId
        StateFileName = $script:StateFileName
        RunValueName = $script:RunValueName
        BrokerProtocolVersion = $script:BrokerProtocolVersion
        BrokerPipeName = $script:BrokerPipeName
        BrokerReportRefreshMaxIntervalMs = $script:BrokerReportRefreshMaxIntervalMs
        BrokerReportDeadlineMs = $script:BrokerReportDeadlineMs
        BrokerHeartbeatMaxIntervalMs = $script:BrokerHeartbeatMaxIntervalMs
        BrokerIdleDeadlineMs = $script:BrokerIdleDeadlineMs
        DriverAbiVersion = $script:DriverAbiVersion
        ReportLayoutVersion = $script:ReportLayoutVersion
    }
}
function Assert-WiiLandAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run this script from an elevated PowerShell session.'
    }
}

function Get-WiiLandPlatform {
    if (-not [Environment]::Is64BitProcess) { throw 'Use 64-bit PowerShell to install a WiiLand Windows package.' }
    $os = Get-CimInstance -ClassName Win32_OperatingSystem
    $build = [int]$os.BuildNumber
    if ($os.ProductType -ne 1 -or $build -lt 19045) {
        throw "WiiLand Windows packaging requires Windows 10 22H2 (build 19045) or Windows 11; detected build $build."
    }
    $processor = Get-CimInstance -ClassName Win32_Processor | Select-Object -First 1
    $architecture = switch ([int]$processor.Architecture) {
        9 { 'x64' }
        12 { 'ARM64' }
        default { throw "Unsupported Windows processor architecture code $($processor.Architecture)." }
    }
    [pscustomobject]@{ Build = $build; Architecture = $architecture; Caption = [string]$os.Caption }
}
function Assert-WiiLandProductionBoot {
    $bcdedit = Join-Path $env:SystemRoot 'System32\bcdedit.exe'
    $output = & $bcdedit '/enum' '{current}' 2>&1
    if ($LASTEXITCODE -ne 0) { throw "Could not verify Windows code-integrity boot settings: $($output -join ' ')" }
    $text = $output -join "`n"
    if ($text -match '(?im)^\s*(testsigning|nointegritychecks)\s+Yes\s*$') {
        throw 'Refusing a production install while Windows test signing or disabled code-integrity checks are enabled.'
    }
}


function ConvertTo-WiiLandId([object]$Value, [string]$Name) {
    if ($null -eq $Value) { throw "Missing $Name in release metadata." }
    $text = [string]$Value
    try {
        if ($text -match '^0[xX][0-9A-Fa-f]{1,4}$') { return [Convert]::ToUInt16($text.Substring(2), 16) }
        return [UInt16]::Parse($text, [Globalization.CultureInfo]::InvariantCulture)
    } catch { throw "Invalid $Name '$text'; use a 16-bit decimal value or 0x-prefixed hexadecimal value." }
}

function Get-WiiLandSha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Assert-WiiLandHash([string]$Path, [object]$Expected, [string]$Label) {
    $expectedText = ([string]$Expected).ToLowerInvariant()
    if ($expectedText -notmatch '^[0-9a-f]{64}$') { throw "Release metadata has an invalid SHA-256 for $Label." }
    $actual = Get-WiiLandSha256 $Path
    if ($actual -ne $expectedText) { throw "SHA-256 mismatch for $Label." }
}

function Assert-WiiLandPlainFile([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Required $Label is missing: $Path" }
    $item = Get-Item -LiteralPath $Path -Force
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Refusing reparse-point $($Label): $Path" }
}

function Get-WiiLandRelease([string]$PackageRoot) {
    $root = (Resolve-Path -LiteralPath $PackageRoot -ErrorAction Stop).ProviderPath
    if (-not (Test-Path -LiteralPath $root -PathType Container)) { throw "Package root is not a directory: $PackageRoot" }
    $manifestPath = Join-Path $root 'wiiland-release.json'
    Assert-WiiLandPlainFile $manifestPath 'release manifest'
    try { $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json } catch { throw "Cannot parse wiiland-release.json: $($_.Exception.Message)" }
    if ([int]$manifest.formatVersion -ne 2) { throw 'Unsupported or missing release manifest formatVersion (required: 2 for the v3 gamepad-only output contract).' }
    if ([string]$manifest.rootHardwareId -cne $script:RootHardwareId) { throw "Release must target exactly $($script:RootHardwareId)." }
    $contract = [ordered]@{
        outputContract = $script:OutputContract
        brokerProtocolVersion = $script:BrokerProtocolVersion
        brokerPipeName = $script:BrokerPipeName
        reportRefreshMaxIntervalMs = $script:BrokerReportRefreshMaxIntervalMs
        reportDeadlineMs = $script:BrokerReportDeadlineMs
        heartbeatMaxIntervalMs = $script:BrokerHeartbeatMaxIntervalMs
        idleDeadlineMs = $script:BrokerIdleDeadlineMs
        driverAbiVersion = $script:DriverAbiVersion
        reportLayoutVersion = $script:ReportLayoutVersion
        gamepadReportId = $script:GamepadReportId
        supplementalAxesReportId = $script:SupplementalAxesReportId
        desktopInput = 'per-user-sendinput'
    }
    foreach ($name in $contract.Keys) {
        $property = $manifest.PSObject.Properties[$name]
        if ($null -eq $property -or [string]$property.Value -cne [string]$contract[$name]) {
            throw "Release metadata does not declare the required v3 output contract field '$name' as '$($contract[$name])'."
        }
    }
    if ($manifest.identityAuthorization.authorized -ne $true -or [string]::IsNullOrWhiteSpace([string]$manifest.identityAuthorization.reference)) {
        throw 'Release metadata lacks an authorized, assigned VID/PID approval reference.'
    }
    $vid = ConvertTo-WiiLandId $manifest.hidVendorId 'hidVendorId'
    $pid = ConvertTo-WiiLandId $manifest.hidProductId 'hidProductId'
    if ($vid -eq 0 -or $vid -eq 0xffff -or $pid -eq 0) { throw 'The reserved/prototype HID identity is not installable; use an authorized, non-reserved VID/PID.' }
    if (-not $manifest.files) { throw 'Release metadata has no files SHA-256 map.' }

    $expectedFiles = @('wiilandd.exe', 'wiiland-output-service.exe', 'wiiland-vhid.inf', 'wiiland-vhid.sys', 'wiiland-vhid.cat')
    foreach ($name in $expectedFiles) {
        $path = Join-Path $root $name
        Assert-WiiLandPlainFile $path $name
        $hash = $manifest.files.PSObject.Properties[$name]
        if ($null -eq $hash) { throw "Release metadata has no SHA-256 for $name." }
        Assert-WiiLandHash $path $hash.Value $name
    }
    if ([string]::IsNullOrWhiteSpace([string]$manifest.releaseVersion)) { throw 'Release metadata has no releaseVersion.' }
    if ([string]$manifest.architecture -notin @('x64', 'ARM64')) { throw 'Release architecture must be x64 or ARM64.' }
    $inf = Get-Content -LiteralPath (Join-Path $root 'wiiland-vhid.inf') -Raw
    if ($inf -notmatch '(?im)^\s*CatalogFile\s*=\s*wiiland-vhid\.cat\s*$' -or $inf -notmatch '(?i)ROOT\\WIILANDVHID') {
        throw 'The INF must catalog the supplied CAT and match exactly ROOT\WIILANDVHID.'
    }
    $signature = Get-AuthenticodeSignature -LiteralPath (Join-Path $root 'wiiland-vhid.cat')
    if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid) { throw "Driver catalog signature is not valid and trusted (status: $($signature.Status))." }

    [pscustomobject]@{ Root = $root; Manifest = $manifest; VendorId = $vid; ProductId = $pid }
}

function Get-WiiLandCanonicalPath([string]$Path) {
    [IO.Path]::GetFullPath($Path).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
}

function Assert-WiiLandInstallDirectory([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path) -or -not [IO.Path]::IsPathRooted($Path)) { throw 'InstallDirectory must be an absolute path.' }
    $full = Get-WiiLandCanonicalPath $Path
    $programFiles = Get-WiiLandCanonicalPath $env:ProgramFiles
    $expectedParent = Get-WiiLandCanonicalPath (Split-Path -Parent $full)
    if ($expectedParent -ine $programFiles -or (Split-Path -Leaf $full) -ine 'WiiLand') {
        throw "InstallDirectory must be exactly '$programFiles\WiiLand'; refusing an alternate or user-writable service location."
    }
    if (Test-Path -LiteralPath $full) {
        $rootItem = Get-Item -LiteralPath $full -Force
        if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'InstallDirectory must not be a reparse point.' }
    }
    return $full
}
function Assert-WiiLandOwnedContents([string]$Path) {
    $allowed = @('install-state.json', 'wiilandd.exe', 'wiiland-output-service.exe', 'wiiland-vhid.inf', 'wiiland-vhid.sys', 'wiiland-vhid.cat')
    foreach ($item in @(Get-ChildItem -LiteralPath $Path -Force)) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Refusing a reparse point in WiiLand installation: $($item.FullName)" }
        if ($item.PSIsContainer -or $allowed -notcontains $item.Name) { throw "Refusing to modify unowned installation content: $($item.FullName)" }
    }
}


function Read-WiiLandState([string]$InstallDirectory) {
    $statePath = Join-Path $InstallDirectory $script:StateFileName
    if (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) { return $null }
    Assert-WiiLandPlainFile $statePath 'installation state'
    try { $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json } catch { throw "Installation state is unreadable: $($_.Exception.Message)" }
    if ([int]$state.formatVersion -ne 1 -or [string]$state.installDirectory -ine $InstallDirectory -or [string]$state.serviceName -cne $script:ServiceName -or
        [string]$state.rootHardwareId -cne $script:RootHardwareId -or [string]$state.installId -notmatch '^[0-9a-fA-F-]{36}$') {
        throw 'Installation state does not prove ownership of this WiiLand installation.'
    }
    $stateVid = ConvertTo-WiiLandId $state.hidVendorId 'installation hidVendorId'
    $statePid = ConvertTo-WiiLandId $state.hidProductId 'installation hidProductId'
    if ($stateVid -eq 0 -or $stateVid -eq 0xffff -or $statePid -eq 0) { throw 'Installation state contains a reserved/prototype HID identity.' }
    if ($state.rootDeviceInstanceId -and [string]$state.rootDeviceInstanceId -notmatch '(?i)^ROOT\\WIILANDVHID\\[^\\]+$') { throw 'Recorded root devnode ID is not a WiiLand ROOT\WIILANDVHID instance.' }
    foreach ($oem in @($state.driverOemInfs)) { if ([string]$oem -notmatch '(?i)^oem\d+\.inf$') { throw 'Installation state contains an invalid published driver INF name.' } }
    if ([string]$state.runUserSid -notmatch '^S-1-(?:\d+-)+\d+$') { throw 'Installation state has no valid per-user startup SID.' }
    $expectedRun = '"' + (Join-Path $InstallDirectory 'wiilandd.exe') + '"'
    if ([string]$state.runCommand -cne $expectedRun) { throw 'Installation state does not identify the WiiLand daemon startup command.' }
    return $state
}
function Assert-WiiLandServiceConfiguration {
    $service = Get-WiiLandService
    if ($null -eq $service) { return }
    if ([string]$service.StartMode -cne 'Auto') { throw "The WiiLand service start mode was changed; refusing to overwrite the user's service configuration." }
    Assert-WiiLandServiceSid
}

function Write-WiiLandState([string]$Path, [object]$State) {
    $temp = "$Path.$([guid]::NewGuid().ToString('N')).tmp"
    $json = $State | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($temp, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temp -Destination $Path -Force
}

function Get-WiiLandRootDevices {
    $ids = [WiiLandPackaging.Native]::FindRootDevices($script:RootHardwareId)
    foreach ($id in $ids) {
        [pscustomobject]@{
            InstanceId = $id
            Service = [WiiLandPackaging.Native]::GetDeviceService($id)
            Started = [WiiLandPackaging.Native]::IsDeviceStarted($id)
        }
    }
}

function Get-WiiLandRootDriverBinding([string]$InstanceId) {
    if ([string]$InstanceId -notmatch '(?i)^ROOT\\WIILANDVHID\\[^\\]+$') {
        throw "Not a WiiLand root device instance ID: $InstanceId"
    }
    $bindings = @(Get-CimInstance -ClassName Win32_PnPSignedDriver -ErrorAction Stop |
        Where-Object { [string]$_.DeviceID -ieq $InstanceId })
    if ($bindings.Count -gt 1) {
        throw "Expected at most one signed-driver binding for '$InstanceId', found $($bindings.Count)."
    }
    if ($bindings.Count -eq 0) { return $null }
    [pscustomobject]@{
        InstanceId = [string]$bindings[0].DeviceID
        InfName = [string]$bindings[0].InfName
        DriverVersion = [string]$bindings[0].DriverVersion
    }
}

function Wait-WiiLandRootDriverBinding([string]$InstanceId, [int]$TimeoutSeconds = 15) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $binding = Get-WiiLandRootDriverBinding $InstanceId
        if ($null -ne $binding) { return $binding }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Driver binding for '$InstanceId' did not become available while the root device was disabled; refusing to enable it."
}

function Assert-WiiLandRootV3EnableReady([string]$InstanceId, [switch]$AllowV2Rollback) {
    $state = Read-WiiLandState (Join-Path $env:ProgramFiles 'WiiLand')
    Assert-WiiLandV3StateContract $state 'installation state' -AllowV2Rollback:$AllowV2Rollback
    $expectedInf = $script:ExpectedActivatedDriverInf
    if ([string]::IsNullOrWhiteSpace([string]$expectedInf)) {
        throw 'Refusing to enable the WiiLand root device without a verified driver activation in this installer session.'
    }
    if ($AllowV2Rollback -and (@($state.driverOemInfs) -notcontains [string]$expectedInf)) {
        throw "Refusing rollback activation of unowned driver package '$expectedInf'; the root remains disabled."
    }
    $binding = Wait-WiiLandRootDriverBinding $InstanceId
    if ([string]$binding.InfName -ine [string]$expectedInf) {
        throw "Refusing to enable the WiiLand root device: its binding '$($binding.InfName)' does not match the activated package '$expectedInf'. The root remains disabled."
    }
    $binding
}

function Test-WiiLandV3StateContract([object]$State) {
    if ($null -eq $State) { return $false }
    $expected = [ordered]@{
        outputContract = $script:OutputContract
        brokerProtocolVersion = $script:BrokerProtocolVersion
        brokerPipeName = $script:BrokerPipeName
        reportRefreshMaxIntervalMs = $script:BrokerReportRefreshMaxIntervalMs
        reportDeadlineMs = $script:BrokerReportDeadlineMs
        heartbeatMaxIntervalMs = $script:BrokerHeartbeatMaxIntervalMs
        idleDeadlineMs = $script:BrokerIdleDeadlineMs
        driverAbiVersion = $script:DriverAbiVersion
        reportLayoutVersion = $script:ReportLayoutVersion
    }
    foreach ($name in $expected.Keys) {
        $property = $State.PSObject.Properties[$name]
        if ($null -eq $property -or [string]$property.Value -cne [string]$expected[$name]) { return $false }
    }
    $idProperty = $State.PSObject.Properties['reportIds']
    if ($null -eq $idProperty) { return $false }
    try {
        $reportIds = @($idProperty.Value)
        return $reportIds.Count -eq 2 -and [int]$reportIds[0] -eq $script:GamepadReportId -and
            [int]$reportIds[1] -eq $script:SupplementalAxesReportId
    } catch { return $false }
}

function Test-WiiLandV2RollbackStateContract([object]$State) {
    if ($null -eq $State) { return $false }
    $expected = [ordered]@{
        outputContract = 'gamepad-only-v2'
        brokerProtocolVersion = 2
        driverAbiVersion = $script:DriverAbiVersion
        reportLayoutVersion = $script:ReportLayoutVersion
    }
    foreach ($name in $expected.Keys) {
        $property = $State.PSObject.Properties[$name]
        if ($null -eq $property -or [string]$property.Value -cne [string]$expected[$name]) { return $false }
    }
    foreach ($name in @('brokerPipeName', 'heartbeatMaxIntervalMs', 'idleDeadlineMs')) {
        $property = $State.PSObject.Properties[$name]
        if ($null -ne $property) {
            $legacyValue = switch ($name) {
                'brokerPipeName' { '\\.\pipe\WiiLandOutput.v2' }
                'heartbeatMaxIntervalMs' { 500 }
                'idleDeadlineMs' { 2000 }
            }
            if ([string]$property.Value -cne [string]$legacyValue) { return $false }
        }
    }
    if ($null -ne $State.PSObject.Properties['reportRefreshMaxIntervalMs'] -or
        $null -ne $State.PSObject.Properties['reportDeadlineMs']) { return $false }
    $idProperty = $State.PSObject.Properties['reportIds']
    if ($null -eq $idProperty) { return $false }
    try {
        $reportIds = @($idProperty.Value)
        return $reportIds.Count -eq 2 -and [int]$reportIds[0] -eq $script:GamepadReportId -and
            [int]$reportIds[1] -eq $script:SupplementalAxesReportId
    } catch { return $false }
}
function Test-WiiLandRollbackStateContract([object]$State) {
    if (Test-WiiLandV3StateContract $State) { return $true }
    return (Test-WiiLandV2RollbackStateContract $State)
}

function Test-WiiLandRollbackPackageFiles([string]$Directory) {
    try {
        foreach ($name in @('wiilandd.exe', 'wiiland-output-service.exe', 'wiiland-vhid.inf', 'wiiland-vhid.sys', 'wiiland-vhid.cat')) {
            Assert-WiiLandPlainFile (Join-Path $Directory $name) "previous installation file $name"
        }
        $inf = Get-Content -LiteralPath (Join-Path $Directory 'wiiland-vhid.inf') -Raw
        if ($inf -notmatch '(?im)^\s*CatalogFile\s*=\s*wiiland-vhid\.cat\s*$' -or $inf -notmatch '(?i)ROOT\\WIILANDVHID') { return $false }
        $signature = Get-AuthenticodeSignature -LiteralPath (Join-Path $Directory 'wiiland-vhid.cat')
        return $signature.Status -eq [Management.Automation.SignatureStatus]::Valid
    } catch { return $false }
}

function Assert-WiiLandV3StateContract([object]$State, [string]$Context, [switch]$AllowV2Rollback) {
    if (Test-WiiLandV3StateContract $State) { return }
    if ($AllowV2Rollback -and (Test-WiiLandV2RollbackStateContract $State)) { return }
    throw "$Context lacks a complete supported WiiLand broker/report contract; refusing privileged broker or driver activation."
}
function Wait-WiiLandRootDeviceState([string]$InstanceId, [bool]$Started, [int]$TimeoutSeconds = 15) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $device = @(Get-WiiLandRootDevices | Where-Object { [string]$_.InstanceId -ieq $InstanceId }) | Select-Object -First 1
        if ($Started -and $null -ne $device -and $device.Started) { return }
        if (-not $Started -and ($null -eq $device -or -not $device.Started)) { return }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($Started) {
        throw "WiiLand root devnode '$InstanceId' did not reach the started state after package activation. Reboot Windows to clear a stale driver stack, then rerun the installer; installation did not succeed."
    }
    throw "WiiLand root devnode '$InstanceId' did not stop after disable. Reboot Windows to unload the old driver stack, then rerun the installer; no package activation was attempted."
}
function Get-WiiLandDriverState {
    Get-CimInstance -ClassName Win32_SystemDriver -Filter "Name='$script:DriverServiceName'" -ErrorAction Stop
}
function Wait-WiiLandDriverStopped([int]$TimeoutSeconds = 15) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $driver = Get-WiiLandDriverState
        if ($null -eq $driver -or [string]$driver.State -ceq 'Stopped') { return }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'The old WiiLand HID driver is still loaded after its root devnode was disabled. Reboot Windows to unload the stale stack, then rerun the installer; no package activation was attempted.'
}
function Assert-WiiLandHidStack {
    $roots = @(Get-WiiLandRootDevices)
    if ($roots.Count -ne 1 -or -not $roots[0].Started -or [string]$roots[0].Service -ine $script:DriverServiceName) {
        throw 'Refusing to start WiiLandOutput without one started ROOT\WIILANDVHID device bound to WiiLandVhid.'
    }
    $state = Read-WiiLandState (Join-Path $env:ProgramFiles 'WiiLand')
    $expectedInf = $script:ExpectedActivatedDriverInf
    $allowV2Rollback = -not [string]::IsNullOrWhiteSpace([string]$expectedInf)
    Assert-WiiLandV3StateContract $state 'installation state' -AllowV2Rollback:$allowV2Rollback
    $binding = Get-WiiLandRootDriverBinding ([string]$roots[0].InstanceId)
    if ($null -eq $binding) { throw "Root driver binding for '$($roots[0].InstanceId)' is not ready; refusing broker startup." }
    if ([string]::IsNullOrWhiteSpace([string]$expectedInf)) {
        $recordedInfs = @($state.driverOemInfs)
        if ($recordedInfs.Count -eq 0) { throw 'Refusing to start WiiLandOutput without a recorded driver package.' }
        $expectedInf = [string]$recordedInfs[-1]
    } elseif ($allowV2Rollback -and (@($state.driverOemInfs) -notcontains [string]$expectedInf)) {
        throw "Refusing broker startup with unowned rollback driver package '$expectedInf'."
    }
    if ([string]$binding.InfName -ine [string]$expectedInf) {
        throw "Refusing to start WiiLandOutput: root device is bound to '$($binding.InfName)', not the activated package '$expectedInf'. Reboot Windows and rerun the installer; the stale driver stack was not accepted."
    }
    $children = @(Get-WiiLandHidChildren ([string]$roots[0].InstanceId))
    if ($children.Count -ne 0) {
        throw "Refusing to start WiiLandOutput while $($children.Count) HID children are already present before any gamepad lease. A stale stack may still expose v1 keyboard/mouse collections; reboot Windows and rerun the installer."
    }
}
function Get-WiiLandHidChildren([string]$RootInstanceId) {
    @([WiiLandPackaging.Native]::FindHidChildren($RootInstanceId))
}


function New-WiiLandRootDevice([switch]$AllowV2Rollback) {
    $state = Read-WiiLandState (Join-Path $env:ProgramFiles 'WiiLand')
    Assert-WiiLandV3StateContract $state 'installation state' -AllowV2Rollback:$AllowV2Rollback
    if ([string]::IsNullOrWhiteSpace([string]$script:ExpectedActivatedDriverInf)) {
        throw 'Refusing to create the WiiLand root device without a verified driver package activation in this installer session.'
    }
    if ($AllowV2Rollback -and (@($state.driverOemInfs) -notcontains [string]$script:ExpectedActivatedDriverInf)) {
        throw "Refusing to create the root device for unowned rollback package '$script:ExpectedActivatedDriverInf'."
    }
    [WiiLandPackaging.Native]::CreateRootDevice($script:RootHardwareId, 'WiiLand Virtual HID Source')
}

function Remove-WiiLandRootDevice([string]$InstanceId) {
    [WiiLandPackaging.Native]::RemoveRootDevice($InstanceId, $script:RootHardwareId)
}

function Invoke-WiiLandPnpUtil([string[]]$Arguments, [string]$ExpectedDriverInf, [string]$ExpectedRootInstanceId, [switch]$AllowMissingRollbackRoot) {
    $pnputil = Join-Path $env:SystemRoot 'System32\pnputil.exe'
    $driverActivation = $Arguments.Count -ge 3 -and $Arguments[0] -ieq '/add-driver' -and $Arguments -contains '/install'
    $allowV2Rollback = -not [string]::IsNullOrWhiteSpace($ExpectedDriverInf)
    if ($allowV2Rollback -and -not $driverActivation) {
        throw 'An expected rollback INF is valid only for an /add-driver /install activation.'
    }
    if ($AllowMissingRollbackRoot -and -not $allowV2Rollback) {
        throw 'A missing rollback root is permitted only for an explicitly identified owned package activation.'
    }
    if ($driverActivation) {
        $infPath = [IO.Path]::GetFullPath([string]$Arguments[1])
        $statePath = Join-Path (Split-Path -Parent $infPath) $script:StateFileName
        $packageState = Read-WiiLandState (Split-Path -Parent $statePath)
        if ($null -eq $packageState) { throw 'Driver activation state does not prove an owned WiiLand installation.' }
        Assert-WiiLandV3StateContract $packageState 'driver activation state' -AllowV2Rollback:$allowV2Rollback
        if ($allowV2Rollback) {
            if ($ExpectedDriverInf -notmatch '(?i)^oem\d+\.inf$' -or (@($packageState.driverOemInfs) -notcontains $ExpectedDriverInf)) {
                throw "Rollback package '$ExpectedDriverInf' is not an owned published INF in the restored installation state."
            }
            if ($ExpectedRootInstanceId -notmatch '(?i)^ROOT\\WIILANDVHID\\[^\\]+$' -or
                [string]$packageState.rootDeviceInstanceId -ine $ExpectedRootInstanceId) {
                throw "Rollback root '$ExpectedRootInstanceId' does not match the exact recorded root in the restored installation state."
            }
        }
        $script:ExpectedActivatedDriverInf = $null
    }
    $rootEnable = $Arguments.Count -ge 2 -and $Arguments[0] -ieq '/enable-device'
    if ($rootEnable -and [string]$Arguments[1] -match '(?i)^ROOT\\WIILANDVHID\\[^\\]+$') {
        Assert-WiiLandRootV3EnableReady ([string]$Arguments[1]) | Out-Null
    }
    $disabledRoot = $null
    $activationSucceeded = $false
    if ($driverActivation) {
        $broker = Get-WiiLandService
        if ($null -ne $broker -and [string]$broker.State -cne 'Stopped') {
            throw 'Stop the WiiLandOutput broker before activating a driver package; refusing to update a live broker/driver stack.'
        }
        $roots = @(Get-WiiLandRootDevices)
        if ($roots.Count -gt 1) { throw 'Multiple WiiLand root devices exist; refusing driver activation.' }
        if ($allowV2Rollback) {
            if ($roots.Count -eq 0 -and -not $AllowMissingRollbackRoot) {
                throw 'The exact recorded root devnode is missing; refusing rollback activation without its original binding.'
            }
            if ($roots.Count -eq 1 -and [string]$roots[0].InstanceId -ine $ExpectedRootInstanceId) {
                throw "Rollback found root '$($roots[0].InstanceId)' instead of the exact recorded root '$ExpectedRootInstanceId'; refusing activation."
            }
        }
        if ($roots.Count -eq 1) {
            $disabledRoot = [string]$roots[0].InstanceId
            if ($roots[0].Started) {
                $disableOutput = & $pnputil '/disable-device' $disabledRoot 2>&1
                if ($LASTEXITCODE -ne 0) { throw "pnputil /disable-device failed ($LASTEXITCODE): $($disableOutput -join ' ')" }
                Wait-WiiLandRootDeviceState $disabledRoot $false
            }
        }
        Wait-WiiLandDriverStopped
        $broker = Get-WiiLandService
        if ($null -ne $broker -and [string]$broker.State -cne 'Stopped') {
            throw 'The WiiLandOutput broker became live before package activation; refusing to modify its driver stack.'
        }
        if ($null -ne $disabledRoot) { Wait-WiiLandRootDeviceState $disabledRoot $false }
        Wait-WiiLandDriverStopped
    }
    try {
        $output = & $pnputil @Arguments 2>&1
        $exitCode = $LASTEXITCODE
        if ($exitCode -ne 0) { throw "pnputil $($Arguments -join ' ') failed ($exitCode): $($output -join ' ')" }
        if ($driverActivation) {
            $published = [regex]::Matches(($output -join "`n"), '(?i)\boem\d+\.inf\b')
            if ($published.Count -eq 0 -and -not $allowV2Rollback) { throw 'PnPUtil did not report the published driver INF name; refusing to enable the root device.' }
            $activatedInf = if ($published.Count -gt 0) { $published[$published.Count - 1].Value.ToLowerInvariant() } else { $ExpectedDriverInf }
            if ($allowV2Rollback -and $activatedInf -ine $ExpectedDriverInf) {
                throw "Rollback activation published '$activatedInf', not the exact owned previous package '$ExpectedDriverInf'; the root remains disabled."
            }
            $script:ExpectedActivatedDriverInf = $activatedInf
            if ($allowV2Rollback -and $null -ne $disabledRoot) {
                $binding = Wait-WiiLandRootDriverBinding $disabledRoot
                if ([string]$binding.InfName -ine $ExpectedDriverInf) {
                    throw "Rollback bound '$($binding.InfName)' instead of the exact owned previous package '$ExpectedDriverInf'; the root remains disabled."
                }
            }
            $activationSucceeded = $true
        }
        return ,@($output | ForEach-Object { [string]$_ })
    } finally {
        if ($null -ne $disabledRoot -and $activationSucceeded) {
            Assert-WiiLandRootV3EnableReady $disabledRoot -AllowV2Rollback:$allowV2Rollback | Out-Null
            $enableOutput = & $pnputil '/enable-device' $disabledRoot 2>&1
            if ($LASTEXITCODE -ne 0) { throw "Could not re-enable WiiLand root devnode '$disabledRoot': $($enableOutput -join ' ')" }
            Wait-WiiLandRootDeviceState $disabledRoot $true
        }
    }
}

function Get-WiiLandService {
    Get-CimInstance -ClassName Win32_Service -Filter "Name='$script:ServiceName'" -ErrorAction Stop
}

function Assert-WiiLandServiceOwner([string]$InstallDirectory, [object]$Service) {
    if ($null -eq $Service) { return }
    $expectedPath = '"' + (Join-Path $InstallDirectory 'wiiland-output-service.exe') + '"'
    if ([string]$Service.PathName -cne $expectedPath -or [string]$Service.StartName -ine 'LocalSystem' -or [string]$Service.ServiceType -notmatch 'Own Process') {
        throw "The $($script:ServiceName) service is not owned by '$expectedPath'; refusing to change it."
    }
}

function Invoke-WiiLandSc([string[]]$Arguments) {
    if ($Arguments.Count -ge 2 -and $Arguments[0] -ieq 'start' -and $Arguments[1] -ieq $script:ServiceName) {
        Assert-WiiLandHidStack
    }
    $sc = Join-Path $env:SystemRoot 'System32\sc.exe'
    $output = & $sc @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) { throw "sc.exe $($Arguments -join ' ') failed ($LASTEXITCODE): $($output -join ' ')" }
    return ,@($output | ForEach-Object { [string]$_ })
}

function Assert-WiiLandServiceSid {
    $key = "HKLM:\SYSTEM\CurrentControlSet\Services\$script:ServiceName"
    if (-not (Test-Path -LiteralPath $key) -or [int](Get-ItemProperty -LiteralPath $key -Name ServiceSidType -ErrorAction SilentlyContinue).ServiceSidType -ne 1) {
        throw "$($script:ServiceName) does not have SERVICE_SID_TYPE_UNRESTRICTED (ServiceSidType=1)."
    }
}

function Get-WiiLandUserRunKey([string]$UserSid, [switch]$Create) {
    if ([string]$UserSid -notmatch '^S-1-(?:\d+-)+\d+$') { throw 'UserSid must be a Windows account SID.' }
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::Users, [Microsoft.Win32.RegistryView]::Registry64)
    try {
        $userRoot = $base.OpenSubKey($UserSid, $false)
        if ($null -eq $userRoot) { throw "The selected user's registry hive is not loaded: $UserSid" }
        $userRoot.Dispose()
        $path = "$UserSid\Software\Microsoft\Windows\CurrentVersion\Run"
        if ($Create) { return $base.CreateSubKey($path) }
        return $base.OpenSubKey($path, $true)
    } finally { $base.Dispose() }
}

function Get-WiiLandRunValue([string]$UserSid) {
    $key = Get-WiiLandUserRunKey $UserSid
    if ($null -eq $key) { return $null }
    try {
        if ($key.GetValueNames() -notcontains $script:RunValueName) { return $null }
        [pscustomobject]@{
            Value = $key.GetValue($script:RunValueName, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            Kind = $key.GetValueKind($script:RunValueName)
        }
    } finally { $key.Dispose() }
}

function Set-WiiLandRunValue([string]$UserSid, [string]$Value) {
    $key = Get-WiiLandUserRunKey $UserSid -Create
    try { $key.SetValue($script:RunValueName, $Value, [Microsoft.Win32.RegistryValueKind]::String) }
    finally { $key.Dispose() }
}

function Remove-WiiLandRunValue([string]$UserSid) {
    $key = Get-WiiLandUserRunKey $UserSid
    if ($null -eq $key) { return }
    try { $key.DeleteValue($script:RunValueName, $false) }
    finally { $key.Dispose() }
}


Export-ModuleMember -Function Get-WiiLandConstants, Assert-WiiLandAdministrator, Get-WiiLandPlatform, Assert-WiiLandProductionBoot, Get-WiiLandRelease, Get-WiiLandCanonicalPath, Assert-WiiLandInstallDirectory, Assert-WiiLandOwnedContents, Read-WiiLandState, Write-WiiLandState, Get-WiiLandRootDevices, Get-WiiLandRootDriverBinding, Get-WiiLandHidChildren, New-WiiLandRootDevice, Remove-WiiLandRootDevice, Invoke-WiiLandPnpUtil, Get-WiiLandService, Assert-WiiLandServiceOwner, Assert-WiiLandServiceConfiguration, Invoke-WiiLandSc, Assert-WiiLandServiceSid, Get-WiiLandRunValue, Set-WiiLandRunValue, Remove-WiiLandRunValue, Test-WiiLandRollbackStateContract, Test-WiiLandRollbackPackageFiles
