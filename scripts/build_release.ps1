<#
.SYNOPSIS
  Builds the installed copy of Cairn: a staged folder with Cairn.exe, optctl.exe,
  cairn-maintenance.exe, a private Python 3.12 runtime and the app, the setup program (Inno
  Setup 6.3 or later) and the files that go with a release.
.DESCRIPTION
  1. Version: Cargo.toml [workspace.package] version; every workspace package in Cargo.lock,
     ui\pyproject.toml and ui\optimizer\__init__.py must carry the same one. CMakeLists.txt and
     telemetry.cpp take it from Cargo.toml; a version written into either must agree too.
  2. Checks, unless -SkipChecks: rustfmt, clippy, the Rust 1.80 check, the Rust tests (with
     CAIRN_RELEASE=1 unless -AllowPending, so a tweak without its verification record, or one
     whose check must be repeated, fails the build), the tests inside optctl.exe when this
     shell is elevated (their test program carries optctl's requireAdministrator manifest;
     otherwise a warning says they were skipped), ruff and the UI tests
     (scripts\pytest_serial.ps1).
  3. Telemetry DLL: CMake (the newest Visual Studio unless -Generator names another generator),
     x64, Release, without AVX2 (TEL_AVX2=OFF), into build\dist-cpp.
  4. Rust: the launcher, the command-line tools and the engine in the release profile for
     x86-64-v2, with the repository and cargo folders remapped out of every embedded source
     path and with CAIRN_REQUIRE_RESOURCES=1 (a binary without its version resource fails the
     build), into -TargetDir.
  5. Stage (build\dist\stage, recreated): the runtime of Python 3.12 (-Python, else
     `py -3.12`) without its tests, tools and site-packages, the VC runtime of the newest Visual
     Studio, python312._pth, the app with its natives, the runtime packages of
     requirements-runtime.txt (exact hashes, wheels only) without the Pillow modules the app
     does not need, the licenses (with the notices of the Rust crates in the programs and
     THIRD-PARTY-NOTICES.md), and bytecode compiled in advance.
  6. Gates: `Cairn.exe --check` and `Cairn.exe --self-test` must pass (both read-only: they
     never elevate, prompt or write logs; the self-test runs with Tcl variables set that the
     launcher must remove); the brand renderer must work with the staged Pillow and load only
     its kept modules; Cairn's own programs and libraries must carry the product name and the
     version; the notices must be there; no staged file may be a PDB or contain the path of the
     repository or of the user profile (UTF-8 or UTF-16). Unsigned programs are listed.
  7. Setup: build\dist\Cairn-<version>-setup.exe and Cairn-<version>-symbols.zip (the PDBs;
     not shipped).
  8. Release files in build\dist: SHA256SUMS.txt (the setup), release-notes.md (install steps,
     SHA-256, verification commands, `gh attestation verify` only with -Attested and
     `gh release verify` only with -ImmutableReleases, the SmartScreen paragraph and an empty
     Changes heading), signpath-metadata.txt (the exact ProductName and ProductVersion strings
     of the files a signing request restricts) and build-info.json (tools, commit and kind of
     build; not shipped).

  The release workflow runs the steps separately around its two signing rounds: -StageOnly
  runs steps 1-6 and, with -SigningDir, collects the files to sign; -PackageOnly runs step 7 on
  the existing stage, after putting the signed files back (-SignedDir); -FinalizeOnly runs
  step 8, after putting the signed setup in place (-SignedSetup).

  Writes only under build\ and -TargetDir (and -SigningDir), apart from pip's and cargo's
  download caches and, unless -SkipChecks, what the checks of step 2 write like any test run
  (cargo's target folder, caches in ui\, temporary folders, the registry sandbox
  HKCU\Software\PCOptimizer\SelfTest). It never installs anything on this PC, never runs the
  setup and never asks for administrator rights. The staged folder is writable by this user,
  so the staged Cairn.exe never elevates; use it for checks only.
.PARAMETER SkipChecks
  Skips step 2.
.PARAMETER StageOnly
  Runs steps 1-6.
.PARAMETER PackageOnly
  Runs step 7 on the stage of an earlier -StageOnly run of the same version.
.PARAMETER FinalizeOnly
  Runs step 8 on the setup of an earlier -PackageOnly run.
.PARAMETER SigningDir
  With -StageOnly: copies Cairn's own programs and libraries (the engine as
  optimizer_engine.dll) to <dir>\binaries, then compiles the setup once in Inno Setup's
  signed-uninstaller mode, which leaves the uninstaller to sign in <dir>\uninstaller-cache; it
  is copied to <dir>\binaries\uninstaller\uninst.exe. With -PackageOnly: where that cache is.
.PARAMETER SignedDir
  With -PackageOnly: the signed files, laid out as <SigningDir>\binaries.
.PARAMETER SignedSetup
  With -FinalizeOnly: the signed setup.
.PARAMETER SigningMode
  none (default), test or release; test and release go with -SignedDir or -SignedSetup. test
  accepts a signature by the certificate of -TestSignerThumbprint that Windows does not trust
  and needs no timestamp; release requires a valid signature by SignPath Foundation with a
  timestamp. In both, every other program file of the stage must carry a valid signature of
  its publisher, apart from Pillow's extension modules, which their authors do not sign.
.PARAMETER TestSignerThumbprint
  With -SigningMode test: the thumbprint of the test certificate (40 hex digits).
.PARAMETER Python
  The python.exe of a Python 3.12 whose runtime the stage copies; default: `py -3.12`.
.PARAMETER Generator
  The CMake generator; default: CMake's own choice, the newest installed Visual Studio.
.PARAMETER AllowPending
  For release candidates: the Rust tests run without CAIRN_RELEASE=1, so tweaks that still
  lack a verification record or wait for a new check do not fail the build, and
  build-info.json says "release candidate".
.PARAMETER Attested
  With a run that writes the release files (no -StageOnly or -PackageOnly): the setup gets a
  build-provenance attestation, which the release workflow makes on a version tag, so the
  release notes show `gh attestation verify`, which checks it. A build made anywhere else has
  no attestation.
.PARAMETER ImmutableReleases
  With a run that writes the release files (no -StageOnly or -PackageOnly): the repository
  publishes its releases as immutable, so GitHub attests each release it publishes, and the
  release notes also show `gh release verify`, which checks that attestation.
.PARAMETER IsccPath
  Inno Setup's ISCC.exe; default: the ISCC environment variable, else the per-machine or
  per-user Inno Setup 6 folder.
.PARAMETER TargetDir
  Cargo's target folder for the release build (default target\dist).
.EXAMPLE
  .\scripts\build_release.ps1
.EXAMPLE
  .\scripts\build_release.ps1 -StageOnly -SkipChecks
.EXAMPLE
  .\scripts\build_release.ps1 -AllowPending
