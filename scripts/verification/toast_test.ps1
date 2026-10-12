<#
.SYNOPSIS
  Shows a test notification after a delay, so it arrives while the PC is locked.
.DESCRIPTION
  Waits -Delay seconds, then shows one notification with the text -Text from Windows PowerShell.
  Start it and press Win+L at once: when lock-screen notifications are on, the notification
  appears on the lock screen; when they are off, it does not, and it waits in the notification
  centre after you sign in.

  Run it from a PowerShell prompt without administrator rights: Windows shows no notification
  from an elevated process, so an elevated run proves nothing. Lock the PC before the delay
  ends and stay on the lock screen until the notification has fired; a notification that fires
  after you have signed in again shows on the desktop, whatever the setting.

  If the notification does not appear on the lock screen even before any change, check
  Settings › System › Notifications › Windows PowerShell › 'Show notifications on the lock screen',
  or use an app with its own entry there instead, such as a timer of the Clock app. Do not run it
  while Do not disturb is on.

  It changes no setting.
.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verification\toast_test.ps1
  # from a prompt without administrator rights; press Win+L before the 20 seconds are over
#>
param(
    [int]$Delay = 20,
    [string]$Text = 'Cairn test'
)
$ErrorActionPreference = 'Stop'
# Windows PowerShell's own app id, so the notification is attributed to an app Settings lists.
$appId = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
"press Win+L now; the notification follows in $Delay seconds"
Start-Sleep -Seconds $Delay
$null = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
$template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent(
    [Windows.UI.Notifications.ToastTemplateType]::ToastText01)
# Item(0), not [0]: indexing the WinRT node list enumerates it, which Windows PowerShell rejects.
$null = $template.GetElementsByTagName('text').Item(0).AppendChild($template.CreateTextNode($Text))
$notification = [Windows.UI.Notifications.ToastNotification]::new($template)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($appId).Show($notification)
"notification '$Text' shown at $(Get-Date -Format 'HH:mm:ss')"
