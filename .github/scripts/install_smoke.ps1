<#
.SYNOPSIS
  Installs, upgrades and uninstalls the setup that scripts\build_release.ps1 built, on a
  GitHub-hosted runner.
.DESCRIPTION
  1. Installs the setup of -Dist silently and checks the installed copy: it holds only files of
     the stage (and the uninstaller), `Cairn.exe --self-test` reports no error and ran as the
     installed launcher, and `optctl.exe journal pending` exits with 0.
  2. Uninstalls it silently and checks that the program folder, the uninstall entry and the
     \Cairn\ folder of Task Scheduler are gone.
  3. Installs the published setup of the previous release (its SHA-256 checked), installs the
     new setup over it, checks the installed copy as in 1 (an upgrade leaves no file of the
     previous version behind) and uninstalls it as in 2.

  It installs software, so it refuses to run anywhere but on a GitHub-hosted runner, which is
  discarded after the job.
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$Dist,
    [string]$PreviousUrl = 'https://github.com/Dray973/Cairn/releases/download/v0.2.0/Cairn-0.2.0-setup.exe',
    [string]$PreviousSha256 = 'a865f96e6a7e3fb28aa68dbb66aa5cec08d205504290958aa92652f720f4972a'
)

$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'This script installs and uninstalls Cairn; it runs only on a GitHub-hosted runner.'
}

$InstallDir = Join-Path $env:ProgramFiles 'Cairn'
# optimizer_core::app::UNINSTALL_KEY (installer\cairn.iss AppId).
$UninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{616A27F7-C15A-4244-ACD1-B7F536649EA0}_is1'
# Files of the installed copy that the stage does not hold: Inno Setup's uninstaller and its data.
$UninstallerFiles = @('unins000.exe', 'unins000.dat')
$Logs = Join-Path $env:RUNNER_TEMP 'install-smoke'
New-Item -ItemType Directory -Force $Logs | Out-Null

$distFolder = (Resolve-Path -LiteralPath $Dist).Path
$setups = @(Get-ChildItem -LiteralPath $distFolder -File -Filter 'Cairn-*-setup.exe')
if ($setups.Count -ne 1) { throw "Expected one setup in $distFolder, found $($setups.Count)." }
$Setup = $setups[0].FullName
$Stage = Join-Path $distFolder 'stage'
if (-not (Test-Path -LiteralPath (Join-Path $Stage 'Cairn.exe'))) { throw "There is no stage in $Stage." }

function Write-Step([string]$Text) {
    Write-Host "==> $Text" -ForegroundColor Cyan
}

# The files under $Folder, relative to it.
function Get-RelativeFiles([string]$Folder) {
    $prefix = $Folder.TrimEnd('\').Length + 1
    return @(Get-ChildItem -LiteralPath $Folder -Recurse -File | ForEach-Object { $_.FullName.Substring($prefix) })
}

function Install-Setup([string]$Path, [string]$Name) {
    Write-Step "install ($Name): $Path"
    $log = Join-Path $Logs "$Name-install.log"
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=$log")
    $process = Start-Process -FilePath $Path -ArgumentList $arguments -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Installing ($Name) failed with exit code $($process.ExitCode); see $log." }
    if (-not (Test-Path -LiteralPath (Join-Path $InstallDir 'Cairn.exe'))) { throw "Installing ($Name) left no Cairn.exe in $InstallDir." }
}

function Test-InstalledCopy([string]$Name) {
    Write-Step "check the installed copy ($Name)"
    $stageFiles = @(Get-RelativeFiles $Stage) + $UninstallerFiles
    $extra = @(Get-RelativeFiles $InstallDir | Where-Object { $stageFiles -notcontains $_ })
    if ($extra.Count) {
        throw "The installed copy holds files the stage does not: $(($extra | Select-Object -First 20) -join ', ')."
    }
    $report = Join-Path $Logs "$Name-selftest.json"
    $process = Start-Process -FilePath (Join-Path $InstallDir 'Cairn.exe') -ArgumentList @('--self-test', "`"$report`"") -NoNewWindow -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Cairn.exe --self-test failed with exit code $($process.ExitCode); see $report." }
    $result = Get-Content -Raw -LiteralPath $report | ConvertFrom-Json
    $errors = @($result.errors)
    if ($errors.Count) { throw "The self-test reported: $($errors -join '; ')" }
    if ($result.installed -ne $true) { throw 'The self-test did not run as the installed launcher.' }
    & (Join-Path $InstallDir 'optctl.exe') journal pending
    if ($LASTEXITCODE -ne 0) { throw "optctl.exe journal pending exited with $LASTEXITCODE." }
}

function Uninstall-Cairn([string]$Name) {
    Write-Step "uninstall ($Name)"
    $log = Join-Path $Logs "$Name-uninstall.log"
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=$log")
    $process = Start-Process -FilePath (Join-Path $InstallDir 'unins000.exe') -ArgumentList $arguments -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Uninstalling ($Name) failed with exit code $($process.ExitCode); see $log." }
    # The uninstaller finishes from a copy of itself in the temporary folder.
    $deadline = (Get-Date).AddMinutes(3)
    while (((Test-Path -LiteralPath $InstallDir) -or (Test-Path -LiteralPath $UninstallKey)) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2
    }
    $left = @()
    if (Test-Path -LiteralPath $InstallDir) { $left += "the folder $InstallDir ($(@(Get-RelativeFiles $InstallDir).Count) files)" }
    if (Test-Path -LiteralPath $UninstallKey) { $left += 'the uninstall entry' }
    if (@(Get-ScheduledTask -TaskPath '\Cairn\' -ErrorAction SilentlyContinue).Count) { $left += 'tasks in \Cairn\' }
    if (Test-Path -LiteralPath (Join-Path $env:SystemRoot 'System32\Tasks\Cairn')) { $left += 'the \Cairn\ task folder' }
    if ($left.Count) { throw "Uninstalling ($Name) left $($left -join ', ')." }
}

Install-Setup $Setup 'new'
Test-InstalledCopy 'new'
Uninstall-Cairn 'new'

$previous = Join-Path $env:RUNNER_TEMP (Split-Path -Leaf $PreviousUrl)
Write-Step "download $PreviousUrl"
Invoke-WebRequest -Uri $PreviousUrl -OutFile $previous -UseBasicParsing
$hash = (Get-FileHash -LiteralPath $previous -Algorithm SHA256).Hash.ToLowerInvariant()
if ($hash -ne $PreviousSha256) { throw "The previous setup has the SHA-256 $hash, not $PreviousSha256." }
Install-Setup $previous 'previous'
Install-Setup $Setup 'upgrade'
Test-InstalledCopy 'upgrade'
Uninstall-Cairn 'upgrade'

Write-Host 'Install, upgrade and uninstall passed.' -ForegroundColor Green