#>
[CmdletBinding()]
param(
    [switch]$SkipChecks,
    [switch]$StageOnly,
    [switch]$PackageOnly,
    [switch]$FinalizeOnly,
    [string]$SigningDir,
    [string]$SignedDir,
    [string]$SignedSetup,
    [ValidateSet('none', 'test', 'release')]
    [string]$SigningMode = 'none',
    [string]$TestSignerThumbprint,
    [string]$Python,
    [string]$Generator,
    [switch]$AllowPending,
    [switch]$Attested,
    [switch]$ImmutableReleases,
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
# The packages of the Rust workspace whose versions Cargo.lock must record.
$WorkspacePackages = @('cairn', 'optctl', 'optimizer_core', 'optimizer_engine')
# Cairn's own programs and libraries in the stage, and their names in the upload to sign
# (SignPath signs .exe and .dll files; the engine goes up under its cargo name).
$OwnFiles = [ordered]@{
    'Cairn.exe' = 'Cairn.exe'
    'optctl.exe' = 'optctl.exe'
    'cairn-maintenance.exe' = 'cairn-maintenance.exe'
    'app\optimizer\native\optimizer_engine.pyd' = 'app\optimizer\native\optimizer_engine.dll'
    'app\optimizer\native\optimizer_telemetry.dll' = 'app\optimizer\native\optimizer_telemetry.dll'
}
# The uninstaller of the setup in the upload to sign.
$UninstallerUpload = 'uninstaller\uninst.exe'
# Pillow's extension modules: the app needs only the kept ones, and the stage leaves the others
# out. Pillow imports _imagingft (for ImageFont, which ImageDraw imports), _imagingcms, _webp and
# _avif inside try blocks, and _imagingmath and _imagingmorph only for ImageMath and ImageMorph,
# which the app does not use.
$PillowKept = @('_imaging', '_imagingtk')
$PillowUnused = @('_avif', '_imagingcms', '_imagingft', '_imagingmath', '_imagingmorph', '_webp')
# Program files of the stage that may stay unsigned in a signed build: Pillow's extension
# modules, which their authors do not sign.
$UnsignedAllowed = @('app\site\PIL\*.pyd')
# A test-signing certificate is self-signed: Windows reports its signatures as untrusted.
$TestSignatureStatuses = @('Valid', 'UnknownError', 'NotTrusted')
$ReleaseSigner = 'SignPath Foundation'
$IsccMinimum = [version]'6.3.0'
# What ISCC says when it stops for the signature of the uninstaller it wrote.
$SignedUninstallerMessage = 'Signed uninstaller mode is enabled'
$RustNotices = 'Rust-THIRD-PARTY.txt'
$ThirdPartyNotices = 'THIRD-PARTY-NOTICES.md'
$RepositoryUrl = 'https://github.com/Dray973/Cairn'
$Repository = 'Dray973/Cairn'

# Runs the brand renderer, the icon file and the window's Tk photos with the staged packages in
# the build's Python 3.12 (isolated, without site-packages) and prints the Pillow extension
# modules that loaded, as JSON.
$PillowTrace = @'
import json
import sys

stage = sys.argv[1]
sys.path[0:0] = [stage + "\\app", stage + "\\app\\site"]

import tkinter  # noqa: E402

from optimizer.widgets import brand  # noqa: E402

pil = brand.pillow()
if pil is None:
    raise SystemExit("Pillow cannot be loaded from the stage")
renderer = brand.CairnRenderer(brand.mark_size(1.0))
first = renderer.frame(0.0)
later = renderer.frame(0.5)
brand.icon_images()
icons = brand.window_icon_images()
if len(icons) != len(brand.ICON_SIZES):
    raise SystemExit("the icon file could not be read")
root = tkinter.Tk()
try:
    root.withdraw()
    photo = pil.ImageTk.PhotoImage(first, master=root)
    photo.paste(later)
    root.wm_iconphoto(True, *[pil.ImageTk.PhotoImage(image, master=root) for image in icons])
finally:
    root.destroy()
loaded = sorted(
    name.rsplit(".", 1)[-1]
    for name, module in list(sys.modules.items())
    if name.startswith("PIL._") and str(getattr(module, "__file__", "") or "").lower().endswith(".pyd")
)
print(json.dumps({"loaded": loaded}))
'@

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

# Runs a native command and returns its exit code and its output (stdout and stderr) as lines.
function Invoke-Capture([scriptblock]$Command) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $global:LASTEXITCODE = 0
    try {
        $captured = @(& $Command 2>&1 | ForEach-Object { "$_" })
        $exit = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
        # The caller reads the code from the result. Left in $LASTEXITCODE, it would become the
        # exit code of a GitHub Actions step that runs this script (`exit $LASTEXITCODE`).
        $global:LASTEXITCODE = 0
    }
    return [pscustomobject]@{ Code = $exit; Lines = $captured }
}

# A full path for a path given relative to PowerShell's current location.
function Resolve-FullPath([string]$Path) {
    if (-not $Path) { return $Path }
    return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
}

