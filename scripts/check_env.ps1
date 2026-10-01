# Fails fast (exit 1) if any build prerequisite is missing. Called first by the "build: all" task.
$ErrorActionPreference = 'Continue'
$missing = @()

foreach ($t in 'cargo', 'rustc', 'cmake', 'py') {
    $c = Get-Command $t -ErrorAction SilentlyContinue
    if ($c) { Write-Host ("{0,-6} OK  {1}" -f $t, $c.Source) -ForegroundColor Green }
    else    { Write-Host ("{0,-6} MISSING" -f $t) -ForegroundColor Red; $missing += $t }
}

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = $null
if (Test-Path $vswhere) {
    $vs = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath | Select-Object -First 1
}
if ($vs) { Write-Host "MSVC   OK  $vs" -ForegroundColor Green } else { Write-Host "MSVC   MISSING (VS 2022 Build Tools with C++ workload)" -ForegroundColor Red; $missing += 'msvc' }

if ($missing.Count) {
    Write-Host "`nMissing: $($missing -join ', '). Run task 'env: bootstrap toolchains (winget)' then open a new terminal." -ForegroundColor Yellow
    exit 1
}
Write-Host "`nToolchain OK." -ForegroundColor Green
exit 0
