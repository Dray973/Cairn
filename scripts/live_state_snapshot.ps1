<#
.SYNOPSIS
  Read-only snapshot of the live settings Cairn can change, written as JSON.
.DESCRIPTION
  Records the DNS servers of every interface, the state of the scheduled tasks in the debloat
  catalog, the System Restore point sequence numbers ("not read" when the shell is not elevated,
  because Get-ComputerRestorePoint needs administrator rights) and creation-frequency override, the
  self-test scheduled task sandbox and the \Cairn\ task folder, the Windows Update UX and policy
  values, the mouse, GPU scheduling and classic context menu values, the Office and Edge policy
  keys (machine and user), the camera, microphone and location consent stores (every store's and
  subkey's Value and LastSetTime), the location sensor's override (SensorPermissionState),
  CairnSpeedTest-* folders at fixed volume roots, the registry sandbox key, and the timestamps and
  sizes of the files in the real data folder.

  It also records the keys that the privacy, search, AI, sync, security and File Explorer tweaks
  write: policy keys and small settings keys whole (every value, the subkey names and the subkey
  and value counts), and only the tweaks' own values inside large keys that Windows and other
  programs also write (Explorer\Advanced, ContentDeliveryManager, Notifications\Settings and
  others). It records the Recall component's servicing key, the documented Secure Boot servicing
  values and, when the shell is elevated, the enabled flag and last run time of the
  \Microsoft\Windows\PI\Secure-Boot-Update task. The task's access list admits only
  administrators, so an unelevated run records "not read" for it.

  When scripts\verification\targets.json exists, every target it lists is recorded as well. Every
  list is optional; hives are HKLM, HKCU, HKCR or HKU:

    {
      "registry_values": [{ "hive": "HKCU", "path": "Software\\Example", "name": "Value" }],
      "registry_keys": [{ "hive": "HKLM", "path": "SOFTWARE\\Example" }],
      "services": ["ServiceName"],
      "tasks": ["\\Folder\\TaskName"]
    }

  A value is recorded with its type and data (null when absent), a key with its existence and its
  subkey and value counts, a service with its Start and DelayedAutostart values, and a task with
  its enabled flag.

  Two snapshots taken before and after a test run must be identical, apart from what Windows
  changes on its own schedule (task states, the Secure Boot task's last run time, restore points).
  Nothing is changed: the script only calls Get-* cmdlets, lists folders and reads the registry and
  the targets file. The consent-store entries hold app names and paths, so the output stays outside
  the repository.
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\live_state_snapshot.ps1 -Out "$env:LOCALAPPDATA\PCOptimizerDev\baseline\before.json"
#>
param(
    [Parameter(Mandatory = $true)] [string]$Out
)
$ErrorActionPreference = 'Stop'

# Reading the System Restore points and the Secure Boot update task both need administrator
# rights; an unelevated run records "not read" for them instead of an empty or misleading value.
$isElevated = ([System.Security.Principal.WindowsPrincipal]`
    [System.Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [System.Security.Principal.WindowsBuiltInRole]::Administrator)

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
    $root = switch ($Hive) {
        'HKLM' { [Microsoft.Win32.RegistryHive]::LocalMachine }
        'HKCU' { [Microsoft.Win32.RegistryHive]::CurrentUser }
        'HKCR' { [Microsoft.Win32.RegistryHive]::ClassesRoot }
        'HKU' { [Microsoft.Win32.RegistryHive]::Users }
        default { throw "Unknown registry hive '$Hive'." }
    }
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

# A whole key: every value sorted by name, the subkey names, and the subkey and value counts.
function Get-RegKeyState([string]$Hive, [string]$Path) {
    $values = [ordered]@{}
    $key = Open-RegKey $Hive $Path
    if ($null -eq $key) {
        return [ordered]@{
            Hive = $Hive; Path = $Path; Exists = $false; SubkeyCount = $null; ValueCount = $null; Subkeys = @(); Values = $values
        }
    }
    try {
        foreach ($name in ($key.GetValueNames() | Sort-Object)) { $values[$name] = Get-RegData $key $name }
        return [ordered]@{
            Hive        = $Hive
            Path        = $Path
            Exists      = $true
            SubkeyCount = $key.SubKeyCount
            ValueCount  = $key.ValueCount
            Subkeys     = @($key.GetSubKeyNames() | Sort-Object)
            Values      = $values
        }
    } finally { $key.Close() }
}

# Existence and the subkey and value counts of one key.
function Get-RegKeyCounts([string]$Hive, [string]$Path) {
    $key = Open-RegKey $Hive $Path
    if ($null -eq $key) { return [ordered]@{ Hive = $Hive; Path = $Path; Exists = $false; SubkeyCount = $null; ValueCount = $null } }
    try {
        return [ordered]@{ Hive = $Hive; Path = $Path; Exists = $true; SubkeyCount = $key.SubKeyCount; ValueCount = $key.ValueCount }
    } finally { $key.Close() }
}

# Start type of one service from its key (Start: 2 automatic, 3 manual, 4 disabled), with the
# delayed-start flag.
function Get-ServiceStart([string]$Name) {
    $state = Get-RegNamedValues 'HKLM' "SYSTEM\CurrentControlSet\Services\$Name" @('Start', 'DelayedAutostart')
    return [ordered]@{
        Name             = $Name
        Exists           = $state.Exists
        Start            = $state['Values']['Start']
        DelayedAutostart = $state['Values']['DelayedAutostart']
    }
}

# The registered task at a full task path such as \Microsoft\Windows\Autochk\Proxy, or $null.
function Find-Task([string]$Path) {
    $split = $Path.LastIndexOf('\')
    $folder = $Path.Substring(0, $split + 1)
    $name = $Path.Substring($split + 1)
    return Get-ScheduledTask -TaskPath $folder -TaskName $name -ErrorAction SilentlyContinue
}

# One list of the targets file; an absent list yields nothing.
function Get-TargetList($Targets, [string]$Name) {
    $property = $Targets.PSObject.Properties[$Name]
    if ($null -eq $property -or $null -eq $property.Value) { return }
    foreach ($entry in @($property.Value)) { $entry }
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
    $task = Find-Task $path
    if ($task) {
        [ordered]@{ Path = $path; Exists = $true; State = [string]$task.State; Enabled = [bool]$task.Settings.Enabled }
    } else {
        [ordered]@{ Path = $path; Exists = $false; State = $null; Enabled = $null }
    }
})

# Get-ComputerRestorePoint needs administrator rights; unelevated it returns nothing, which
# would read as "no restore points". Record "not read" instead so the two snapshots of a run
# are only compared when both could read them.
if ($isElevated) {
    $restorePoints = @(Get-ComputerRestorePoint -ErrorAction SilentlyContinue |
        Sort-Object SequenceNumber | ForEach-Object { [int]$_.SequenceNumber })
} else {
    $restorePoints = 'not read'
}

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
    'ActiveHoursStart', 'ActiveHoursEnd', 'SmartActiveHoursState', 'RestartNotificationsAllowed2',
    'IsContinuousInnovationOptedIn'
)
$wuPolicy = Get-RegNamedValues 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate' @(
    'ExcludeWUDriversInQualityUpdate', 'DeferFeatureUpdates', 'DeferFeatureUpdatesPeriodInDays',
    'SetAllowOptionalContent', 'AllowOptionalContent'
)

$mouse = Get-RegNamedValues 'HKCU' 'Control Panel\Mouse' @('MouseSpeed', 'MouseThreshold1', 'MouseThreshold2')
$graphics = Get-RegNamedValues 'HKLM' 'SYSTEM\CurrentControlSet\Control\GraphicsDrivers' @('HwSchMode')
$classicMenuPath = 'Software\Classes\CLSID\{86ca1aa0-34aa-4e8b-a509-50c905bae2a2}'
$classicMenu = [ordered]@{
    Exists               = Test-RegKey 'HKCU' $classicMenuPath
    InprocServer32Exists = Test-RegKey 'HKCU' "$classicMenuPath\InprocServer32"
}

$officePolicies = @(
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\office\common\clienttelemetry'),
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\office\16.0\common'),
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\office\16.0\common\privacy'),
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\office\16.0\common\feedback'),
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\office\16.0\outlook\options\general'),
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\office\16.0\outlook\preferences')
)
$edgePolicies = @(
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Edge'),
    (Get-RegKeyState 'HKCU' 'SOFTWARE\Policies\Microsoft\Edge')
)

# Policy keys and small settings keys of the privacy, search, AI, sync and security tweaks,
# recorded whole so that any value or subkey a run leaves behind shows up.
$tweakKeys = @(
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\Windows Search'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\System'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\AppPrivacy'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\LocationAndSensors'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\WindowsAI'),
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\Windows\WindowsAI'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\WindowsCopilot'),
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\Windows\WindowsCopilot'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\Explorer'),
    (Get-RegKeyState 'HKCU' 'Software\Policies\Microsoft\Windows\Explorer'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\SettingSync'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\CredUI'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\Device Metadata'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\DataCollection'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\AppCompat'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\Connect'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows\DriverSearching'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\FindMyDevice'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\Microsoft\MRT'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Policies\WindowsNotepad'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\Paint'),
    (Get-RegKeyState 'HKLM' 'SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\TextInput'),
    (Get-RegKeyState 'HKLM' 'SYSTEM\CurrentControlSet\Control\Remote Assistance'),
    (Get-RegKeyState 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\SearchSettings'),
    (Get-RegKeyState 'HKCU' 'Software\Microsoft\Input\TIPC'),
    (Get-RegKeyState 'HKCU' 'Control Panel\International\User Profile'),
    (Get-RegKeyState 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\Mobility')
)

# The tweaks' own values inside keys that Windows and other programs also write. A key that a
# tweak creates (UserProfileEngagement, the two CLSID keys) shows up through Exists.
$tweakValues = @(
    (Get-RegNamedValues 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced' @(
        'Start_IrisRecommendations', 'Start_AccountNotifications', 'Start_TrackDocs', 'ShowSyncProviderNotifications'
    )),
    (Get-RegNamedValues 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\SystemSettings\AccountNotifications' @(
        'EnableAccountNotifications'
    )),
    (Get-RegNamedValues 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\ContentDeliveryManager' @(
        'SubscribedContent-310093Enabled', 'SoftLandingEnabled', 'RotatingLockScreenEnabled'
    )),
    (Get-RegNamedValues 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\UserProfileEngagement' @(
        'ScoobeSystemSettingEnabled'
    )),
    (Get-RegNamedValues 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\Notifications\Settings' @(
        'NOC_GLOBAL_SETTING_ALLOW_TOASTS_ABOVE_LOCK'
    )),
    (Get-RegNamedValues 'HKCU' 'Software\Classes\CLSID\{f874310e-b6b7-47dc-bc84-b9e6b38f5903}' @(
        'System.IsPinnedToNameSpaceTree'
    )),
    (Get-RegNamedValues 'HKCU' 'Software\Classes\CLSID\{e88865ea-0e1c-4e20-9aa6-edcd0212c87c}' @(
        'System.IsPinnedToNameSpaceTree'
    )),
    (Get-RegNamedValues 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\Explorer' @('ShowCloudFilesInQuickAccess')),
    (Get-RegNamedValues 'HKCU' 'Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel' @(
        '{2cc5ca98-6485-489a-920e-b3e88a6ccce3}'
    )),
    (Get-RegNamedValues 'HKCU' 'Software\Microsoft\MediaPlayer\Preferences' @('UsageTracking')),
    (Get-RegNamedValues 'HKLM' 'SYSTEM\CurrentControlSet\Control\FileSystem' @('LongPathsEnabled')),
    (Get-RegNamedValues 'HKLM' 'SYSTEM\CurrentControlSet\Control\Session Manager' @('DisableWpbtExecution')),
    (Get-RegNamedValues 'HKLM' 'SOFTWARE\Microsoft\Windows\CurrentVersion\Device Installer' @('DisableCoInstallers'))
)

$recallComponent = Get-RegKeyState 'HKLM' 'SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\Notifications\OptionalFeatures\Recall'

$secureBootKey = 'SYSTEM\CurrentControlSet\Control\SecureBoot'
$secureBootValues = @(
    (Get-RegNamedValues 'HKLM' "$secureBootKey\Servicing" @('UEFICA2023Status', 'UEFICA2023Error')),
    (Get-RegNamedValues 'HKLM' $secureBootKey @('AvailableUpdates', 'HighConfidenceOptOut', 'AvailableUpdatesPolicy')),
    (Get-RegNamedValues 'HKLM' "$secureBootKey\State" @('UEFISecureBootEnabled'))
)
$secureBootTaskPath = '\Microsoft\Windows\PI\Secure-Boot-Update'
if (-not $isElevated) {
    $secureBootTask = 'not read'
} else {
    $sbTask = Find-Task $secureBootTaskPath
    if ($sbTask) {
        $sbInfo = $sbTask | Get-ScheduledTaskInfo -ErrorAction SilentlyContinue
        $lastRun = $null
        if ($sbInfo -and $sbInfo.LastRunTime) { $lastRun = $sbInfo.LastRunTime.ToUniversalTime().ToString('o') }
        $secureBootTask = [ordered]@{
            Path = $secureBootTaskPath; Exists = $true; Enabled = [bool]$sbTask.Settings.Enabled; LastRunTime = $lastRun
        }
    } else {
        $secureBootTask = [ordered]@{ Path = $secureBootTaskPath; Exists = $false; Enabled = $null; LastRunTime = $null }
    }
}

$targetsFile = Join-Path $PSScriptRoot 'verification\targets.json'
if (Test-Path -LiteralPath $targetsFile -PathType Leaf) {
    $targets = [System.IO.File]::ReadAllText($targetsFile) | ConvertFrom-Json
    $targetValues = @(foreach ($entry in (Get-TargetList $targets 'registry_values')) {
        $state = Get-RegNamedValues $entry.hive $entry.path @($entry.name)
        [ordered]@{
            Hive = $entry.hive; Path = $entry.path; Name = $entry.name; KeyExists = $state.Exists; Value = $state['Values'][$entry.name]
        }
    })
    $targetKeys = @(foreach ($entry in (Get-TargetList $targets 'registry_keys')) { Get-RegKeyCounts $entry.hive $entry.path })
    $targetServices = @(foreach ($name in (Get-TargetList $targets 'services')) { Get-ServiceStart $name })
    $targetTasks = @(foreach ($path in (Get-TargetList $targets 'tasks')) {
        $found = Find-Task $path
        $enabled = $null
        if ($found) { $enabled = [bool]$found.Settings.Enabled }
        [ordered]@{ Path = $path; Exists = [bool]$found; Enabled = $enabled }
    })
    $verificationTargets = [ordered]@{
        FileExists = $true; RegistryValues = $targetValues; RegistryKeys = $targetKeys; Services = $targetServices; Tasks = $targetTasks
    }
} else {
    $verificationTargets = [ordered]@{
        FileExists = $false; RegistryValues = @(); RegistryKeys = @(); Services = @(); Tasks = @()
    }
}

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
    EdgePolicies                        = $edgePolicies
    TweakKeys                           = $tweakKeys
    TweakValues                         = $tweakValues
    RecallComponent                     = $recallComponent
    SecureBootValues                    = $secureBootValues
    SecureBootUpdateTask                = $secureBootTask
    VerificationTargets                 = $verificationTargets
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