function Write-TextFile([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
}

function Get-TomlVersion([string]$Text, [string]$Section) {
    $pattern = '(?ms)^\[' + [regex]::Escape($Section) + '\]\s*$(.*?)(?=^\[|\z)'
    $body = [regex]::Match($Text, $pattern)
    if (-not $body.Success) { return $null }
    $version = [regex]::Match($body.Groups[1].Value, '(?m)^version\s*=\s*"([^"]+)"')
    if ($version.Success) { return $version.Groups[1].Value }
    return $null
}

# The versions Cargo.lock records for the workspace's own packages (those without a source),
# by package name.
function Get-WorkspaceLockVersions([string]$Text) {
    $found = [ordered]@{}
    foreach ($block in [regex]::Split($Text, '(?m)^\[\[package\]\][ \t]*\r?$')) {
        $name = [regex]::Match($block, '(?m)^name = "([^"]+)"')
        $version = [regex]::Match($block, '(?m)^version = "([^"]+)"')
        if (-not $name.Success -or -not $version.Success) { continue }
        if ([regex]::IsMatch($block, '(?m)^source = ')) { continue }
        $found[$name.Groups[1].Value] = $version.Groups[1].Value
    }
    return $found
}

# The release version; fails when the places that carry it disagree: Cargo.toml's
# [workspace.package], every workspace package in Cargo.lock, ui\pyproject.toml and
# ui\optimizer\__init__.py, and a version written into CMakeLists.txt or telemetry.cpp, which
# take theirs from Cargo.toml.
function Get-CairnVersion([string]$RepoRoot) {
    $cargo = Get-TomlVersion (Get-Content -Raw (Join-Path $RepoRoot 'Cargo.toml')) 'workspace.package'
    if (-not $cargo) { throw 'Cargo.toml has no [workspace.package] version.' }
    $locked = Get-WorkspaceLockVersions (Get-Content -Raw (Join-Path $RepoRoot 'Cargo.lock'))
    foreach ($name in $WorkspacePackages) {
        if (-not $locked.Contains($name)) {
            throw "Cargo.lock has no version of $name; run cargo check --workspace and keep the updated Cargo.lock."
        }
    }
    $found = [ordered]@{}
    foreach ($name in $locked.Keys) { $found["Cargo.lock ($name)"] = $locked[$name] }
    $found['ui\pyproject.toml'] = Get-TomlVersion (Get-Content -Raw (Join-Path $RepoRoot 'ui\pyproject.toml')) 'project'
    $init = [regex]::Match((Get-Content -Raw (Join-Path $RepoRoot 'ui\optimizer\__init__.py')),
        '(?m)^__version__\s*=\s*"([^"]+)"')
    if (-not $init.Success) { throw 'ui\optimizer\__init__.py has no __version__.' }
    $found['ui\optimizer\__init__.py'] = $init.Groups[1].Value
    $cmake = [regex]::Match((Get-Content -Raw (Join-Path $RepoRoot 'CMakeLists.txt')),
        '(?m)^project\([^)]*?\bVERSION\s+(\d+\.\d+\.\d+)')
    if ($cmake.Success) { $found['CMakeLists.txt'] = $cmake.Groups[1].Value }
    $cpp = [regex]::Match((Get-Content -Raw (Join-Path $RepoRoot 'telemetry\src\telemetry.cpp')),
        'kVersionString\s*=\s*"optimizer_telemetry (\d+\.\d+\.\d+)"')
    if ($cpp.Success) { $found['telemetry\src\telemetry.cpp'] = $cpp.Groups[1].Value }
    $differ = @($found.Keys | Where-Object { $found[$_] -ne $cargo })
    if ($differ.Count) {
        $list = ($differ | ForEach-Object { "$_ $($found[$_])" }) -join ', '
        throw "Versions differ: Cargo.toml $cargo, $list."
    }
    return $cargo
}

# Why a combination of parameters cannot run; empty when it can.
function Test-BuildArguments([hashtable]$Options) {
    $problems = New-Object System.Collections.Generic.List[string]
    $modes = @(@('StageOnly', 'PackageOnly', 'FinalizeOnly') | Where-Object { $Options[$_] })
    if ($modes.Count -gt 1) {
        $problems.Add("Use one of -StageOnly, -PackageOnly and -FinalizeOnly, not $($modes -join ' and ').")
    }
    $mode = if ($Options.SigningMode) { [string]$Options.SigningMode } else { 'none' }
    if (@('none', 'test', 'release') -notcontains $mode) {
        $problems.Add("-SigningMode is none, test or release, not $mode.")
    }
    if ($Options.SigningDir -and -not ($Options.StageOnly -or $Options.PackageOnly)) {
        $problems.Add('-SigningDir goes with -StageOnly or -PackageOnly.')
    }
    if ($Options.SignedDir) {
        if (-not $Options.PackageOnly) { $problems.Add('-SignedDir goes with -PackageOnly.') }
        if (-not $Options.SigningDir) { $problems.Add('-SignedDir needs -SigningDir, where the uninstaller to sign is.') }
    }
    if ($Options.SignedSetup -and -not $Options.FinalizeOnly) { $problems.Add('-SignedSetup goes with -FinalizeOnly.') }
    if ($mode -eq 'none') {
        if ($Options.SignedDir -or $Options.SignedSetup) { $problems.Add('Signed files need -SigningMode test or release.') }
    } elseif ($Options.PackageOnly) {
        if (-not $Options.SignedDir) { $problems.Add("-SigningMode $mode with -PackageOnly needs -SignedDir.") }
    } elseif ($Options.FinalizeOnly) {
        if (-not $Options.SignedSetup) { $problems.Add("-SigningMode $mode with -FinalizeOnly needs -SignedSetup.") }
    } else {
        $problems.Add("-SigningMode $mode goes with -PackageOnly or -FinalizeOnly; the other steps sign nothing.")
    }
    $thumbprint = [string]$Options.TestSignerThumbprint
    if ($mode -eq 'test' -and $thumbprint -notmatch '^[0-9A-Fa-f]{40}$') {
        $problems.Add('-SigningMode test needs -TestSignerThumbprint, the 40 hex digits of the test certificate.')
    }
    if ($thumbprint -and $mode -ne 'test') { $problems.Add('-TestSignerThumbprint goes with -SigningMode test.') }
    if ($Options.AllowPending -and ($Options.PackageOnly -or $Options.FinalizeOnly)) {
        $problems.Add('-AllowPending goes with a run that makes the stage (no -PackageOnly or -FinalizeOnly).')
    }
    if ($Options.Attested -and ($Options.StageOnly -or $Options.PackageOnly)) {
        $problems.Add('-Attested goes with a run that writes the release files (no -StageOnly or -PackageOnly).')
    }
    if ($Options.ImmutableReleases -and ($Options.StageOnly -or $Options.PackageOnly)) {
        $problems.Add('-ImmutableReleases goes with a run that writes the release files (no -StageOnly or -PackageOnly).')
    }
    return $problems.ToArray()
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

# The CPython 3.12 runtime at $Prefix without its tests, tools and site-packages.
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
    $installs = @(& $vswhere -products * -sort -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
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

# The notices of the Rust crates the programs are built from (scripts\third_party_notices.py)
# and THIRD-PARTY-NOTICES.md, under <stage>\licenses.
function Add-Notices([string]$RepoRoot, [string]$Stage, [string]$Interpreter) {
    $licenses = Join-Path $Stage 'licenses'
    $rust = Join-Path $licenses $RustNotices
    $noticesScript = Join-Path $RepoRoot 'scripts\third_party_notices.py'
    $manifest = Join-Path $RepoRoot 'Cargo.toml'
    Invoke-Native 'write the notices of the Rust crates' { & $Interpreter $noticesScript --out $rust --manifest-path $manifest }
    $notices = Join-Path $RepoRoot $ThirdPartyNotices
    if (-not (Test-Path -LiteralPath $notices)) { throw "$ThirdPartyNotices is missing; the stage ships it with the licenses." }
    Copy-Item -LiteralPath $notices (Join-Path $licenses $ThirdPartyNotices)
}

# Problems of the notices in the stage; empty when they are there.
function Find-NoticeProblems([string]$Stage) {
    $problems = @()
    $rust = Join-Path $Stage "licenses\$RustNotices"
    if (-not (Test-Path -LiteralPath $rust) -or (Get-Item -LiteralPath $rust).Length -eq 0) {
        $problems += "licenses\$RustNotices is missing or empty"
    } elseif (-not ([string](Get-Content -LiteralPath $rust -TotalCount 1)).StartsWith('Third-party notices')) {
        $problems += "licenses\$RustNotices is not the notices file"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $Stage "licenses\$ThirdPartyNotices"))) {
        $problems += "licenses\$ThirdPartyNotices is missing"
    }
    return $problems
}

# Deletes the Pillow extension modules of $PillowUnused from <site>\PIL; their names.
function Remove-UnusedPillowModules([string]$Site) {
    $pil = Join-Path $Site 'PIL'
    $removed = @()
    foreach ($name in $PillowUnused) {
        foreach ($file in @(Get-ChildItem -LiteralPath $pil -File -Filter "$name.*.pyd" -ErrorAction SilentlyContinue)) {
            Remove-Item -LiteralPath $file.FullName
            $removed += $file.Name
        }
    }
    return $removed
}

# Runs $PillowTrace on the stage; the Pillow extension modules that loaded. Fails unless the
# renderer worked and loaded exactly the kept modules.
function Test-PillowModules([string]$Stage, [string]$Interpreter, [string]$Work) {
    New-Item -ItemType Directory -Force $Work | Out-Null
    $trace = Join-Path $Work 'pillow-trace.py'
    Write-TextFile $trace $PillowTrace
    $result = Invoke-Capture { & $Interpreter -I -S $trace $Stage }
    $json = @($result.Lines | Where-Object { $_.StartsWith('{') }) | Select-Object -Last 1
    if ($result.Code -ne 0 -or -not $json) {
        throw ("The brand renderer failed with the staged Pillow (exit code $($result.Code)):`n  " + ($result.Lines -join "`n  "))
    }
    $loaded = @((ConvertFrom-Json $json).loaded | Sort-Object)
    $expected = @($PillowKept | Sort-Object)
    if (($loaded -join ',') -ne ($expected -join ',')) {
        throw "The brand renderer loaded the Pillow modules $($loaded -join ', '); the stage keeps $($expected -join ', ')."
    }
    return $loaded
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

# The version resources of Cairn's own programs and libraries in the stage (Path, Info).
function Get-OwnFileVersions([string]$Stage) {
    foreach ($relative in $OwnFiles.Keys) {
        $path = Join-Path $Stage $relative
        $info = $null
        if (Test-Path -LiteralPath $path) { $info = (Get-Item -LiteralPath $path).VersionInfo }
        [pscustomobject]@{ Path = $relative; Info = $info }
    }
}

# Problems of the version resources of $Files (Path, Info): each must name the product Cairn
# and carry $Version, as text and as numbers.
function Find-VersionResourceProblems([object[]]$Files, [string]$Version) {
    $problems = @()
    foreach ($file in $Files) {
        $info = $file.Info
        if (-not $info) { $problems += "$($file.Path) is missing"; continue }
        if ($info.ProductName -ne 'Cairn') {
            $problems += "$($file.Path) has no version resource that names the product Cairn"
            continue
        }
        if ($info.ProductVersion -ne $Version) {
            $problems += "$($file.Path) has the product version '$($info.ProductVersion)', not $Version"
        }
        $numbers = '{0}.{1}.{2}' -f $info.FileMajorPart, $info.FileMinorPart, $info.FileBuildPart
        if ($numbers -ne $Version) { $problems += "$($file.Path) has the file version $numbers, not $Version" }
    }
    return $problems
}

# Runs the staged launcher (a windowed program) in this console and returns its exit code.
function Invoke-Staged([string]$Program, [string[]]$Arguments) {
    $process = Start-Process -FilePath $Program -ArgumentList $Arguments -NoNewWindow -PassThru
    # Reading the handle first keeps the exit code available after the process ends.
    $null = $process.Handle
    $process.WaitForExit()
    return $process.ExitCode
}

# The checks of the stage (step 6); the Pillow modules the renderer loaded.
function Invoke-StageGates([string]$Stage, [string]$Dist, [string]$Version, [string]$Interpreter, [string]$Work) {
    $launcher = Join-Path $Stage 'Cairn.exe'
    Write-Step 'Cairn.exe --check'
    $code = Invoke-Staged $launcher @('--check')
    if ($code -ne 0) { throw "Cairn.exe --check failed with exit code $code." }
    $report = Join-Path $Dist 'selftest.json'
    if (Test-Path $report) { Remove-Item -Force $report }
    Write-Step 'Cairn.exe --self-test'
    # Tcl variables the launcher must remove: the self-test reports any that reaches the app.
    $removed = @{}
    foreach ($name in $LauncherRemovesTcl) {
        $removed[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, (Join-Path $Dist 'no-such-folder'), 'Process')
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

    Write-Step 'brand renderer with the staged Pillow'
    $loaded = Test-PillowModules $Stage $Interpreter $Work
    Write-Host "Pillow modules loaded: $($loaded -join ', ')"
    Write-Step 'version resources, notices and hygiene scan'
    $problems = @(Find-VersionResourceProblems @(Get-OwnFileVersions $Stage) $Version)
    $problems += @(Find-NoticeProblems $Stage)
    $problems += @(Find-HygieneProblems $Stage @($Root, $env:USERPROFILE))
    if ($problems.Count) { throw ("The stage is not clean:`n  " + ($problems -join "`n  ")) }
    return $loaded
}

# Every program file of the stage with its Authenticode signature (Path, Signature).
function Get-StageSignatures([string]$Stage) {
    $prefix = $Stage.TrimEnd('\').Length + 1
    foreach ($file in Get-ChildItem $Stage -Recurse -File -Include *.exe, *.dll, *.pyd) {
        [pscustomobject]@{ Path = $file.FullName.Substring($prefix); Signature = Get-AuthenticodeSignature -LiteralPath $file.FullName }
    }
}

function Write-UnsignedList([string]$Stage) {
    $unsigned = @(Get-StageSignatures $Stage | Where-Object { [string]$_.Signature.Status -ne 'Valid' })
    Write-Host ("{0} unsigned program files (SmartScreen and Smart App Control treat them as unknown):" -f $unsigned.Count)
    foreach ($file in $unsigned) { Write-Host ('  ' + $file.Path) }
}

# What is wrong with one signature in -SigningMode test or release; $null when it is accepted.
# Cairn's own files ($Own) carry the signature of the mode; every other program file keeps a
# valid signature of its publisher.
function Get-SignatureProblem([object]$Signature, [string]$Mode, [string]$TestThumbprint, [bool]$Own) {
    $status = [string]$Signature.Status
    if (-not $Own) {
        if ($status -ne 'Valid') { return "has no valid signature ($status)" }
        return $null
    }
    $signer = $Signature.SignerCertificate
    if (-not $signer) { return "is not signed ($status)" }
    if ($Mode -eq 'test') {
        if ([string]$signer.Thumbprint -ne ($TestThumbprint -replace '\s', '')) {
            return "is signed by $($signer.Subject), not by the test certificate"
        }
        if ($TestSignatureStatuses -notcontains $status) { return "has a broken test signature ($status)" }
        return $null
    }
    if ($status -ne 'Valid') { return "has no valid signature ($status)" }
    if ([string]$signer.Subject -notlike "*$ReleaseSigner*") { return "is signed by $($signer.Subject), not by $ReleaseSigner" }
    if (-not $Signature.TimeStamperCertificate) { return 'has a signature without a timestamp' }
    return $null
}

# Problems of the signatures of $Files (Path, Signature) in -SigningMode test or release; the
# files of $UnsignedAllowed are left out, $Own are Cairn's own.
function Find-SignatureProblems([object[]]$Files, [string]$Mode, [string]$TestThumbprint, [string[]]$Own) {
    $problems = @()
    foreach ($file in $Files) {
        $path = [string]$file.Path
        if (@($UnsignedAllowed | Where-Object { $path -like $_ }).Count) { continue }
        $problem = Get-SignatureProblem $file.Signature $Mode $TestThumbprint ($Own -contains $path)
        if ($problem) { $problems += "$path $problem" }
    }
    return $problems
}

# Copies Cairn's own programs and libraries from the stage into $Binaries, under their upload names.
function Export-FilesToSign([string]$Stage, [string]$Binaries) {
    foreach ($relative in $OwnFiles.Keys) {
        $target = Join-Path $Binaries $OwnFiles[$relative]
        New-Item -ItemType Directory -Force (Split-Path -Parent $target) | Out-Null
        Copy-Item -LiteralPath (Join-Path $Stage $relative) $target
    }
}

# The name of the one uninstaller (uninst-*.e32) in $After and not in $Before; fails unless
# there is exactly one.
function Find-NewUninstallerStub([string[]]$Before, [string[]]$After) {
    $new = @($After | Where-Object { $_ -like 'uninst-*.e32' -and $Before -notcontains $_ })
    if ($new.Count -ne 1) {
        $names = if ($new.Count) { ': ' + ($new -join ', ') } else { '' }
        throw "Expected one new uninstaller to sign (uninst-*.e32), found $($new.Count)$names."
    }
    return $new[0]
}

# Compiles the setup in Inno Setup's signed-uninstaller mode without a sign tool: ISCC writes
# the uninstaller it would embed into $Cache and stops to ask for its signature. Returns the
# uninstaller's path.
function Invoke-UninstallerProbe([string]$Iscc, [string]$Version, [string]$Stage, [string]$Cache, [string]$Output, [string]$IssFile) {
    New-Item -ItemType Directory -Force $Cache, $Output | Out-Null
    $before = @(Get-ChildItem -LiteralPath $Cache -File | ForEach-Object { $_.Name })
    Write-Step 'compile the setup once for the uninstaller to sign'
    $result = Invoke-Capture { & $Iscc "/DAppVersion=$Version" "/DStageDir=$Stage" "/DOutputDir=$Output" "/DSignedUninstallerDir=$Cache" $IssFile }
    if ($result.Code -eq 0 -or ($result.Lines -join "`n").IndexOf($SignedUninstallerMessage) -lt 0) {
        throw ("ISCC was expected to stop for the uninstaller's signature (exit code $($result.Code)):`n  " + ($result.Lines -join "`n  "))
    }
    $after = @(Get-ChildItem -LiteralPath $Cache -File | ForEach-Object { $_.Name })
    return Join-Path $Cache (Find-NewUninstallerStub $before $after)
}

# Puts the signed files back: Cairn's programs and libraries into the stage (the engine as
# .pyd again) and the uninstaller over the one in $Cache. Returns the uninstaller's path.
function Import-SignedFiles([string]$SignedFiles, [string]$Stage, [string]$Cache) {
    foreach ($relative in $OwnFiles.Keys) {
        $signed = Join-Path $SignedFiles $OwnFiles[$relative]
        if (-not (Test-Path -LiteralPath $signed)) { throw "The signed files have no $($OwnFiles[$relative])." }
        Copy-Item -LiteralPath $signed (Join-Path $Stage $relative) -Force
    }
    $stubs = @(Get-ChildItem -LiteralPath $Cache -File -Filter 'uninst-*.e32' -ErrorAction SilentlyContinue)
    if ($stubs.Count -ne 1) {
        throw "Expected one uninstaller (uninst-*.e32) in $Cache, found $($stubs.Count); run -StageOnly with -SigningDir first."
    }
    $signedStub = Join-Path $SignedFiles $UninstallerUpload
    if (-not (Test-Path -LiteralPath $signedStub)) { throw "The signed files have no $UninstallerUpload." }
    Copy-Item -LiteralPath $signedStub $stubs[0].FullName -Force
    return $stubs[0].FullName
}

function Find-Iscc([string]$Explicit, [string]$FromEnvironment) {
    foreach ($named in @($Explicit, $FromEnvironment)) {
        if (-not $named) { continue }
        if (Test-Path -LiteralPath $named -PathType Leaf) { return (Resolve-Path -LiteralPath $named).Path }
        throw "ISCC.exe not found at $named."
    }
    foreach ($candidate in @(
            (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
            (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
            (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'))) {
        if (Test-Path $candidate) { return $candidate }
    }
    return $null
}

# The Inno Setup version in ISCC's output ("Compiler engine version: Inno Setup 6.7.3"); $null
# when the line is missing.
function ConvertFrom-IsccOutput([string[]]$Lines) {
    foreach ($line in $Lines) {
        $match = [regex]::Match([string]$line, 'Compiler engine version:\s*Inno Setup\s+(\d+\.\d+\.\d+)')
        if ($match.Success) { return [version]$match.Groups[1].Value }
    }
    return $null
}

# ISCC's version: a script read from standard input that only stops the compile makes it print
# its version line and write nothing. Fails below Inno Setup 6.3.
function Assert-IsccVersion([string]$Iscc) {
    $query = "#error version query`r`n"
    $result = Invoke-Capture { $query | & $Iscc '/O-' '-' }
    $version = ConvertFrom-IsccOutput $result.Lines
    if (-not $version) { throw "$Iscc did not report its version (exit code $($result.Code))." }
    Write-Host "Inno Setup $version ($Iscc)"
    if ($version -lt $IsccMinimum) { throw "Inno Setup $version is too old; the setup needs Inno Setup $IsccMinimum or later." }
    return $version
}

function Find-RequiredIscc([string]$Explicit) {
    $iscc = Find-Iscc $Explicit $env:ISCC
    if (-not $iscc) {
        throw 'Inno Setup 6.3 or later is required (winget install JRSoftware.InnoSetup, or set ISCC to its ISCC.exe).'
    }
    return $iscc
}

# The Python 3.12 whose runtime the stage copies: Executable, Prefix (its base prefix) and Version.
function Resolve-Python([string]$Explicit) {
    if ($Explicit) {
        if (-not (Test-Path -LiteralPath $Explicit -PathType Leaf)) { throw "Python was not found at $Explicit." }
        $program = $Explicit
        $first = @()
    } else {
        $program = 'py'
        $first = @('-3.12')
    }
    # No double quotes inside the code: Windows PowerShell drops them when it starts a program.
    $code = 'import sys; print(sys.executable); print(sys.base_prefix); print(*sys.version_info[:3], sep=chr(46))'
    $result = Invoke-Capture { & $program @first -c $code }
    $lines = @($result.Lines | Where-Object { $_.Trim() } | ForEach-Object { $_.Trim() })
    if ($result.Code -ne 0 -or $lines.Count -lt 3) {
        throw "Python 3.12 was not found ($program gave exit code $($result.Code)); install it or pass -Python."
    }
    $found = [pscustomobject]@{ Executable = $lines[0]; Prefix = $lines[1]; Version = $lines[2] }
    if (-not $found.Version.StartsWith('3.12.')) {
        throw "The stage needs Python 3.12; $($found.Executable) is Python $($found.Version)."
    }
    if (-not (Test-Path (Join-Path $found.Prefix 'python312.dll'))) {
        throw "The Python runtime in $($found.Prefix) has no python312.dll."
    }
    return $found
}

# The generator and platform arguments of the CMake configure: -G only when a generator is
# named; -A x64 for the Visual Studio generators, CMake's default on Windows (the newest
# installed Visual Studio); a single-configuration generator gets the Release build type.
function Get-CMakeGeneratorArguments([string]$Generator, [string]$EnvironmentGenerator) {
    $arguments = @()
    if ($Generator) { $arguments += @('-G', $Generator) }
    $effective = if ($Generator) { $Generator } else { $EnvironmentGenerator }
    if (-not $effective -or $effective -like 'Visual Studio*') {
        $arguments += @('-A', 'x64')
    } else {
        $arguments += '-DCMAKE_BUILD_TYPE=Release'
    }
    return $arguments
}

# A value of CMakeCache.txt ("NAME:TYPE=value"); $null when it is not there.
function Get-CMakeCacheValue([string]$CacheFile, [string]$Name) {
    if (-not (Test-Path -LiteralPath $CacheFile)) { return $null }
    $pattern = '^' + [regex]::Escape($Name) + ':[A-Z]+=(.*)$'
    foreach ($line in Get-Content -LiteralPath $CacheFile) {
        $match = [regex]::Match($line, $pattern)
        if ($match.Success) { return $match.Groups[1].Value }
    }
    return $null
}

# The compiler version and the Windows SDK of a CMake build folder; $null where unknown.
function Get-MsvcToolchain([string]$BuildDir) {
    $msvc = $null
    $sdk = $null
    $compiler = Get-ChildItem -LiteralPath (Join-Path $BuildDir 'CMakeFiles') -Recurse -Filter 'CMakeCXXCompiler.cmake' -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($compiler) {
        $match = [regex]::Match((Get-Content -Raw -LiteralPath $compiler.FullName), 'set\(CMAKE_CXX_COMPILER_VERSION "([^"]+)"\)')
        if ($match.Success) { $msvc = $match.Groups[1].Value }
    }
    $project = Get-ChildItem -LiteralPath $BuildDir -Recurse -Filter 'optimizer_telemetry.vcxproj' -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($project) {
        $match = [regex]::Match((Get-Content -Raw -LiteralPath $project.FullName), '<WindowsTargetPlatformVersion>([^<]+)<')
        if ($match.Success) { $sdk = $match.Groups[1].Value }
    }
    return [pscustomobject]@{ Msvc = $msvc; WindowsSdk = $sdk }
}

# The first line a tool prints for its version; $null when it cannot run.
function Get-ToolVersion([scriptblock]$Command) {
    $result = Invoke-Capture $Command
    $line = @($result.Lines | Where-Object { $_.Trim() }) | Select-Object -First 1
    if ($result.Code -ne 0 -or -not $line) { return $null }
    return $line.Trim()
}

function Invoke-TelemetryBuild([string]$RepoRoot, [string]$BuildDir, [string]$Generator) {
    $cmake = Get-Command cmake -ErrorAction SilentlyContinue
    if (-not $cmake) { throw 'cmake was not found on PATH.' }
    # A cache that names another generator or Visual Studio makes CMake refuse to configure.
    if (Test-Path -LiteralPath $BuildDir) { Remove-Item -LiteralPath $BuildDir -Recurse -Force }
    $arguments = @('-S', $RepoRoot, '-B', $BuildDir) + @(Get-CMakeGeneratorArguments $Generator $env:CMAKE_GENERATOR) +
        @('-DTEL_AVX2=OFF', '-DTEL_BUILD_TESTS=OFF')
    Invoke-Native 'configure the telemetry DLL' { & $cmake.Source @arguments }
    $chosen = Get-CMakeCacheValue (Join-Path $BuildDir 'CMakeCache.txt') 'CMAKE_GENERATOR'
    Write-Host "CMake generator: $chosen"
    Invoke-Native 'build the telemetry DLL' { & $cmake.Source --build $BuildDir --config Release --parallel }
    $dll = @('bin\Release\optimizer_telemetry.dll', 'bin\optimizer_telemetry.dll') | ForEach-Object { Join-Path $BuildDir $_ } |
        Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $dll) { throw "The telemetry DLL was not built in $BuildDir." }
    $toolchain = Get-MsvcToolchain $BuildDir
    return [pscustomobject]@{
        Dll = $dll
        Pdb = [System.IO.Path]::ChangeExtension($dll, '.pdb')
        Generator = $chosen
        CMake = Get-ToolVersion { & $cmake.Source --version }
        Msvc = $toolchain.Msvc
        WindowsSdk = $toolchain.WindowsSdk
    }
}

function Invoke-RustBuild([string]$RepoRoot, [string]$Target, [string]$Interpreter) {
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
    # CARGO_ENCODED_RUSTFLAGS replaces .cargo\config.toml's x86-64-v3 and keeps paths with
    # spaces in one flag each.
    $flags = @('-C', 'target-cpu=x86-64-v2', "--remap-path-prefix=$cargoHome=cargo",
        "--remap-path-prefix=$rustupHome=rustup", "--remap-path-prefix=$RepoRoot=cairn")
    $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f
    $env:PYO3_PYTHON = $Interpreter
    # A program or library without its version resource cannot carry the product name and
    # version a signature is checked against.
    $env:CAIRN_REQUIRE_RESOURCES = '1'
    Push-Location $RepoRoot
    try {
        Invoke-Native 'build Cairn.exe, optctl.exe, cairn-maintenance.exe and the engine' {
            cargo build --release --locked -p optimizer_engine -p optctl -p cairn --target-dir $Target
        }
    } finally {
        Pop-Location
    }
}

# True when this process runs with administrator rights (an elevated token).
function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Invoke-Checks([string]$RepoRoot, [bool]$AllowPending) {
    $venv = Join-Path $RepoRoot '.venv\Scripts\python.exe'
    $env:PYO3_PYTHON = $venv
    Push-Location $RepoRoot
    try {
        Invoke-Native 'rustfmt check' { cargo fmt --all -- --check }
        Invoke-Native 'clippy' { cargo clippy --workspace --all-targets -- -D warnings }
        Invoke-Native 'Rust 1.80 check' { cargo +1.80 check --workspace --all-targets }
        # Every shipped tweak needs its verification record (crates/core/tests/verification_records.rs);
        # a release candidate may still lack some.
        if ($AllowPending) { $env:CAIRN_RELEASE = $null } else { $env:CAIRN_RELEASE = '1' }
        try {
            Invoke-Native 'Rust tests' { cargo test --workspace }
        } finally {
            $env:CAIRN_RELEASE = $null
        }
        # `cargo test --workspace` leaves out the tests inside optctl.exe: its requireAdministrator
        # manifest also applies to their test program, which only an elevated process can start.
        if (Test-Elevated) {
            Invoke-Native 'optctl tests' { cargo test -p optctl --bin optctl }
        } else {
            Write-Warning 'The tests inside optctl.exe were skipped: they run only from an elevated shell.'
        }
    } finally {
        Pop-Location
    }
    Push-Location (Join-Path $RepoRoot 'ui')
    try {
        Invoke-Native 'ruff' { & $venv -m ruff check . }
    } finally {
        Pop-Location
    }
    # A separate PowerShell: the test script ends with `exit`.
    $tests = Join-Path $RepoRoot 'scripts\pytest_serial.ps1'
    Invoke-Native 'UI tests' { & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $tests tests -q }
}

# The stage (step 5).
function New-Stage([string]$RepoRoot, [hashtable]$Paths, [object]$Runtime, [string]$Telemetry) {
    $stage = $Paths.Stage
    $release = Join-Path $Paths.Target 'release'
    Write-Step "stage $stage"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Runtime $Runtime.Prefix $stage
    $crt = Find-VcRuntime
    if (-not $crt) { throw 'The x64 VC runtime of Visual Studio 2022 or later (VC\Redist\MSVC) was not found.' }
    Copy-Item (Join-Path $crt 'vcruntime140.dll'), (Join-Path $crt 'vcruntime140_1.dll') $stage
    $app = Join-Path $stage 'app\optimizer'
    $ui = Join-Path $RepoRoot 'ui\optimizer'
    Copy-Tree $ui $app -ExcludeDirs @('__pycache__', (Join-Path $ui 'native')) -ExcludeFiles @('*.pyc')
    $native = Join-Path $app 'native'
    New-Item -ItemType Directory -Force $native | Out-Null
    Copy-Item (Join-Path $release 'optimizer_engine.dll') (Join-Path $native 'optimizer_engine.pyd')
    Copy-Item $Telemetry (Join-Path $native 'optimizer_telemetry.dll')
    $site = Join-Path $stage 'app\site'
    $interpreter = $Runtime.Executable
    Invoke-Native 'install the runtime packages' {
        & $interpreter -m pip install --isolated --require-hashes --no-deps --only-binary=:all: --no-compile `
            --disable-pip-version-check --no-warn-script-location --target $site `
            -r (Join-Path $RepoRoot 'requirements-runtime.txt')
    }
    $removed = @(Remove-UnusedPillowModules $site)
    Write-Host "Pillow modules left out: $($removed -join ', ')"
    foreach ($name in $Programs.Keys) { Copy-Item (Join-Path $release $name) (Join-Path $stage $Programs[$name]) }
    Copy-Licenses $RepoRoot $Runtime.Prefix $stage
    Add-Notices $RepoRoot $stage $interpreter
    Invoke-Native 'compile the bytecode' {
        & $interpreter -m compileall -q -j 0 --invalidation-mode unchecked-hash -s $stage (Join-Path $stage 'Lib') (Join-Path $stage 'app')
    }
}

# The commit the build comes from: GITHUB_SHA in GitHub Actions, else the commit .git\HEAD
# names, read from the repository's files (git itself is not run); $null when unknown.
function Get-SourceCommit([string]$RepoRoot) {
    if ($env:GITHUB_SHA) { return $env:GITHUB_SHA }
    $git = Join-Path $RepoRoot '.git'
    $head = Join-Path $git 'HEAD'
    if (-not (Test-Path -LiteralPath $head -PathType Leaf)) { return $null }
    $text = (Get-Content -Raw -LiteralPath $head).Trim()
    if ($text -match '^[0-9a-f]{40}$') { return $text }
    if ($text -notmatch '^ref:\s*(\S+)$') { return $null }
    $ref = $Matches[1]
    $loose = Join-Path $git ($ref -replace '/', '\')
    if (Test-Path -LiteralPath $loose -PathType Leaf) { return (Get-Content -Raw -LiteralPath $loose).Trim() }
    $packed = Join-Path $git 'packed-refs'
    if (Test-Path -LiteralPath $packed -PathType Leaf) {
        $pattern = '^([0-9a-f]{40}) ' + [regex]::Escape($ref) + '$'
        foreach ($line in Get-Content -LiteralPath $packed) {
            $match = [regex]::Match($line, $pattern)
            if ($match.Success) { return $match.Groups[1].Value }
        }
    }
    return $null
}

function Read-BuildInfo([string]$Path) {
    if (Test-Path -LiteralPath $Path) { return Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json }
    return [pscustomobject]@{}
}

# Adds $Values to build-info.json (or replaces the same keys) and writes it.
function Write-BuildInfo([string]$Path, [System.Collections.IDictionary]$Values) {
    $info = Read-BuildInfo $Path
    foreach ($key in $Values.Keys) { $info | Add-Member -NotePropertyName $key -NotePropertyValue $Values[$key] -Force }
    Write-TextFile $Path ($info | ConvertTo-Json -Depth 6)
}

# The release notes (Markdown) of the setup $SetupName with the SHA-256 $SetupHash. Only the
# release workflow's run on a version tag attests the setup, and GitHub attests a release only
# when it publishes it as immutable, so `gh attestation verify` is shown only with $Attested and
# `gh release verify` only with $Immutable.
function New-ReleaseNotes([string]$Version, [string]$SetupName, [string]$SetupHash, [bool]$Signed, [bool]$Attested, [bool]$Immutable) {
    $arrow = [string][char]0x203A
    $fence = '```'
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add('## Install')
    $lines.Add('')
    $lines.Add(('1. Download `{0}` below.' -f $SetupName))
    $lines.Add(('2. Check that its SHA-256 is `{0}` (see *Verify your download*).' -f $SetupHash))
    $lines.Add('3. Run it and approve the UAC prompt. Cairn installs for all users into `C:\Program Files\Cairn`; a Cairn that is already installed is upgraded in place.')
    $lines.Add('')
    $lines.Add(('Requirements: Windows 11, 64-bit (x64). To uninstall: Settings {0} Apps {0} Installed apps {0} Cairn {0} Uninstall.' -f $arrow))
    $lines.Add('')
    $lines.Add('## Verify your download')
    $lines.Add('')
    $lines.Add($fence + 'powershell')
    $lines.Add(('Get-FileHash .\{0} -Algorithm SHA256        # compare with SHA256SUMS.txt' -f $SetupName))
    if ($Signed) {
        $lines.Add(('Get-AuthenticodeSignature .\{0} | Format-List Status, SignerCertificate, TimeStamperCertificate' -f $SetupName))
    }
    $explained = New-Object System.Collections.Generic.List[string]
    if ($Attested) {
        $lines.Add(('gh attestation verify .\{0} --repo {1}' -f $SetupName, $Repository))
        $explained.Add('`gh attestation verify` checks that GitHub Actions built the setup from this repository.')
    }
    if ($Immutable) {
        $lines.Add(('gh release verify v{0} --repo {1}' -f $Version, $Repository))
        $explained.Add('`gh release verify` checks the attestation GitHub made when it published this release as immutable.')
    }
    $lines.Add($fence)
    $lines.Add('')
    if ($explained.Count) {
        $lines.Add(($explained -join ' '))
        $lines.Add('')
    }
    $lines.Add('## SmartScreen and Smart App Control')
    $lines.Add('')
    if ($Signed) {
        $lines.Add('Cairn''s setup and programs are code-signed, so Windows names *SignPath Foundation* as their publisher. Microsoft Defender SmartScreen can still show *Windows protected your PC* for a new release until enough people have downloaded it: choose **More info**, then **Run anyway**, once you trust the file.')
        $lines.Add('')
        $lines.Add('Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org).')
        $lines.Add(('See the [code signing policy]({0}/blob/main/docs/code-signing-policy.md).' -f $RepositoryUrl))
    } else {
        $lines.Add('Cairn''s setup and programs are not code-signed yet, so Windows treats them as unknown:')
        $lines.Add('')
        $lines.Add('- **Microsoft Defender SmartScreen** may show *Windows protected your PC* when you run the setup. Choose **More info**, then **Run anyway**, once you trust the file.')
        $lines.Add(('- **Smart App Control** (Windows Security {0} App & browser control) is stricter: while it is on, it can block unsigned programs and libraries outright (the setup, the installed Cairn and a development build alike), and it offers no exception for a single app.' -f $arrow))
    }
    $lines.Add('')
    $lines.Add('## Changes')
    $lines.Add('')
    return ($lines -join "`n")
}

# A version string quoted exactly, padding included, with its length.
function Format-ExactString([string]$Text) {
    return '"{0}" ({1} characters)' -f $Text, $Text.Length
}

# signpath-metadata.txt: for each file a signing request restricts (Configuration, Name,
# ProductName, ProductVersion; the names $null when this build did not make the file), its
# exact strings.
function New-SignPathMetadata([object[]]$Entries) {
    $lines = New-Object System.Collections.Generic.List[string]
    foreach ($comment in @(
            '# ProductName and ProductVersion of the files that the SignPath artifact configurations',
            '# (.signpath\artifact-configurations) restrict, read from the files of this build. Each',
            '# string is quoted exactly as the file carries it, padding included, and followed by its',
            '# length. Inno Setup pads the strings of the setup and of its uninstaller; when a signing',
            '# request shows that SignPath compares them exactly, paste these values into product-name',
            '# and product-version.',
            '')) {
        $lines.Add($comment)
    }
    foreach ($entry in $Entries) {
        $lines.Add(('{0}: {1}' -f $entry.Configuration, $entry.Name))
        if ($null -eq $entry.ProductName) {
            $lines.Add('  not made by this build (the uninstaller comes from a -StageOnly run with -SigningDir)')
            continue
        }
        $lines.Add('  product-name ' + (Format-ExactString $entry.ProductName))
        $lines.Add('  product-version ' + (Format-ExactString $entry.ProductVersion))
    }
    return ($lines -join "`n") + "`n"
}

# The entries of signpath-metadata.txt: Cairn's own files in the stage, the uninstaller this run
# recorded in build-info.json and the setup.
function Get-SignPathEntries([string]$Stage, [string]$Setup, [object]$Info) {
    foreach ($relative in $OwnFiles.Keys) {
        $path = Join-Path $Stage $relative
        $name = $null
        $productVersion = $null
        if (Test-Path -LiteralPath $path) {
            $version = (Get-Item -LiteralPath $path).VersionInfo
            $name = [string]$version.ProductName
            $productVersion = [string]$version.ProductVersion
        }
        [pscustomobject]@{ Configuration = 'binaries'; Name = $OwnFiles[$relative].Replace('\', '/'); ProductName = $name; ProductVersion = $productVersion }
    }
    $stubName = $null
    $stubVersion = $null
    if ($Info.PSObject.Properties['uninstaller_product_name']) {
        $stubName = [string]$Info.uninstaller_product_name
        $stubVersion = [string]$Info.uninstaller_product_version
    }
    [pscustomobject]@{ Configuration = 'binaries'; Name = $UninstallerUpload.Replace('\', '/'); ProductName = $stubName; ProductVersion = $stubVersion }
    $setupInfo = (Get-Item -LiteralPath $Setup).VersionInfo
    [pscustomobject]@{ Configuration = 'setup'; Name = (Split-Path -Leaf $Setup); ProductName = [string]$setupInfo.ProductName; ProductVersion = [string]$setupInfo.ProductVersion }
}

function New-SymbolsArchive([string]$Release, [string]$TelemetryPdb, [string]$Archive) {
    $pdbs = @($SymbolFiles | ForEach-Object { Join-Path $Release $_ } | Where-Object { Test-Path $_ })
    if ($TelemetryPdb -and (Test-Path -LiteralPath $TelemetryPdb)) { $pdbs += $TelemetryPdb }
    if (Test-Path -LiteralPath $Archive) { Remove-Item -LiteralPath $Archive -Force }
    if (-not $pdbs.Count) {
        Write-Warning "No PDB was found in $Release; $Archive is not written."
        return $null
    }
    Compress-Archive -Path $pdbs -DestinationPath $Archive
    return $Archive
}

function Get-BuildPaths([string]$RepoRoot, [string]$Target) {
    $dist = Join-Path $RepoRoot 'build\dist'
    $targetDir = if ($Target) { $Target } else { Join-Path $RepoRoot 'target\dist' }
    return @{
        Dist = $dist
        Stage = Join-Path $dist 'stage'
        BuildInfo = Join-Path $dist 'build-info.json'
        Cpp = Join-Path $RepoRoot 'build\dist-cpp'
        Work = Join-Path $RepoRoot 'build\dist-work'
        Target = $targetDir
    }
}

# Steps 1-6, and with -SigningDir the files to sign.
function Invoke-Stage([hashtable]$Options, [string]$Version, [hashtable]$Paths) {
    if (-not $Options.SkipChecks) { Invoke-Checks $Root ([bool]$Options.AllowPending) }
    $runtime = Resolve-Python $Options.Python
    Write-Host "Python $($runtime.Version) ($($runtime.Executable))"
    $telemetry = Invoke-TelemetryBuild $Root $Paths.Cpp $Options.Generator
    Invoke-RustBuild $Root $Paths.Target $runtime.Executable
    New-Item -ItemType Directory -Force $Paths.Dist | Out-Null
    if (Test-Path -LiteralPath $Paths.BuildInfo) { Remove-Item -LiteralPath $Paths.BuildInfo -Force }
    New-Stage $Root $Paths $runtime $telemetry.Dll
    $loaded = Invoke-StageGates $Paths.Stage $Paths.Dist $Version $runtime.Executable $Paths.Work
    Write-UnsignedList $Paths.Stage

    $checks = if ($Options.SkipChecks) { 'skipped' } else { 'run' }
    $gate = 'not run by this build'
    if (-not $Options.SkipChecks) { $gate = if ($Options.AllowPending) { 'pending records allowed' } else { 'CAIRN_RELEASE=1' } }
    $ciRun = $null
    if ($env:GITHUB_RUN_ID) { $ciRun = "$env:GITHUB_SERVER_URL/$env:GITHUB_REPOSITORY/actions/runs/$env:GITHUB_RUN_ID" }
    $image = $null
    if ($env:ImageOS) { $image = "$env:ImageOS $env:ImageVersion".Trim() }
    Write-BuildInfo $Paths.BuildInfo ([ordered]@{
            product = 'Cairn'
            version = $Version
            kind = if ($Options.AllowPending) { 'release candidate' } else { 'release' }
            checks = $checks
            verification_gate = $gate
            commit = Get-SourceCommit $Root
            ci_run = $ciRun
            runner_image = $image
            built = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
            rustc = Get-ToolVersion { rustc -V }
            cargo = Get-ToolVersion { cargo -V }
            cmake = $telemetry.CMake
            cmake_generator = $telemetry.Generator
            msvc = $telemetry.Msvc
            windows_sdk = $telemetry.WindowsSdk
            python = $runtime.Version
            pillow_modules = @($loaded)
        })

    if ($Options.SigningDir) {
        $binaries = Join-Path $Options.SigningDir 'binaries'
        $cache = Join-Path $Options.SigningDir 'uninstaller-cache'
        $output = Join-Path $Options.SigningDir 'probe-output'
        foreach ($folder in @($binaries, $cache, $output)) {
            if (Test-Path -LiteralPath $folder) { Remove-Item -LiteralPath $folder -Recurse -Force }
        }
        Write-Step "files to sign: $binaries"
        Export-FilesToSign $Paths.Stage $binaries
        $iscc = Find-RequiredIscc $Options.IsccPath
        $isccVersion = Assert-IsccVersion $iscc
        $stub = Invoke-UninstallerProbe $iscc $Version $Paths.Stage $cache $output (Join-Path $Root 'installer\cairn.iss')
        $upload = Join-Path $binaries $UninstallerUpload
        New-Item -ItemType Directory -Force (Split-Path -Parent $upload) | Out-Null
        Copy-Item -LiteralPath $stub $upload
        $stubInfo = (Get-Item -LiteralPath $stub).VersionInfo
        Write-BuildInfo $Paths.BuildInfo ([ordered]@{
                iscc = "$isccVersion"
                uninstaller = Split-Path -Leaf $stub
                uninstaller_product_name = [string]$stubInfo.ProductName
                uninstaller_product_version = [string]$stubInfo.ProductVersion
            })
        Write-Host "Files to sign: $binaries" -ForegroundColor Green
    }
}

# Step 7: the setup and the symbols, with the signed files put back first when given.
function Invoke-Package([hashtable]$Options, [string]$Version, [hashtable]$Paths) {
    $stage = $Paths.Stage
    if (-not (Test-Path -LiteralPath (Join-Path $stage 'Cairn.exe'))) {
        throw "There is no stage in $stage; make it with -StageOnly first."
    }
    $info = Read-BuildInfo $Paths.BuildInfo
    if ([string]$info.version -ne $Version) {
        throw "The stage in $stage was made for version '$($info.version)', not $Version; make it again."
    }
    $iscc = Find-RequiredIscc $Options.IsccPath
    $isccVersion = Assert-IsccVersion $iscc
    $cache = $null
    if ($Options.SignedDir) {
        $cache = Join-Path $Options.SigningDir 'uninstaller-cache'
        Write-Step "signed files from $($Options.SignedDir)"
        $stub = Import-SignedFiles $Options.SignedDir $stage $cache
        $problems = @(Find-SignatureProblems @(Get-StageSignatures $stage) $Options.SigningMode $Options.TestSignerThumbprint @($OwnFiles.Keys))
        $stubProblem = Get-SignatureProblem (Get-AuthenticodeSignature -LiteralPath $stub) $Options.SigningMode $Options.TestSignerThumbprint $true
        if ($stubProblem) { $problems += "$UninstallerUpload $stubProblem" }
        if ($problems.Count) { throw ("The signatures are not as -SigningMode $($Options.SigningMode) requires:`n  " + ($problems -join "`n  ")) }
        $runtime = Resolve-Python $Options.Python
        Invoke-StageGates $stage $Paths.Dist $Version $runtime.Executable $Paths.Work | Out-Null
    }
    $defines = @("/DAppVersion=$Version", "/DStageDir=$stage", "/DOutputDir=$($Paths.Dist)")
    if ($cache) { $defines += "/DSignedUninstallerDir=$cache" }
    $issFile = Join-Path $Root 'installer\cairn.iss'
    Invoke-Native 'build the setup' { & $iscc @defines $issFile }
    $setup = Join-Path $Paths.Dist "Cairn-$Version-setup.exe"
    if (-not (Test-Path -LiteralPath $setup)) { throw "ISCC did not write $setup." }
    $telemetryPdb = @('bin\Release\optimizer_telemetry.pdb', 'bin\optimizer_telemetry.pdb') | ForEach-Object { Join-Path $Paths.Cpp $_ } |
        Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    $symbols = New-SymbolsArchive (Join-Path $Paths.Target 'release') $telemetryPdb (Join-Path $Paths.Dist "Cairn-$Version-symbols.zip")
    $values = [ordered]@{ iscc = "$isccVersion"; signing_mode = $Options.SigningMode; symbols_sha256 = $null }
    if ($symbols) { $values['symbols_sha256'] = (Get-FileHash -LiteralPath $symbols -Algorithm SHA256).Hash.ToLowerInvariant() }
    Write-BuildInfo $Paths.BuildInfo $values
    Write-Host "Setup ready: $setup" -ForegroundColor Green
}

# Step 8: the files that go with a release, with the signed setup put in place first when given.
function Invoke-Finalize([hashtable]$Options, [string]$Version, [hashtable]$Paths) {
    $mode = $Options.SigningMode
    $setup = Join-Path $Paths.Dist "Cairn-$Version-setup.exe"
    if ($Options.SignedSetup) {
        Write-Step "signed setup $($Options.SignedSetup)"
        Copy-Item -LiteralPath $Options.SignedSetup $setup -Force
    }
    if (-not (Test-Path -LiteralPath $setup)) { throw "$setup does not exist; build it with -PackageOnly first." }
    if ($mode -ne 'none') {
        $problem = Get-SignatureProblem (Get-AuthenticodeSignature -LiteralPath $setup) $mode $Options.TestSignerThumbprint $true
        if ($problem) { throw "The setup $problem." }
    }
    $name = Split-Path -Leaf $setup
    $hash = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-TextFile (Join-Path $Paths.Dist 'SHA256SUMS.txt') ("{0}  {1}`n" -f $hash, $name)
    $notes = New-ReleaseNotes $Version $name $hash ($mode -eq 'release') ([bool]$Options.Attested) ([bool]$Options.ImmutableReleases)
    Write-TextFile (Join-Path $Paths.Dist 'release-notes.md') $notes
    $info = Read-BuildInfo $Paths.BuildInfo
    Write-TextFile (Join-Path $Paths.Dist 'signpath-metadata.txt') (New-SignPathMetadata @(Get-SignPathEntries $Paths.Stage $setup $info))
    Write-BuildInfo $Paths.BuildInfo ([ordered]@{ signing_mode = $mode; setup = $name; setup_sha256 = $hash })
    Write-Host "Release files ready: $($Paths.Dist) ($name, SHA-256 $hash)" -ForegroundColor Green
}

function Invoke-Build([hashtable]$Options) {
    $problems = @(Test-BuildArguments $Options)
    if ($problems.Count) { throw ("These parameters do not go together:`n  " + ($problems -join "`n  ")) }
    if (-not $Options.SigningMode) { $Options.SigningMode = 'none' }
    foreach ($key in @('SigningDir', 'SignedDir', 'SignedSetup', 'Python', 'IsccPath', 'TargetDir')) {
        if ($Options[$key]) { $Options[$key] = Resolve-FullPath $Options[$key] }
    }
    $version = Get-CairnVersion $Root
    Write-Host "Cairn $version" -ForegroundColor Green
    $paths = Get-BuildPaths $Root $Options.TargetDir
    if ($Options.FinalizeOnly) {
        Invoke-Finalize $Options $version $paths
        return
    }
    if ($Options.PackageOnly) {
        Invoke-Package $Options $version $paths
        return
    }
    Invoke-Stage $Options $version $paths
    if ($Options.StageOnly) {
        Write-Host "Stage ready: $($paths.Stage)" -ForegroundColor Green
        return
    }
    Invoke-Package $Options $version $paths
    Invoke-Finalize $Options $version $paths
}

# Dot-sourcing loads the functions only.
if ($MyInvocation.InvocationName -eq '.') { return }

$BuildOptions = @{
    SkipChecks = [bool]$SkipChecks
    StageOnly = [bool]$StageOnly
    PackageOnly = [bool]$PackageOnly
    FinalizeOnly = [bool]$FinalizeOnly
    SigningDir = $SigningDir
    SignedDir = $SignedDir
    SignedSetup = $SignedSetup
    SigningMode = $SigningMode
    TestSignerThumbprint = $TestSignerThumbprint
    Python = $Python
    Generator = $Generator
    AllowPending = [bool]$AllowPending
    Attested = [bool]$Attested
    ImmutableReleases = [bool]$ImmutableReleases
    IsccPath = $IsccPath
    TargetDir = $TargetDir
}
$saved = @{}
foreach ($name in @('CARGO_ENCODED_RUSTFLAGS', 'PYO3_PYTHON', 'CAIRN_REQUIRE_RESOURCES', 'CAIRN_RELEASE')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    Invoke-Build $BuildOptions
} finally {
    # A null value removes the variable when the caller did not have it.
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
