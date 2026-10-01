<#
.SYNOPSIS
  One-time toolchain bootstrap for Cairn (Windows 11 x64).
.DESCRIPTION
  Installs, via winget:
    - Rust (rustup, stable-x86_64-pc-windows-msvc)
    - Microsoft Visual Studio 2022 Build Tools (MSVC v143 + Windows 11 SDK) -- required by BOTH the C++ DLL and the Rust MSVC linker
    - CMake
    - Python 3.12 (python.org build, includes the 'py' launcher)
    - Git
  Must run elevated. Logs to scripts\bootstrap.log so the calling (non-elevated) window can read the result.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$log  = Join-Path $PSScriptRoot 'bootstrap.log'
Start-Transcript -Path $log -Force | Out-Null

function Step($msg) { Write-Host "`n=== $msg ===" -ForegroundColor Cyan }

function Install-Pkg {
    param([string]$Id, [string[]]$Extra = @())
    Step "winget install $Id"
    $args = @('install', '--id', $Id, '--exact', '--silent',
              '--accept-package-agreements', '--accept-source-agreements',
              '--disable-interactivity') + $Extra
    & winget @args
    Write-Host "exit code: $LASTEXITCODE"
}

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { Write-Warning "Not elevated - machine-scope installs (Build Tools, CMake, Git) will fail." }

# 1. Visual Studio 2022 Build Tools: MSVC x64/x86 compiler + Windows 11 SDK + CMake tools.
#    --override replaces winget's default args so we control the workload set exactly.
#    NOTE: build the string first — inside @( ... ) the comma binds tighter than '+', which would
#    split the override into separate winget arguments.
$vsOverride = '--quiet --wait --norestart --nocache' +
    ' --add Microsoft.VisualStudio.Workload.VCTools' +
    ' --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64' +
    ' --add Microsoft.VisualStudio.Component.Windows11SDK.22621' +
    ' --add Microsoft.VisualStudio.Component.VC.CMake.Project' +
    ' --includeRecommended'
Install-Pkg 'Microsoft.VisualStudio.2022.BuildTools' @('--override', $vsOverride)

# 2. CMake (standalone, on PATH for everyone)
Install-Pkg 'Kitware.CMake' @('--scope', 'machine')

# 3. Git
Install-Pkg 'Git.Git' @('--scope', 'machine')

# 4. Python 3.12 (python.org). Installs the 'py' launcher which our tasks use to sidestep the Store alias.
Install-Pkg 'Python.Python.3.12' @('--scope', 'machine',
    '--override', '/quiet InstallAllUsers=1 PrependPath=1 Include_launcher=1 Include_test=0')

# 5. Rustup. Installed per-user; sets stable-msvc as default toolchain.
Install-Pkg 'Rustlang.Rustup' @('--override', '-y --default-toolchain stable-x86_64-pc-windows-msvc --profile default')

Step 'Refreshing PATH for verification'
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')

Step 'Post-install: rustup components'
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path (Join-Path $cargoBin 'rustup.exe')) {
    & (Join-Path $cargoBin 'rustup.exe') component add clippy rustfmt rust-analyzer
} else {
    Write-Warning "rustup.exe not found at $cargoBin"
}

Step 'Verification'
foreach ($t in 'cargo', 'rustc', 'cmake', 'git', 'py') {
    $c = Get-Command $t -ErrorAction SilentlyContinue
    if ($c) { Write-Host ("{0,-6} OK  {1}" -f $t, $c.Source) -ForegroundColor Green }
    else    { Write-Host ("{0,-6} MISSING" -f $t) -ForegroundColor Red }
}
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) {
    $vs = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($vs) { Write-Host "MSVC   OK  $vs" -ForegroundColor Green } else { Write-Host "MSVC   MISSING (VC tools component not found)" -ForegroundColor Red }
} else { Write-Host "MSVC   MISSING (vswhere not found)" -ForegroundColor Red }

Write-Host "`nBootstrap finished. Open a NEW terminal / restart VS Code so PATH changes apply." -ForegroundColor Yellow
Stop-Transcript | Out-Null
