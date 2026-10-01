<#
.SYNOPSIS
  Builds the installed copy of Cairn: a staged folder with Cairn.exe, optctl.exe,
  cairn-maintenance.exe, a private Python 3.12 runtime and the app, and (with Inno Setup 6.3 or
  later) the setup program.
.DESCRIPTION
  1. Version: Cargo.toml [workspace.package] version; ui\optimizer\__init__.py and
     ui\pyproject.toml must carry the same one.
  2. Checks, unless -SkipChecks: rustfmt, clippy, the Rust 1.80 check, the Rust tests, ruff and
     the UI tests (scripts\pytest_serial.ps1).
  3. Telemetry DLL: CMake with Visual Studio 2022 x64, Release, without AVX2 (TEL_AVX2=OFF),
     into build\dist-cpp.
  4. Rust: the launcher, the command-line tools and the engine in the release profile for
     x86-64-v2, with the repository and cargo folders remapped out of every embedded source
     path, into -TargetDir.
  5. Stage (build\dist\stage, recreated): the runtime of `py -3.12` without its tests, tools
     and site-packages, the VC runtime of the Visual Studio toolset, python312._pth, the app
     with its natives, the runtime packages of requirements-runtime.txt (exact hashes, wheels
     only), the licenses, and bytecode compiled in advance.
  6. Gates: `Cairn.exe --check` and `Cairn.exe --self-test` must pass (both read-only: they
     never elevate, prompt or write logs; the self-test runs with Tcl variables set that the
     launcher must remove); no staged file may be a PDB or contain the path of the repository
     or of the user profile (UTF-8 or UTF-16). Unsigned programs are listed.
  7. Unless -StageOnly, when ISCC.exe is found: build\dist\Cairn-<version>-setup.exe,
     SHA256SUMS.txt and Cairn-<version>-symbols.zip (the PDBs; not shipped).

  Writes only under build\ and -TargetDir, apart from pip's and cargo's download caches and,
  unless -SkipChecks, what the checks of step 2 write like any test run (cargo's target folder,
  caches in ui\, temporary folders, the registry sandbox HKCU\Software\PCOptimizer\SelfTest).
  It never installs anything on this PC, never runs the setup and never asks for administrator
  rights. The staged folder is writable by this user, so the staged Cairn.exe never elevates;
  use it for checks only.
.PARAMETER SkipChecks
  Skips step 2.
.PARAMETER StageOnly
  Stops after step 6.
.PARAMETER IsccPath
  Inno Setup's ISCC.exe; default: the per-machine or per-user Inno Setup 6 folder.
.PARAMETER TargetDir
  Cargo's target folder for the release build (default target\dist).
.EXAMPLE
  .\scripts\build_release.ps1
.EXAMPLE
  .\scripts\build_release.ps1 -StageOnly -SkipChecks
