<#
.SYNOPSIS
  Runs the UI test suite, one run at a time on this machine.
.DESCRIPTION
  Takes the named mutex Local\PCOptimizerPytest (a mutex abandoned by a crashed run counts
  as taken), sets the test guards to 1 and runs pytest in ui\ with the project's virtual
  environment. The arguments are passed to pytest (default: tests -q). Exits with pytest's
  exit code.

  The guards make the engine refuse, for the whole run: creating a System Restore point
  (OPTIMIZER_FORBID_RESTORE_POINT), a disk speed test at a volume root
  (OPTIMIZER_FORBID_DRIVE_TESTS), an online Windows Update search
  (OPTIMIZER_FORBID_UPDATE_SEARCH) and installing or upgrading apps with winget
  (OPTIMIZER_FORBID_APP_INSTALLS). The tests' conftest sets them too, and points
  OPTIMIZER_DATA_DIR at a throwaway folder inside the pytest process.

  The script runs inside the calling PowerShell session, so it puts back the caller's guard
  variables (or their absence) and working directory when it ends, also after an error or
  Ctrl+C: an app or engine started later from the same terminal behaves as before.
.EXAMPLE
  .\scripts\pytest_serial.ps1 tests -q
.EXAMPLE
  .\scripts\pytest_serial.ps1 tests\test_history.py -q
#>
$root = Split-Path -Parent $PSScriptRoot
$python = Join-Path $root '.venv\Scripts\python.exe'
$ui = Join-Path $root 'ui'
$pytestArgs = if ($args.Count -gt 0) { @($args) } else { @('tests', '-q') }
$guards = @(
    'OPTIMIZER_FORBID_RESTORE_POINT',
    'OPTIMIZER_FORBID_DRIVE_TESTS',
    'OPTIMIZER_FORBID_UPDATE_SEARCH',
    'OPTIMIZER_FORBID_APP_INSTALLS'
)

function Enter-Mutex([System.Threading.Mutex]$Mutex, [int]$TimeoutMs) {
    try {
        return $Mutex.WaitOne($TimeoutMs)
    } catch {
        $e = $_.Exception
        while ($e) {
            if ($e -is [System.Threading.AbandonedMutexException]) { return $true }
            $e = $e.InnerException
        }
        throw
    }
}

$previous = @{}
foreach ($name in $guards) {
    $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$mutex = New-Object System.Threading.Mutex($false, 'Local\PCOptimizerPytest')
$acquired = $false
$code = 1
try {
    $acquired = Enter-Mutex $mutex 0
    if (-not $acquired) {
        Write-Host 'Another UI test run is in progress; waiting for it to finish...'
        $acquired = Enter-Mutex $mutex -1
    }
    foreach ($name in $guards) {
        [Environment]::SetEnvironmentVariable($name, '1', 'Process')
    }
    Push-Location $ui
    try {
        & $python -m pytest @pytestArgs
        $code = $LASTEXITCODE
    } finally {
        Pop-Location
    }
} finally {
    # A null value removes the variable when the caller did not have it.
    foreach ($name in $guards) {
        [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process')
    }
    if ($acquired) { $mutex.ReleaseMutex() }
    $mutex.Dispose()
}
exit $code
