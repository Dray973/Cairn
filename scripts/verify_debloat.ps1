# Elevated verification of the debloat engine. Changes nothing on the machine except a
# throwaway service (PCOptimizerSelfTest) that the service test creates and deletes and, with
# -LibTestExe, a sandbox scheduled task under \PCOptimizerSelfTest\ that the scheduled task
# test registers and deletes. Every optctl call below is a scan, a read or a dry run.
# Usage (elevated):
#   powershell -ExecutionPolicy Bypass -File scripts\verify_debloat.ps1 -TestExe <path> [-LibTestExe <path>]
# -TestExe is the debloat integration test executable (target\debug\deps\debloat-*.exe);
# -LibTestExe is optimizer_core's unit test executable (target\debug\deps\optimizer_core-*.exe).
param(
    [Parameter(Mandatory = $true)] [string]$TestExe,
    [string]$LibTestExe
)
# No step may create a System Restore point. The caller's value (or its absence) is put back at
# the end, so an app started later from the same terminal can create restore points.
$previousForbid = $env:OPTIMIZER_FORBID_RESTORE_POINT
$env:OPTIMIZER_FORBID_RESTORE_POINT = '1'
$root = Split-Path -Parent $PSScriptRoot
$optctl = Join-Path $root 'target\release\optctl.exe'
$log = Join-Path $PSScriptRoot 'verify_debloat.log'
Start-Transcript -Path $log -Force | Out-Null

function Section($title) { Write-Host "`n=== $title ===" }

try {
    Section 'service round trip (throwaway service)'
    & $TestExe --ignored --exact service_action_round_trip_on_throwaway_service --nocapture
    Write-Host "exit=$LASTEXITCODE"
    $left = Get-Service -Name PCOptimizerSelfTest -ErrorAction SilentlyContinue
    Write-Host ("throwaway service left behind: {0}" -f [bool]$left)

    if ($LibTestExe) {
        Section 'scheduled task round trip (sandbox task under \PCOptimizerSelfTest\)'
        & $LibTestExe --ignored --exact debloat::scheduled_tasks::tests::sandbox_task_round_trip --nocapture
        Write-Host "exit=$LASTEXITCODE"
        # Both must be empty: no sandbox task, and no empty task folder on disk.
        $leftTasks = @(Get-ScheduledTask -TaskPath '\PCOptimizerSelfTest\' -ErrorAction SilentlyContinue)
        Write-Host ("sandbox tasks left behind: {0}" -f $leftTasks.Count)
        $taskFolder = Join-Path $env:SystemRoot 'System32\Tasks\PCOptimizerSelfTest'
        Write-Host ("sandbox task folder left behind: {0}" -f (Test-Path $taskFolder))
    }

    Section 'optctl doctor'
    & $optctl doctor

    Section 'optctl scan'
    & $optctl scan

    Section 'optctl task-get (read-only)'
    & $optctl task-get '\Microsoft\Windows\Autochk\Proxy'
    Write-Host "exit=$LASTEXITCODE"

    foreach ($category in 'privacy', 'gaming', 'performance', 'bloatware') {
        Section "optctl apply --category $category --dry-run"
        & $optctl apply --category $category --dry-run
        Write-Host "exit=$LASTEXITCODE"
    }

    Section 'optctl revert --all --dry-run'
    & $optctl revert --all --dry-run
    Write-Host "exit=$LASTEXITCODE"

    Section 'journal summary'
    & $optctl journal summary
} finally {
    # Assigning $null removes the variable when the caller did not have it.
    $env:OPTIMIZER_FORBID_RESTORE_POINT = $previousForbid
    Stop-Transcript | Out-Null
}