#>
[CmdletBinding()]
param(
    [switch]$SkipChecks,
    [switch]$StageOnly,
    [string]$IsccPath,
    [string]$TargetDir
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot

# Folders of the Python runtime and files the stage leaves out.
$LibExcluded = @('site-packages', 'test', 'idlelib', 'turtledemo', 'ensurepip', 'lib2to3', 'pydoc_data',
    'venv', '__pycache__')
$DllsExcluded = @('*.ico', '_msi.pyd')
$TclExcludedDirs = @('nmake', 'tix8.4.3', 'dde1.4', 'reg1.3', 'demos')
$TclExcludedFiles = @('*.lib', '*Config.sh')
# Files the runtime and the launcher need; the stage is refused without them.
$RuntimeRequired = @('python312.dll', 'python3.dll', 'LICENSE.txt', 'DLLs\_tkinter.pyd', 'tcl\tcl8.6\init.tcl',
    'tcl\tk8.6\tk.tcl', 'tcl\tcl8')
$PthLines = @('Lib', 'DLLs', 'app', 'app\site')
# Variables the staged launcher must remove before the runtime loads; set during the self-test.
$LauncherRemovesTcl = @('TCLLIBPATH', 'TIX_LIBRARY', 'TCL8.6_TM_PATH', 'TCL8_6_TM_PATH')
$Programs = @{ 'cairn.exe' = 'Cairn.exe'; 'optctl.exe' = 'optctl.exe'; 'cairn-maintenance.exe' = 'cairn-maintenance.exe' }
$SymbolFiles = @('cairn.pdb', 'optctl.pdb', 'cairn_maintenance.pdb', 'optimizer_engine.pdb')

function Write-Step([string]$Text) {
    Write-Host "==> $Text" -ForegroundColor Cyan
}

# Runs a native command and fails when it exits with a non-zero code. Native programs report
# progress on stderr, which must not count as an error.
function Invoke-Native([string]$What, [scriptblock]$Command) {
    Write-Step $What
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $global:LASTEXITCODE = 0
    try {
        & $Command
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    if ($code -ne 0) { throw "$What failed with exit code $code." }
}

function Get-TomlVersion([string]$Text, [string]$Section) {
    $pattern = '(?ms)^\[' + [regex]::Escape($Section) + '\]\s*$(.*?)(?=^\[|\z)'
    $body = [regex]::Match($Text, $pattern)
    if (-not $body.Success) { return $null }
    $version = [regex]::Match($body.Groups[1].Value, '(?m)^version\s*=\s*"([^"]+)"')
    if ($version.Success) { return $version.Groups[1].Value }
    return $null
}

# The release version; fails when the three places that carry it disagree.
function Get-CairnVersion([string]$RepoRoot) {
    $cargo = Get-TomlVersion (Get-Content -Raw (Join-Path $RepoRoot 'Cargo.toml')) 'workspace.package'
    $pyproject = Get-TomlVersion (Get-Content -Raw (Join-Path $RepoRoot 'ui\pyproject.toml')) 'project'
    $init = [regex]::Match((Get-Content -Raw (Join-Path $RepoRoot 'ui\optimizer\__init__.py')),
        '(?m)^__version__\s*=\s*"([^"]+)"')
    if (-not $cargo) { throw 'Cargo.toml has no [workspace.package] version.' }
    if (-not $init.Success) { throw 'ui\optimizer\__init__.py has no __version__.' }
    if ($cargo -ne $pyproject -or $cargo -ne $init.Groups[1].Value) {
        throw "Versions differ: Cargo.toml $cargo, ui\pyproject.toml $pyproject, __init__.py $($init.Groups[1].Value)."
    }
    return $cargo
}

# robocopy with its success codes (below 8); no file or folder lists.
function Copy-Tree([string]$From, [string]$To, [string[]]$ExcludeDirs = @(), [string[]]$ExcludeFiles = @()) {
    $arguments = @($From, $To, '/E', '/NFL', '/NDL', '/NJH', '/NJS', '/NP', '/R:1', '/W:1')
    if ($ExcludeDirs.Count) { $arguments += '/XD'; $arguments += $ExcludeDirs }
    if ($ExcludeFiles.Count) { $arguments += '/XF'; $arguments += $ExcludeFiles }
    & robocopy.exe @arguments | Out-Null
    if ($LASTEXITCODE -ge 8) { throw "Copying $From failed (robocopy exit code $LASTEXITCODE)." }
    $global:LASTEXITCODE = 0
}

# The CPython 3.12 runtime of `py -3.12` without its tests, tools and site-packages.
function Copy-Runtime([string]$Prefix, [string]$Stage) {
    foreach ($name in $RuntimeRequired) {
        if (-not (Test-Path (Join-Path $Prefix $name))) { throw "The Python runtime in $Prefix has no $name." }
    }
    Copy-Item (Join-Path $Prefix 'python312.dll'), (Join-Path $Prefix 'python3.dll') $Stage
    Copy-Tree (Join-Path $Prefix 'Lib') (Join-Path $Stage 'Lib') -ExcludeDirs $LibExcluded
    Copy-Tree (Join-Path $Prefix 'DLLs') (Join-Path $Stage 'DLLs') -ExcludeFiles $DllsExcluded
    Copy-Tree (Join-Path $Prefix 'tcl') (Join-Path $Stage 'tcl') -ExcludeDirs $TclExcludedDirs -ExcludeFiles $TclExcludedFiles
    Set-Content -Path (Join-Path $Stage 'python312._pth') -Value $PthLines -Encoding Ascii
}

# The x64 VC runtime folder (Microsoft.VC14x.CRT) of the newest Visual Studio toolset.
function Find-VcRuntime {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $null }
    $installs = @(& $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
    foreach ($install in $installs) {
        $redist = Join-Path $install 'VC\Redist\MSVC'
        $versions = @(Get-ChildItem $redist -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^\d+\.\d+\.\d+$' } | Sort-Object { [version]$_.Name } -Descending)
        foreach ($version in $versions) {
            $crt = Get-ChildItem (Join-Path $version.FullName 'x64') -Directory -Filter 'Microsoft.VC14*.CRT' -ErrorAction SilentlyContinue |
                Select-Object -First 1
            if ($crt -and (Test-Path (Join-Path $crt.FullName 'vcruntime140_1.dll'))) { return $crt.FullName }
        }
    }
    return $null
}

# License texts of Cairn, Python, Tcl/Tk and every runtime package, under <stage>\licenses.
function Copy-Licenses([string]$RepoRoot, [string]$Prefix, [string]$Stage) {
    $licenses = Join-Path $Stage 'licenses'
    New-Item -ItemType Directory -Force $licenses | Out-Null
    Copy-Item (Join-Path $RepoRoot 'LICENSE') (Join-Path $licenses 'Cairn-LICENSE.txt')
    Copy-Item (Join-Path $Prefix 'LICENSE.txt') (Join-Path $licenses 'Python-LICENSE.txt')
    # Tcl and Tk share one license text; python.org's runtime ships it with Tk.
    $terms = @(@('tcl8.6', 'Tcl-license.terms'), @('tk8.6', 'Tk-license.terms') | Where-Object {
            Test-Path (Join-Path $Prefix "tcl\$($_[0])\license.terms")
        })
    if (-not $terms.Count) { throw "The Python runtime in $Prefix has no Tcl/Tk license.terms." }
    foreach ($pair in $terms) {
        Copy-Item (Join-Path $Prefix "tcl\$($pair[0])\license.terms") (Join-Path $licenses $pair[1])
    }
    foreach ($info in Get-ChildItem (Join-Path $Stage 'app\site') -Directory -Filter '*.dist-info') {
        $package = $info.Name -replace '-[^-]+\.dist-info$', ''
        $target = Join-Path $licenses $package
        $files = @(Get-ChildItem $info.FullName -File | Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING|NOTICE)' })
        $folder = Join-Path $info.FullName 'licenses'
        if (Test-Path $folder) { $files += @(Get-ChildItem $folder -File -Recurse) }
        if (-not $files.Count) { throw "The package $package ships no license file." }
        New-Item -ItemType Directory -Force $target | Out-Null
        foreach ($file in $files) { Copy-Item $file.FullName (Join-Path $target $file.Name) -Force }
    }
}

# Byte strings that must not appear in a staged file: each path with backslashes, slashes and
# doubled backslashes, in UTF-8 and UTF-16LE, as Latin-1 text for a byte-wise search.
function Get-ForbiddenStrings([string[]]$Paths) {
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    $found = New-Object System.Collections.Generic.List[string]
    foreach ($path in $Paths) {
        if (-not $path) { continue }
        $trimmed = $path.TrimEnd('\')
        foreach ($variant in @($trimmed, $trimmed.Replace('\', '/'), $trimmed.Replace('\', '\\'))) {
            foreach ($encoding in @([System.Text.Encoding]::UTF8, [System.Text.Encoding]::Unicode)) {
                $text = $latin1.GetString($encoding.GetBytes($variant))
                if (-not $found.Contains($text)) { $found.Add($text) }
            }
        }
    }
    return $found.ToArray()
}

# Staged files that are PDBs or contain one of `Paths` (ASCII case ignored). Empty when clean.
function Find-HygieneProblems([string]$Stage, [string[]]$Paths) {
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    $needles = Get-ForbiddenStrings $Paths
    $problems = New-Object System.Collections.Generic.List[string]
    foreach ($file in Get-ChildItem $Stage -Recurse -File) {
        $relative = $file.FullName.Substring($Stage.TrimEnd('\').Length + 1)
        if ($file.Extension -eq '.pdb') {
            $problems.Add("$relative is a PDB")
            continue
        }
        $text = $latin1.GetString([System.IO.File]::ReadAllBytes($file.FullName))
        foreach ($needle in $needles) {
            if ($text.IndexOf($needle, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
                $problems.Add("$relative contains a build-machine path")
                break
            }
        }
    }
    return $problems.ToArray()
}

# Runs the staged launcher (a windowed program) in this console and returns its exit code.
function Invoke-Staged([string]$Program, [string[]]$Arguments) {
    $process = Start-Process -FilePath $Program -ArgumentList $Arguments -NoNewWindow -PassThru
    # Reading the handle first keeps the exit code available after the process ends.
    $null = $process.Handle
    $process.WaitForExit()
    return $process.ExitCode
}

function Find-Iscc([string]$Explicit) {
    if ($Explicit) {
        if (Test-Path $Explicit) { return (Resolve-Path $Explicit).Path }
        throw "ISCC.exe not found at $Explicit."
    }
    foreach ($candidate in @(
            (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
            (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
            (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'))) {
        if (Test-Path $candidate) { return $candidate }
    }
    return $null
}

function Invoke-Checks([string]$RepoRoot) {
    $python = Join-Path $RepoRoot '.venv\Scripts\python.exe'
    $env:PYO3_PYTHON = $python
    Push-Location $RepoRoot
    try {
        Invoke-Native 'rustfmt check' { cargo fmt --all -- --check }
        Invoke-Native 'clippy' { cargo clippy --workspace --all-targets -- -D warnings }
        Invoke-Native 'Rust 1.80 check' { cargo +1.80 check --workspace --all-targets }
        Invoke-Native 'Rust tests' { cargo test --workspace }
    } finally {
        Pop-Location
    }
    Push-Location (Join-Path $RepoRoot 'ui')
    try {
        Invoke-Native 'ruff' { & $python -m ruff check . }
    } finally {
        Pop-Location
    }
    # A separate PowerShell: the test script ends with `exit`.
    $tests = Join-Path $RepoRoot 'scripts\pytest_serial.ps1'
    Invoke-Native 'UI tests' { & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $tests tests -q }
}

function Invoke-Build {
    $version = Get-CairnVersion $Root
    Write-Host "Cairn $version" -ForegroundColor Green
    $target = if ($TargetDir) { [System.IO.Path]::GetFullPath($TargetDir) } else { Join-Path $Root 'target\dist' }
    $dist = Join-Path $Root 'build\dist'
    $stage = Join-Path $dist 'stage'
    $cpp = Join-Path $Root 'build\dist-cpp'

    if (-not $SkipChecks) { Invoke-Checks $Root }

    $cmake = (Get-Command cmake -ErrorAction SilentlyContinue)
    if (-not $cmake) { throw 'cmake was not found on PATH.' }
    Invoke-Native 'configure the telemetry DLL' {
        & $cmake.Source -S $Root -B $cpp -G 'Visual Studio 17 2022' -A x64 -DTEL_AVX2=OFF -DTEL_BUILD_TESTS=OFF
    }
    Invoke-Native 'build the telemetry DLL' { & $cmake.Source --build $cpp --config Release --parallel }
    $telemetry = Join-Path $cpp 'bin\Release\optimizer_telemetry.dll'

    # No double quotes inside these: Windows PowerShell drops them when it starts a program.
    $prefix = (& py -3.12 -c 'import sys; print(sys.base_prefix)').Trim()
    $basePython = (& py -3.12 -c 'import sys; print(sys.executable)').Trim()
    if (-not (Test-Path $basePython) -or -not (Test-Path (Join-Path $prefix 'python312.dll'))) {
        throw "Python 3.12 was not found (py -3.12 gave $basePython)."
    }
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
    # CARGO_ENCODED_RUSTFLAGS replaces .cargo\config.toml's x86-64-v3 and keeps paths with
    # spaces in one flag each.
    $flags = @('-C', 'target-cpu=x86-64-v2', "--remap-path-prefix=$cargoHome=cargo",
        "--remap-path-prefix=$rustupHome=rustup", "--remap-path-prefix=$Root=cairn")
    $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f
    $env:PYO3_PYTHON = $basePython
    Push-Location $Root
    try {
        Invoke-Native 'build Cairn.exe, optctl.exe, cairn-maintenance.exe and the engine' {
            cargo build --release --locked -p optimizer_engine -p optctl -p cairn --target-dir $target
        }
    } finally {
        Pop-Location
    }
    $release = Join-Path $target 'release'

    Write-Step "stage $stage"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Runtime $prefix $stage
    $crt = Find-VcRuntime
    if (-not $crt) { throw 'The x64 VC runtime of Visual Studio 2022 (VC\Redist\MSVC) was not found.' }
    Copy-Item (Join-Path $crt 'vcruntime140.dll'), (Join-Path $crt 'vcruntime140_1.dll') $stage
    $app = Join-Path $stage 'app\optimizer'
    $ui = Join-Path $Root 'ui\optimizer'
    Copy-Tree $ui $app -ExcludeDirs @('__pycache__', (Join-Path $ui 'native')) -ExcludeFiles @('*.pyc')
    $native = Join-Path $app 'native'
    New-Item -ItemType Directory -Force $native | Out-Null
    Copy-Item (Join-Path $release 'optimizer_engine.dll') (Join-Path $native 'optimizer_engine.pyd')
    Copy-Item $telemetry (Join-Path $native 'optimizer_telemetry.dll')
    Invoke-Native 'install the runtime packages' {
        & py -3.12 -m pip install --isolated --require-hashes --no-deps --only-binary=:all: --no-compile `
            --disable-pip-version-check --no-warn-script-location --target (Join-Path $stage 'app\site') `
            -r (Join-Path $Root 'requirements-runtime.txt')
    }
    foreach ($name in $Programs.Keys) { Copy-Item (Join-Path $release $name) (Join-Path $stage $Programs[$name]) }
    Copy-Licenses $Root $prefix $stage
    Invoke-Native 'compile the bytecode' {
        & py -3.12 -m compileall -q -j 0 --invalidation-mode unchecked-hash -s $stage (Join-Path $stage 'Lib') (Join-Path $stage 'app')
    }

    $launcher = Join-Path $stage 'Cairn.exe'
    Write-Step 'Cairn.exe --check'
    $code = Invoke-Staged $launcher @('--check')
    if ($code -ne 0) { throw "Cairn.exe --check failed with exit code $code." }
    $report = Join-Path $dist 'selftest.json'
    if (Test-Path $report) { Remove-Item -Force $report }
    Write-Step 'Cairn.exe --self-test'
    # Tcl variables the launcher must remove: the self-test reports any that reaches the app.
    $removed = @{}
    foreach ($name in $LauncherRemovesTcl) {
        $removed[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, (Join-Path $dist 'no-such-folder'), 'Process')
    }
    try {
        $code = Invoke-Staged $launcher @('--self-test', "`"$report`"")
    } finally {
        foreach ($name in $removed.Keys) { [Environment]::SetEnvironmentVariable($name, $removed[$name], 'Process') }
    }
    if ($code -ne 0) { throw "Cairn.exe --self-test failed with exit code $code; see $report." }
    $result = Get-Content -Raw $report | ConvertFrom-Json
    $errors = @($result.errors)
    if ($errors.Count) { throw "The self-test reported: $($errors -join '; ')" }
    # Otherwise the report says nothing about the launcher's settings an interactive start checks.
    if ($result.installed -ne $true) { throw "The self-test did not run as the installed launcher; see $report." }

    Write-Step 'hygiene scan'
    $problems = @(Find-HygieneProblems $stage @($Root, $env:USERPROFILE))
    if ($problems.Count) { throw ("The stage is not clean:`n  " + ($problems -join "`n  ")) }
    $unsigned = @(Get-ChildItem $stage -Recurse -File -Include *.exe, *.dll, *.pyd |
        Where-Object { (Get-AuthenticodeSignature $_.FullName).Status -ne 'Valid' })
    Write-Host ("{0} unsigned program files (SmartScreen and Smart App Control treat them as unknown):" -f $unsigned.Count)
    foreach ($file in $unsigned) { Write-Host ('  ' + $file.FullName.Substring($stage.Length + 1)) }

    if ($StageOnly) {
        Write-Host "Stage ready: $stage" -ForegroundColor Green
        return
    }
    $iscc = Find-Iscc $IsccPath
    if (-not $iscc) {
        Write-Host "Stage ready: $stage" -ForegroundColor Green
        Write-Warning 'Inno Setup 6.3+ not found; install it (winget install JRSoftware.InnoSetup) and run this script again.'
        return
    }
    Invoke-Native 'build the setup' {
        & $iscc "/DAppVersion=$version" "/DStageDir=$stage" "/DOutputDir=$dist" (Join-Path $Root 'installer\cairn.iss')
    }
    $setup = Join-Path $dist "Cairn-$version-setup.exe"
    $symbols = Join-Path $dist "Cairn-$version-symbols.zip"
    $pdbs = @($SymbolFiles | ForEach-Object { Join-Path $release $_ } | Where-Object { Test-Path $_ })
    $pdbs += @(Join-Path $cpp 'bin\Release\optimizer_telemetry.pdb' | Where-Object { Test-Path $_ })
    if (Test-Path $symbols) { Remove-Item -Force $symbols }
    if ($pdbs.Count) { Compress-Archive -Path $pdbs -DestinationPath $symbols }
    $sums = foreach ($file in @($setup, $symbols) | Where-Object { Test-Path $_ }) {
        '{0}  {1}' -f (Get-FileHash $file -Algorithm SHA256).Hash.ToLower(), (Split-Path -Leaf $file)
    }
    Set-Content -Path (Join-Path $dist 'SHA256SUMS.txt') -Value $sums -Encoding Ascii
    Write-Host "Setup ready: $setup" -ForegroundColor Green
}

# Dot-sourcing loads the functions only.
if ($MyInvocation.InvocationName -eq '.') { return }

$saved = @{}
foreach ($name in @('CARGO_ENCODED_RUSTFLAGS', 'PYO3_PYTHON')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    Invoke-Build
} finally {
    # A null value removes the variable when the caller did not have it.
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
