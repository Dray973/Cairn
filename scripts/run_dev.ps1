<#
.SYNOPSIS
  Starts the development copy of Cairn (`pythonw -m optimizer` in ui\ with the project's
  virtual environment), like the VS Code task "run: gui (Administrator)", without VS Code.
.DESCRIPTION
  By default Windows asks for administrator rights first, since changes need them; -Standard
  starts the window without them (it then offers "Restart as administrator").

  The development copy and an installed Cairn share the change history and the single-instance
  lock: while one of them is open, starting the other brings the open window to the front.
  The natives must be built and deployed first (the "build: all" task, or
  scripts\deploy_natives.ps1 after a build).
.PARAMETER Standard
  Starts the window without asking for administrator rights.
.EXAMPLE
  .\scripts\run_dev.ps1
.EXAMPLE
  .\scripts\run_dev.ps1 -Standard
#>
param(
    [switch]$Standard
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$pythonw = Join-Path $root '.venv\Scripts\pythonw.exe'
$ui = Join-Path $root 'ui'

if (-not (Test-Path $pythonw)) {
    throw "No virtual environment in $root\.venv: run the VS Code task 'python: create venv' first."
}
foreach ($name in @('optimizer_engine.pyd', 'optimizer_telemetry.dll')) {
    if (-not (Test-Path (Join-Path $ui "optimizer\native\$name"))) {
        throw "ui\optimizer\native\$name is missing: build the natives and run scripts\deploy_natives.ps1."
    }
}

$arguments = @('-m', 'optimizer')
if ($Standard) {
    Start-Process -FilePath $pythonw -ArgumentList $arguments -WorkingDirectory $ui
} else {
    Start-Process -FilePath $pythonw -ArgumentList $arguments -WorkingDirectory $ui -Verb RunAs
}
