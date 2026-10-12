<#
.SYNOPSIS
  Asks Windows whether a desktop app may use location now.
.DESCRIPTION
  Calls Geolocator.RequestAccessAsync from Windows PowerShell and prints the answer: Allowed,
  Denied or Unspecified. Run it before applying the location tweak (location services on), after
  applying it, and after Undo.

  It changes no setting. Windows may list Windows PowerShell among the apps that recently asked
  for location.
.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verification\location_access.ps1
#>
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Devices.Geolocation.Geolocator, Windows.Devices.Geolocation, ContentType = WindowsRuntime]
$asTask = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
    $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
} | Select-Object -First 1
$operation = [Windows.Devices.Geolocation.Geolocator]::RequestAccessAsync()
$task = $asTask.MakeGenericMethod([Windows.Devices.Geolocation.GeolocationAccessStatus]).Invoke($null, @($operation))
if (-not $task.Wait(10000)) {
    throw 'Windows did not answer within 10 seconds.'
}
"location access for desktop apps: $($task.Result)"
