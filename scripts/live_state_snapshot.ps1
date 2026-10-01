<#
.SYNOPSIS
  Read-only snapshot of the live settings Cairn can change, written as JSON.
.DESCRIPTION
  Records the DNS servers of every interface, the state of the scheduled tasks in the debloat
  catalog, the System Restore point sequence numbers and creation-frequency override, the
  self-test scheduled task sandbox and the \Cairn\ task folder, the Windows Update UX and policy
  values, the mouse, GPU scheduling and classic context menu values, the Office and Edge policy
  values, the camera, microphone and location consent stores (every store's and subkey's Value and
  LastSetTime), the location sensor's override (SensorPermissionState), CairnSpeedTest-* folders at
  fixed volume roots, the registry sandbox key, and the timestamps and sizes of the files in the
  real data folder. Two snapshots taken before and after a test run must be identical. Nothing is
  changed: the script only calls Get-* cmdlets, lists folders and reads the registry. The
  consent-store entries hold app names and paths, so the output stays outside the repository.
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\live_state_snapshot.ps1 -Out "$env:LOCALAPPDATA\PCOptimizerDev\baseline\before.json"
#>
param(
    [Parameter(Mandatory = $true)] [string]$Out
)
$ErrorActionPreference = 'Stop'

$catalogTasks = @(
    '\Microsoft\Windows\Customer Experience Improvement Program\Consolidator',
    '\Microsoft\Windows\Customer Experience Improvement Program\UsbCeip',
    '\Microsoft\Windows\Customer Experience Improvement Program\KernelCeipTask',
    '\Microsoft\Windows\Autochk\Proxy',
    '\Microsoft\Windows\DiskDiagnostic\Microsoft-Windows-DiskDiagnosticDataCollector',
    '\Microsoft\Windows\Feedback\Siuf\DmClient',
    '\Microsoft\Windows\Feedback\Siuf\DmClientOnScenarioDownload',
    '\Microsoft\Windows\Windows Error Reporting\QueueReporting',
    '\Microsoft\Windows\Application Experience\Microsoft Compatibility Appraiser',
    '\Microsoft\Windows\Application Experience\Microsoft Compatibility Appraiser Exp',
    '\Microsoft\Windows\Application Experience\ProgramDataUpdater',
    '\Microsoft\Windows\Application Experience\MareBackup'
)

# Registry reads go through read-only .NET handles on the 64-bit view, so key names that contain
# wildcard characters (consent-store entries of desktop apps) are taken literally.
function Open-RegKey([string]$Hive, [string]$Path) {
    $root = if ($Hive -eq 'HKLM') { [Microsoft.Win32.RegistryHive]::LocalMachine } else { [Microsoft.Win32.RegistryHive]::CurrentUser }
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey($root, [Microsoft.Win32.RegistryView]::Registry64)
    try { return $base.OpenSubKey($Path, $false) } finally { $base.Close() }
}

