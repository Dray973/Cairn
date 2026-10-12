<#
.SYNOPSIS
  Exports registry keys with reg.exe and lists the values that changed since an earlier export.
.DESCRIPTION
  Finds the values a Windows Settings switch writes: export the keys, change the switch, export
  them again under another label and compare the two exports. Each key in -Keys is a reg.exe
  path such as 'HKCU\Software\Microsoft'. The exports are .reg text files in -Root\<Label>.

  With -Against, every value that appeared, disappeared or changed between the export labelled
  -Against and this one is printed and saved to -Root\diff_<Against>_<Label>.txt, together with
  the keys that appeared or disappeared. Values whose path matches one of the -Ignore patterns
  (timestamps, caches and counters that change on their own) are only counted.

  The registry is only read: reg.exe export writes the .reg files and nothing is ever imported.
  The exports hold whatever the keys hold, app names and paths included, so -Root stays outside
  the repository.
.EXAMPLE
  .\scripts\verification\regdiff.ps1 -Keys 'HKCU\Software\Microsoft','HKLM\SOFTWARE\Policies' -Label before
  # change the switch in Settings, then:
  .\scripts\verification\regdiff.ps1 -Keys 'HKCU\Software\Microsoft','HKLM\SOFTWARE\Policies' -Label after -Against before
#>
param(
    [Parameter(Mandatory = $true)] [string[]]$Keys,
    [Parameter(Mandatory = $true)] [string]$Label,
    [string]$Against,
    [string]$Root = (Join-Path $env:LOCALAPPDATA 'PCOptimizerDev\regdiff'),
    [string[]]$Ignore = @(
        '\\LastUpdate$', 'Timestamp', 'LastAccessed', 'LastSeen', 'LastUsedTime', 'LastWriteTime',
        'MRU', '\\Bags\\', 'UserAssist', 'TrayNotify', 'FeatureUsage', 'RecentDocs', '\\CloudStore\\',
        'ContentDeliveryManager\\Health\\', 'ContentDeliveryManager\\Subscriptions\\',
        'Search\\JumplistData\\', '\\Telemetry\\PersistentPropertyBag\\', 'SessionInfo'
    )
)
$ErrorActionPreference = 'Stop'

foreach ($name in @($Label, $Against)) {
    if ($name -and $name -notmatch '^[A-Za-z0-9_-]+$') {
        throw "Labels use letters, digits, '-' and '_' only: '$name'."
    }
}

function Get-ExportName([string]$Key) {
    return (($Key -replace '[\\/:*?"<>| ]', '_') + '.reg')
}

# A value line of a .reg file: the quoted name (quotes and backslashes in it are escaped with a
# backslash) and the data after '='.
$valueLine = [regex]'^"((?:[^"\\]|\\.)*)"=(.*)$'

# Values of every .reg file in one export folder: "KEY\name" -> data, and "KEY\" -> '(key)'.
# Hex data split over several lines is joined; the default value is named '(Default)'.
function Read-Export([string]$Folder) {
    $values = New-Object 'System.Collections.Generic.Dictionary[string,string]' ([StringComparer]::OrdinalIgnoreCase)
    foreach ($file in Get-ChildItem -LiteralPath $Folder -Filter '*.reg' -File) {
        $key = $null
        $pending = $null
        foreach ($line in [System.IO.File]::ReadLines($file.FullName, [System.Text.Encoding]::Unicode)) {
            if ($null -ne $pending) {
                $values[$pending] += $line.Trim().TrimEnd('\')
                if (-not $line.EndsWith('\')) { $pending = $null }
                continue
            }
            if ($line.StartsWith('[') -and $line.EndsWith(']')) {
                $key = $line.Substring(1, $line.Length - 2)
                $values["$key\"] = '(key)'
                continue
            }
            if ($null -eq $key -or $line.Length -eq 0) { continue }
            $name = $null
            $data = $null
            if ($line.StartsWith('@=')) {
                $name = '(Default)'
                $data = $line.Substring(2)
            } else {
                $match = $valueLine.Match($line)
                if ($match.Success) {
                    $name = $match.Groups[1].Value -replace '\\(.)', '$1'
                    $data = $match.Groups[2].Value
                }
            }
            if ($null -eq $name) { continue }
            $full = "$key\$name"
            if ($data.EndsWith('\') -and $data -match '^hex') {
                $values[$full] = $data.TrimEnd('\')
                $pending = $full
            } else {
                $values[$full] = $data
            }
        }
    }
    return $values
}

$folder = Join-Path $Root $Label
New-Item -ItemType Directory -Force -Path $folder | Out-Null
foreach ($key in $Keys) {
    $file = Join-Path $folder (Get-ExportName $key)
    # reg.exe reports a missing key on stderr; that must not stop the script.
    $ErrorActionPreference = 'Continue'
    & reg.exe export $key $file /y 2>&1 | Out-Null
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) {
        Write-Warning "$key could not be exported (it may not exist); it counts as empty."
        if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file }
    }
}
"export '$Label': " + ((Get-ChildItem -LiteralPath $folder -Filter '*.reg' -File |
    ForEach-Object { "$($_.BaseName) $([int]($_.Length / 1KB)) KB" }) -join ', ')

if (-not $Against) { return }
$previousFolder = Join-Path $Root $Against
if (-not (Test-Path -LiteralPath $previousFolder -PathType Container)) {
    throw "There is no export labelled '$Against' in $Root."
}
$before = Read-Export $previousFolder
$after = Read-Export $folder
$names = New-Object 'System.Collections.Generic.SortedSet[string]' ([StringComparer]::OrdinalIgnoreCase)
foreach ($name in $before.Keys) { [void]$names.Add($name) }
foreach ($name in $after.Keys) { [void]$names.Add($name) }

$lines = New-Object System.Collections.Generic.List[string]
$ignored = 0
foreach ($name in $names) {
    $old = if ($before.ContainsKey($name)) { $before[$name] } else { 'None' }
    $new = if ($after.ContainsKey($name)) { $after[$name] } else { 'None' }
    if ($old -ceq $new) { continue }
    if ($Ignore | Where-Object { $name -match $_ }) {
        $ignored++
        continue
    }
    if ($name.EndsWith('\')) {
        $what = if ($new -eq 'None') { 'key removed' } else { 'key added' }
        $lines.Add("$($name.TrimEnd('\')): $what")
    } else {
        $lines.Add("${name}:")
        $lines.Add("    $old  ->  $new")
    }
}
$lines.Add("($ignored changed values matched -Ignore and are not listed)")
$out = Join-Path $Root "diff_${Against}_$Label.txt"
[System.IO.File]::WriteAllLines($out, $lines, (New-Object System.Text.UTF8Encoding($false)))
$lines
"saved to $out"
