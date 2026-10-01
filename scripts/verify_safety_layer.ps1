# Exercises optctl.exe end to end. Must run elevated; writes scripts\verify_safety_layer.log.
# OPTIMIZER_FORBID_RESTORE_POINT=1 keeps every call from creating a System Restore point, so
# the restore-point step checks that the engine refuses. The caller's value (or its absence) is
# put back at the end, so an app started later from the same terminal can create restore points.
$previousForbid = $env:OPTIMIZER_FORBID_RESTORE_POINT
$env:OPTIMIZER_FORBID_RESTORE_POINT = '1'
$root = Split-Path -Parent $PSScriptRoot
$exe  = Join-Path $root 'target\release\optctl.exe'
$log  = Join-Path $PSScriptRoot 'verify_safety_layer.log'
Start-Transcript -Path $log -Force | Out-Null

function Section($title) { Write-Host "`n=== $title ===" }

try {
    Section 'doctor'
    & $exe doctor

    Section 'restore-point (refused while OPTIMIZER_FORBID_RESTORE_POINT=1)'
    & $exe restore-point -d 'Cairn safety-layer verification'
    Write-Host "exit=$LASTEXITCODE"

    Section 'reg-get'
    & $exe reg-get 'HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\SystemRestore' RPSessionInterval

    Section 'svc-get SysMain'
    & $exe svc-get SysMain

    Section 'rollback (plan only)'
    & $exe rollback
    Write-Host "exit=$LASTEXITCODE"

    Section 'journal summary'
    & $exe journal summary
} finally {
    # Assigning $null removes the variable when the caller did not have it.
    $env:OPTIMIZER_FORBID_RESTORE_POINT = $previousForbid
    Stop-Transcript | Out-Null
}
