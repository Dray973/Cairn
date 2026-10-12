<#
.SYNOPSIS
  Lists how many seconds after File Explorer each startup program of this session started.
.DESCRIPTION
  Reads the programs of the Run keys (the signed-in user's and the machine's, in the 64-bit and
  the 32-bit view) and of the two Startup folders, finds their processes in this session, and
  prints how many seconds after this session's explorer.exe each one started, with the median.
  Run it soon after signing in, after a restart with the startup delay and after one without
  it: without the delay the programs start sooner after explorer.exe.

  It only reads. The output names the programs that start on this PC, so -Out stays outside the
  repository; it records each program's file name, not its folder.
.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verification\startup_timing.ps1 -Out "$env:LOCALAPPDATA\PCOptimizerDev\scratch\startup-timing-before.json"
#>
param(
    [string]$Out
)
$ErrorActionPreference = 'Stop'

$session = (Get-Process -Id $PID).SessionId
$explorer = Get-Process -Name explorer -ErrorAction SilentlyContinue |
    Where-Object { $_.SessionId -eq $session } | Sort-Object StartTime | Select-Object -First 1
if ($null -eq $explorer) {
    throw 'explorer.exe is not running in this session.'
}
$explorerStart = $explorer.StartTime
$boot = (Get-CimInstance -ClassName Win32_OperatingSystem).LastBootUpTime
# The Shell Infrastructure Host starts at sign-in and keeps running when File Explorer restarts.
$shellHost = Get-Process -Name sihost -ErrorAction SilentlyContinue |
    Where-Object { $_.SessionId -eq $session } | Sort-Object StartTime | Select-Object -First 1
if ($shellHost -and ($explorerStart - $shellHost.StartTime).TotalSeconds -gt 60) {
    Write-Warning ('File Explorer restarted after sign-in, so the times below count from that ' +
        'restart. Restart Windows and run this right after signing in.')
}

# The program a Run value starts: the quoted path, or the text up to '.exe'.
function Get-CommandProgram([string]$Command) {
    $text = [Environment]::ExpandEnvironmentVariables($Command.Trim())
    if ($text.StartsWith('"')) {
        $end = $text.IndexOf('"', 1)
        if ($end -gt 1) { return $text.Substring(1, $end - 1) }
    }
    $exe = $text.IndexOf('.exe', [StringComparison]::OrdinalIgnoreCase)
    if ($exe -gt 0) { return $text.Substring(0, $exe + 4) }
    return ($text -split ' ')[0]
}

$entries = New-Object System.Collections.Generic.List[object]
$runKeys = [ordered]@{
    'Run (user)'             = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    'Run (machine)'          = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run'
    'Run (machine, 32-bit)'  = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run'
}
foreach ($source in $runKeys.Keys) {
    $key = Get-Item -LiteralPath $runKeys[$source] -ErrorAction SilentlyContinue
    if ($null -eq $key) { continue }
    foreach ($name in $key.GetValueNames()) {
        if ($name -eq '') { continue }
        $command = [string]$key.GetValue($name, '', 'DoNotExpandEnvironmentNames')
        $entries.Add([pscustomobject]@{ Source = $source; Name = $name; Program = (Get-CommandProgram $command) })
    }
}
$shell = New-Object -ComObject WScript.Shell
$folders = [ordered]@{
    'Startup folder (user)'    = [Environment]::GetFolderPath('Startup')
    'Startup folder (machine)' = [Environment]::GetFolderPath('CommonStartup')
}
foreach ($source in $folders.Keys) {
    $folder = $folders[$source]
    if (-not $folder -or -not (Test-Path -LiteralPath $folder)) { continue }
    foreach ($file in Get-ChildItem -LiteralPath $folder -File) {
        $program = $file.FullName
        if ($file.Extension -eq '.lnk') {
            # CreateShortcut only reads an existing shortcut until Save is called.
            $program = $shell.CreateShortcut($file.FullName).TargetPath
        }
        $entries.Add([pscustomobject]@{ Source = $source; Name = $file.BaseName; Program = $program })
    }
}

# The earliest start time of each program path running in this session.
$started = @{}
foreach ($process in Get-Process | Where-Object { $_.SessionId -eq $session }) {
    try {
        $path = $process.Path
        $time = $process.StartTime
    } catch {
        continue
    }
    if (-not $path) { continue }
    $path = $path.ToLowerInvariant()
    if (-not $started.ContainsKey($path) -or $time -lt $started[$path]) { $started[$path] = $time }
}

$rows = foreach ($entry in $entries) {
    $program = if ($entry.Program) { $entry.Program.ToLowerInvariant() } else { '' }
    $seconds = $null
    if ($program -and $started.ContainsKey($program)) {
        $seconds = [math]::Round(($started[$program] - $explorerStart).TotalSeconds, 1)
    }
    [pscustomobject]@{
        Source             = $entry.Source
        Name               = $entry.Name
        File               = if ($entry.Program) { Split-Path -Leaf $entry.Program } else { '' }
        SecondsAfterExplorer = $seconds
    }
}
$rows = @($rows)
$running = @($rows | Where-Object { $null -ne $_.SecondsAfterExplorer } |
    Sort-Object SecondsAfterExplorer)
$median = $null
if ($running.Count -gt 0) {
    $values = @($running | ForEach-Object { $_.SecondsAfterExplorer })
    $middle = [int][math]::Floor($values.Count / 2)
    $median = if ($values.Count % 2) { $values[$middle] } else { ($values[$middle - 1] + $values[$middle]) / 2 }
}

"Windows started at $boot; explorer.exe of this session at $explorerStart " +
"($([math]::Round(($explorerStart - $boot).TotalSeconds, 1)) seconds later)."
$rows | Sort-Object { if ($null -eq $_.SecondsAfterExplorer) { [double]::MaxValue } else { $_.SecondsAfterExplorer } } |
    Format-Table Source, Name, File, @{ Label = 'Seconds after explorer.exe'; Expression = {
            if ($null -eq $_.SecondsAfterExplorer) { 'not running' } else { $_.SecondsAfterExplorer } } } -AutoSize |
    Out-String -Width 200
"$($running.Count) of $($rows.Count) startup programs are running; median start " +
"$(if ($null -eq $median) { '-' } else { $median }) seconds after explorer.exe."

if ($Out) {
    $report = [ordered]@{
        Taken                    = (Get-Date).ToString('s')
        BootTime                 = $boot.ToString('s')
        ExplorerStart            = $explorerStart.ToString('s')
        MedianSecondsAfterExplorer = $median
        Programs                 = $rows
    }
    $report | ConvertTo-Json -Depth 4 | Set-Content -Encoding UTF8 -LiteralPath $Out
    "saved to $Out"
}
