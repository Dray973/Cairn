<#
.SYNOPSIS
  Copies the freshly built native artifacts into ui\optimizer\native so `python -m optimizer` can load them.
    build\cpp\bin\<Config>\optimizer_telemetry.dll  →  ui\optimizer\native\optimizer_telemetry.dll
    target\<profile>\optimizer_engine.dll           →  ui\optimizer\native\optimizer_engine.pyd
#>
param(
    [ValidateSet('Release', 'Debug')] [string]$Config = 'Release'
)
$ErrorActionPreference = 'Stop'
$root    = Split-Path -Parent $PSScriptRoot
$native  = Join-Path $root 'ui\optimizer\native'
$profile = if ($Config -eq 'Release') { 'release' } else { 'debug' }

New-Item -ItemType Directory -Force -Path $native | Out-Null

$dll = Join-Path $root "build\cpp\bin\$Config\optimizer_telemetry.dll"
$pdb = Join-Path $root "build\cpp\bin\$Config\optimizer_telemetry.pdb"
$pyd = Join-Path $root "target\$profile\optimizer_engine.dll"
$pydPdb = Join-Path $root "target\$profile\optimizer_engine.pdb"

if (-not (Test-Path $dll)) { throw "telemetry DLL not built: $dll" }
if (-not (Test-Path $pyd)) { throw "engine cdylib not built: $pyd" }

Copy-Item $dll (Join-Path $native 'optimizer_telemetry.dll') -Force
if (Test-Path $pdb) { Copy-Item $pdb (Join-Path $native 'optimizer_telemetry.pdb') -Force }
Copy-Item $pyd (Join-Path $native 'optimizer_engine.pyd') -Force
if (Test-Path $pydPdb) { Copy-Item $pydPdb (Join-Path $native 'optimizer_engine.pdb') -Force }

Write-Host "Deployed ($Config):" -ForegroundColor Green
Get-ChildItem $native -Include *.dll, *.pyd -Recurse | ForEach-Object { Write-Host ("  {0,-28} {1,10:N0} bytes" -f $_.Name, $_.Length) }
