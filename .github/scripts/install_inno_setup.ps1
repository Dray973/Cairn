<#
.SYNOPSIS
  Installs the pinned Inno Setup on a GitHub-hosted runner and points ISCC at it.
.DESCRIPTION
  Downloads the Inno Setup installer that jrsoftware.org's download page links to, checks its
  SHA-256 against -Sha256, the hash that the winget manifest of JRSoftware.InnoSetup lists for
  that version (change -Version, -Url and -Sha256 together), installs it for the runner's
  account into the runner's temporary folder and adds ISCC=<its ISCC.exe> to GITHUB_ENV, so
  scripts\build_release.ps1 compiles the setup with it whatever Inno Setup the runner image
  carries.

  It installs software, so it refuses to run anywhere but on a GitHub-hosted runner, which is
  discarded after the job.
#>
param(
    [string]$Version = '6.7.3',
    [string]$Url = 'https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe',
    [string]$Sha256 = '9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732'
)

$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'This script installs Inno Setup; it runs only on a GitHub-hosted runner.'
}

$installer = Join-Path $env:RUNNER_TEMP "innosetup-$Version.exe"
Invoke-WebRequest -Uri $Url -OutFile $installer -UseBasicParsing
$hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
if ($hash -ne $Sha256) { throw "The Inno Setup installer has the SHA-256 $hash, not $Sha256." }

$folder = Join-Path $env:RUNNER_TEMP 'InnoSetup'
$arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', '/CURRENTUSER', "/DIR=$folder")
$process = Start-Process -FilePath $installer -ArgumentList $arguments -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "The Inno Setup installer failed with exit code $($process.ExitCode)." }
$iscc = Join-Path $folder 'ISCC.exe'
if (-not (Test-Path -LiteralPath $iscc)) { throw "ISCC.exe was not installed into $folder." }

[System.IO.File]::AppendAllText($env:GITHUB_ENV, "ISCC=$iscc`n")
Write-Host "Inno Setup $Version installed: $iscc"