function Get-RegData($Key, [string]$Name) {
    if (-not ($Key.GetValueNames() -contains $Name)) { return $null }
    $kind = $Key.GetValueKind($Name)
    $data = $Key.GetValue($Name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    if ($kind -eq [Microsoft.Win32.RegistryValueKind]::Binary -and $null -ne $data) {
        $data = [System.BitConverter]::ToString([byte[]]$data)
    }
    return [ordered]@{ Kind = [string]$kind; Data = $data }
}

# The named values of one key; a missing value is recorded as null.
function Get-RegNamedValues([string]$Hive, [string]$Path, [string[]]$Names) {
    $values = [ordered]@{}
    $key = Open-RegKey $Hive $Path
    if ($null -eq $key) {
        foreach ($name in $Names) { $values[$name] = $null }
        return [ordered]@{ Hive = $Hive; Path = $Path; Exists = $false; Values = $values }
    }
    try {
        foreach ($name in $Names) { $values[$name] = Get-RegData $key $name }
    } finally { $key.Close() }
    return [ordered]@{ Hive = $Hive; Path = $Path; Exists = $true; Values = $values }
}

# Every value of one key, sorted by name.
function Get-RegAllValues([string]$Hive, [string]$Path) {
    $values = [ordered]@{}
    $key = Open-RegKey $Hive $Path
    if ($null -eq $key) { return [ordered]@{ Hive = $Hive; Path = $Path; Exists = $false; Values = $values } }
    try {
        foreach ($name in ($key.GetValueNames() | Sort-Object)) { $values[$name] = Get-RegData $key $name }
    } finally { $key.Close() }
    return [ordered]@{ Hive = $Hive; Path = $Path; Exists = $true; Values = $values }
}

function Test-RegKey([string]$Hive, [string]$Path) {
    $key = Open-RegKey $Hive $Path
    if ($null -eq $key) { return $false }
    $key.Close()
    return $true
}

# Value and LastSetTime of every subkey below $Key, depth first, subkeys sorted by name.
function Get-ConsentEntries($Key, [string]$Prefix) {
    foreach ($name in ($Key.GetSubKeyNames() | Sort-Object)) {
        $sub = $Key.OpenSubKey($name, $false)
        if ($null -eq $sub) { continue }
        try {
            $relative = if ($Prefix) { "$Prefix\$name" } else { $name }
            [ordered]@{
                Path        = $relative
                Value       = Get-RegData $sub 'Value'
                LastSetTime = Get-RegData $sub 'LastSetTime'
            }
            Get-ConsentEntries $sub $relative
        } finally { $sub.Close() }
    }
}

# Value and LastSetTime of the store key itself (the switch for the whole PC under HKLM, the
# Store-apps switch under HKCU), then of every subkey.
function Get-ConsentStore([string]$Hive, [string]$Capability) {
    $path = "SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\$Capability"
    $key = Open-RegKey $Hive $path
    if ($null -eq $key) {
        return [ordered]@{ Capability = $Capability; Hive = $Hive; Exists = $false; Value = $null; LastSetTime = $null; Subkeys = @() }
    }
    try {
        return [ordered]@{
            Capability  = $Capability
            Hive        = $Hive
            Exists      = $true
            Value       = Get-RegData $key 'Value'
            LastSetTime = Get-RegData $key 'LastSetTime'
            Subkeys     = @(Get-ConsentEntries $key '')
        }
    } finally { $key.Close() }
}

function Get-FileStamp([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return [ordered]@{ Exists = $false; LastWriteTimeUtc = $null; Length = $null }
    }
    $item = Get-Item -LiteralPath $Path -Force
    return [ordered]@{ Exists = $true; LastWriteTimeUtc = $item.LastWriteTimeUtc.ToString('o'); Length = $item.Length }
}

function Get-FolderStamp([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        return [ordered]@{ Exists = $false; FileCount = $null; NewestLastWriteTimeUtc = $null }
    }
    $files = @(Get-ChildItem -LiteralPath $Path -Recurse -File -Force -ErrorAction SilentlyContinue)
    $newest = $files | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
    $stamp = if ($newest) { $newest.LastWriteTimeUtc.ToString('o') } else { $null }
    return [ordered]@{ Exists = $true; FileCount = $files.Count; NewestLastWriteTimeUtc = $stamp }
}

$dns = @(Get-DnsClientServerAddress | Sort-Object InterfaceIndex, AddressFamily | ForEach-Object {
    [ordered]@{
        InterfaceAlias  = $_.InterfaceAlias
        InterfaceIndex  = $_.InterfaceIndex
        AddressFamily   = [int]$_.AddressFamily
        ServerAddresses = @($_.ServerAddresses)
    }
})

$tasks = @(foreach ($path in $catalogTasks) {
    $split = $path.LastIndexOf('\')
    $folder = $path.Substring(0, $split + 1)
    $name = $path.Substring($split + 1)
    $task = Get-ScheduledTask -TaskPath $folder -TaskName $name -ErrorAction SilentlyContinue
    if ($task) {
        [ordered]@{ Path = $path; Exists = $true; State = [string]$task.State; Enabled = [bool]$task.Settings.Enabled }
    } else {
        [ordered]@{ Path = $path; Exists = $false; State = $null; Enabled = $null }
    }
})

$restorePoints = @(Get-ComputerRestorePoint -ErrorAction SilentlyContinue |
    Sort-Object SequenceNumber | ForEach-Object { [int]$_.SequenceNumber })

$frequency = $null
$srKey = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\SystemRestore'
$srValue = Get-ItemProperty -Path $srKey -Name 'SystemRestorePointCreationFrequency' -ErrorAction SilentlyContinue
if ($srValue) { $frequency = $srValue.SystemRestorePointCreationFrequency }

$sandboxTasks = @(Get-ScheduledTask -TaskPath '\PCOptimizerSelfTest\' -ErrorAction SilentlyContinue |
    ForEach-Object { $_.TaskPath + $_.TaskName })
$sandboxFolder = Test-Path (Join-Path $env:SystemRoot 'System32\Tasks\PCOptimizerSelfTest')

$cairnTasks = @(Get-ScheduledTask -TaskPath '\Cairn\' -ErrorAction SilentlyContinue |
    ForEach-Object { $_.TaskPath + $_.TaskName })
$cairnFolder = Test-Path (Join-Path $env:SystemRoot 'System32\Tasks\Cairn')

$wuUx = Get-RegNamedValues 'HKLM' 'SOFTWARE\Microsoft\WindowsUpdate\UX\Settings' @(
    'PauseFeatureUpdatesStartTime', 'PauseFeatureUpdatesEndTime',
    'PauseQualityUpdatesStartTime', 'PauseQualityUpdatesEndTime',
    'PauseUpdatesStartTime', 'PauseUpdatesExpiryTime',
    'ActiveHoursStart', 'ActiveHoursEnd', 'SmartActiveHoursState', 'RestartNotificationsAllowed2'
)
$wuPolicy = Get-RegNamedValues 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate' @(
    'ExcludeWUDriversInQualityUpdate', 'DeferFeatureUpdates', 'DeferFeatureUpdatesPeriodInDays'
)

$mouse = Get-RegNamedValues 'HKCU' 'Control Panel\Mouse' @('MouseSpeed', 'MouseThreshold1', 'MouseThreshold2')
$graphics = Get-RegNamedValues 'HKLM' 'SYSTEM\CurrentControlSet\Control\GraphicsDrivers' @('HwSchMode')
$classicMenuPath = 'Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}'
$classicMenu = [ordered]@{
    Exists               = Test-RegKey 'HKCU' $classicMenuPath
    InprocServer32Exists = Test-RegKey 'HKCU' "$classicMenuPath\InprocServer32"
}

$officePolicies = @(
    (Get-RegAllValues 'HKCU' 'Software\Policies\Microsoft\office\common\clienttelemetry'),
    (Get-RegAllValues 'HKCU' 'Software\Policies\Microsoft\office\16.0\common\privacy'),
    (Get-RegAllValues 'HKCU' 'Software\Policies\Microsoft\office\16.0\common\feedback')
)
$edgePolicy = Get-RegAllValues 'HKLM' 'SOFTWARE\Policies\Microsoft\Edge'

$consentStores = @(foreach ($hive in @('HKLM', 'HKCU')) {
    foreach ($capability in @('webcam', 'microphone', 'location')) {
        Get-ConsentStore $hive $capability
    }
})
# The location sensor's override, written together with the location switch for the whole PC.
$locationSensor = Get-RegNamedValues 'HKLM' 'SOFTWARE\Microsoft\Windows NT\CurrentVersion\Sensor\Overrides\{BFA794E4-F964-4FDB-90F6-51056BFE4B44}' @('SensorPermissionState')

$speedTestFolders = @(foreach ($drive in [System.IO.DriveInfo]::GetDrives()) {
    if ($drive.DriveType -ne [System.IO.DriveType]::Fixed -or -not $drive.IsReady) { continue }
    $root = $drive.RootDirectory.FullName
    Get-ChildItem -LiteralPath $root -Directory -Force -Filter 'CairnSpeedTest-*' -ErrorAction SilentlyContinue |
        ForEach-Object { $_.FullName }
})

$selfTestKey = Open-RegKey 'HKCU' 'Software\PCOptimizer\SelfTest'
if ($null -eq $selfTestKey) {
    $selfTest = [ordered]@{ Exists = $false; Subkeys = @(); Values = @() }
} else {
    try {
        $selfTest = [ordered]@{
            Exists  = $true
            Subkeys = @($selfTestKey.GetSubKeyNames() | Sort-Object)
            Values  = @($selfTestKey.GetValueNames() | Sort-Object)
        }
    } finally { $selfTestKey.Close() }
}

$dataDir = Join-Path $env:LOCALAPPDATA 'PCOptimizer'
$dataFolder = [ordered]@{
    Exists  = Test-Path -LiteralPath $dataDir -PathType Container
    Files   = [ordered]@{
        'journal.db'                 = Get-FileStamp (Join-Path $dataDir 'journal.db')
        'journal.db-wal'             = Get-FileStamp (Join-Path $dataDir 'journal.db-wal')
        'app_list.json'              = Get-FileStamp (Join-Path $dataDir 'app_list.json')
        'storage\speed_history.json' = Get-FileStamp (Join-Path $dataDir 'storage\speed_history.json')
    }
    Folders = [ordered]@{
        'jobs'        = Get-FolderStamp (Join-Path $dataDir 'jobs')
        'logs'        = Get-FolderStamp (Join-Path $dataDir 'logs')
        'maintenance' = Get-FolderStamp (Join-Path $dataDir 'maintenance')
        'tools'       = Get-FolderStamp (Join-Path $dataDir 'tools')
    }
}

$snapshot = [ordered]@{
    TakenAt                             = (Get-Date).ToString('o')
    Dns                                 = $dns
    ScheduledTasks                      = $tasks
    RestorePointSequenceNumbers         = $restorePoints
    SystemRestorePointCreationFrequency = $frequency
    SelfTestTasks                       = $sandboxTasks
    SelfTestTasksExist                  = ($sandboxTasks.Count -gt 0)
    SelfTestTasksFolderExists           = $sandboxFolder
    CairnTasks                          = $cairnTasks
    CairnTasksExist                     = ($cairnTasks.Count -gt 0)
    CairnTasksFolderExists              = $cairnFolder
    WindowsUpdateUxSettings             = $wuUx
    WindowsUpdatePolicy                 = $wuPolicy
    Mouse                               = $mouse
    GraphicsDrivers                     = $graphics
    ClassicContextMenu                  = $classicMenu
    OfficePolicies                      = $officePolicies
    EdgePolicy                          = $edgePolicy
    ConsentStores                       = $consentStores
    LocationSensorOverride              = $locationSensor
    SpeedTestFolders                    = $speedTestFolders
    SelfTestRegistry                    = $selfTest
    DataFolder                          = $dataFolder
}

$dir = Split-Path -Parent $Out
if ($dir) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
$json = ConvertTo-Json -InputObject $snapshot -Depth 10
[System.IO.File]::WriteAllText($Out, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Host "Live state written to $Out"
