# Installs only VS 2022 Build Tools (MSVC v143 + Windows 11 SDK). Elevated. Logs to scripts\buildtools.log.
$log = Join-Path $PSScriptRoot 'buildtools.log'
Start-Transcript -Path $log -Force | Out-Null
$vsOverride = '--quiet --wait --norestart --nocache' +
    ' --add Microsoft.VisualStudio.Workload.VCTools' +
    ' --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64' +
    ' --add Microsoft.VisualStudio.Component.Windows11SDK.22621' +
    ' --add Microsoft.VisualStudio.Component.VC.CMake.Project' +
    ' --includeRecommended'
& winget install --id Microsoft.VisualStudio.2022.BuildTools --exact --silent --accept-package-agreements --accept-source-agreements --disable-interactivity --override $vsOverride
Write-Host "winget exit code: $LASTEXITCODE"
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) { & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath } else { Write-Host 'vswhere still missing' }
Write-Host 'BUILDTOOLS_DONE'
Stop-Transcript | Out-Null
