; Cairn setup (Inno Setup 6.3 or later).
;
; Built by scripts\build_release.ps1:
;   ISCC /DAppVersion=<x.y.z> /DStageDir=<stage folder> /DOutputDir=<output folder> installer\cairn.iss
;
; Installs per machine into Program Files\Cairn (no folder choice), upgrades in place, refuses
; downgrades and removes only what it installed plus Cairn's scheduled maintenance tasks. The
; change history, the logs and every other file of the user's data folder stay: the history is
; the only way to undo changes that are still applied.

#ifndef AppVersion
  #error AppVersion must be defined (/DAppVersion=x.y.z)
#endif
; The repository folder, from this script's folder.
#define Repo AddBackslash(SourcePath) + ".."
#ifndef StageDir
  #define StageDir Repo + "\build\dist\stage"
#endif
#ifndef OutputDir
  #define OutputDir Repo + "\build\dist"
#endif

#define AppName "Cairn"
#define AppPublisher "Dray973"
#define AppUrl "https://github.com/Dray973/Cairn"
; The uninstall entry of AppId below (HKLM, 64-bit view); optimizer_core::app::UNINSTALL_KEY.
#define UninstallKey "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{616A27F7-C15A-4244-ACD1-B7F536649EA0}_is1"

[Setup]
; optimizer_core::app::INSTALLER_APP_ID. Upgrades and the uninstaller rely on it: never change it.
AppId={{616A27F7-C15A-4244-ACD1-B7F536649EA0}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
AppCopyright=Copyright (c) 2026 {#AppPublisher}
VersionInfoVersion={#AppVersion}.0
VersionInfoCompany={#AppPublisher}
VersionInfoDescription={#AppName} Setup
VersionInfoProductName={#AppName}
; Always Program Files\Cairn: the launcher elevates only from a folder that administrators alone
; can change, so there is no folder page and no earlier folder is reused.
DefaultDirName={autopf}\{#AppName}
DisableDirPage=yes
UsePreviousAppDir=no
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
OutputDir={#OutputDir}
OutputBaseFilename=Cairn-{#AppVersion}-setup
SetupIconFile={#Repo}\ui\optimizer\assets\cairn.ico
UninstallDisplayIcon={app}\Cairn.exe
UninstallDisplayName={#AppName}
LicenseFile={#Repo}\LICENSE
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; The window's single-instance mutex (Local\Cairn.Instance): setup and uninstall ask to close
; Cairn first. Restart Manager may close it when the user agrees; it never forces anything.
AppMutex=Cairn.Instance
SetupMutex=Cairn.Setup
CloseApplications=yes
CloseApplicationsFilter=*.exe,*.dll,*.pyd
RestartApplications=no
UsedUserAreasWarning=no
#ifdef SignTool
SignTool={#SignTool}
SignedUninstaller=yes
#endif

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

; An upgrade replaces the runtime and the app instead of adding to them, so removed modules and
; stale compiled files do not stay behind. Only in a folder this installer registered as
; Cairn's, so a /DIR= that names another program's folder never loses anything.
[InstallDelete]
Type: filesandordirs; Name: "{app}\Lib"; Check: CairnIsInstalledHere
Type: filesandordirs; Name: "{app}\DLLs"; Check: CairnIsInstalledHere
Type: filesandordirs; Name: "{app}\tcl"; Check: CairnIsInstalledHere
Type: filesandordirs; Name: "{app}\app"; Check: CairnIsInstalledHere
Type: filesandordirs; Name: "{app}\licenses"; Check: CairnIsInstalledHere

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\Cairn.exe"; WorkingDir: "{app}"; AppUserModelID: "Cairn.App"; Comment: "Safe, reversible Windows 11 tuning"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\Cairn.exe"; WorkingDir: "{app}"; AppUserModelID: "Cairn.App"; Tasks: desktopicon

[Run]
; runascurrentuser starts Cairn with setup's own administrator rights, so the launcher runs the
; window here without a second prompt. A postinstall entry without it starts as the user who
; started setup, and the launcher would then ask again.
Filename: "{app}\Cairn.exe"; Description: "Open Cairn"; Flags: nowait postinstall skipifsilent runascurrentuser

[UninstallRun]
; Removes Cairn's scheduled maintenance tasks (optimizer_core::app::uninstall_steps).
Filename: "{app}\optctl.exe"; Parameters: "uninstall-cleanup --yes"; Flags: runhidden waituntilterminated; RunOnceId: "CairnCleanup"

[Code]
const
  UninstallKey = '{#UninstallKey}';
  // Exit codes of optctl.exe: `journal pending` (0 none or no journal, 3 pending, anything else
  // unknown) and `maintenance status --running` (4 while a scheduled run holds the lock).
  ExitPending = 3;
  ExitMaintenanceRunning = 4;
  MaintenanceRunningText = 'Scheduled maintenance is running. Wait for it to finish, then choose Retry.';
  MaintenanceCancelledText = 'Setup was cancelled because scheduled maintenance is running. Run setup again when it has finished.';
  PendingText = 'Cairn still has changes that can be undone for your account. Uninstalling leaves them in place: the settings stay as Cairn set them. To put them back first, choose No, open Cairn and use Revert All Changes.' #13#10#13#10 'Uninstall anyway?';
  CouldNotCheckText = 'Cairn could not check for changes that can still be undone for your account (code %d). Uninstalling leaves any such changes in place: the settings stay as Cairn set them. To put them back first, choose No, open Cairn and use Revert All Changes.' #13#10#13#10 'Uninstall anyway?';

function SameFolder(const A, B: String): Boolean;
begin
  Result := CompareText(RemoveBackslashUnlessRoot(Trim(A)), RemoveBackslashUnlessRoot(Trim(B))) = 0;
end;

// True only when the install folder holds Cairn.exe and is the folder the uninstall entry names.
function CairnIsInstalledHere(): Boolean;
var
  Location: String;
begin
  Result := FileExists(ExpandConstant('{app}\Cairn.exe')) and
    RegQueryStringValue(HKLM64, UninstallKey, 'InstallLocation', Location) and
    SameFolder(Location, ExpandConstant('{app}'));
end;

// Runs the installed optctl.exe hidden and waits for it. False when it could not be started;
// Code is then the Windows error.
function RunOptctl(const Params: String; var Code: Integer): Boolean;
begin
  Result := Exec(ExpandConstant('{app}\optctl.exe'), Params, ExpandConstant('{app}'), SW_HIDE,
    ewWaitUntilTerminated, Code);
end;

function MaintenanceIsRunning(): Boolean;
var
  Code: Integer;
begin
  Result := RunOptctl('maintenance status --running', Code) and (Code = ExitMaintenanceRunning);
end;

// Waits, with the user's Retry, while a scheduled maintenance run is in progress; a run is never
// ended from here. False when the user chose Cancel.
function WaitForMaintenance(): Boolean;
begin
  Result := True;
  while MaintenanceIsRunning() do
  begin
    if SuppressibleMsgBox(MaintenanceRunningText, mbError, MB_RETRYCANCEL, IDCANCEL) <> IDRETRY then
    begin
      Result := False;
      exit;
    end;
  end;
end;

function UninstallAnyway(const Text: String): Boolean;
begin
  Result := MsgBox(Text, mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
end;

function InitializeSetup(): Boolean;
var
  Installed: String;
  Have, Want: Int64;
begin
  Result := True;
  if RegQueryStringValue(HKLM64, UninstallKey, 'DisplayVersion', Installed) and
     StrToVersion(Installed, Have) and StrToVersion('{#AppVersion}', Want) and
     (ComparePackedVersion(Have, Want) > 0) then
  begin
    SuppressibleMsgBox('A newer version of Cairn (' + Installed + ') is installed. Uninstall it first to install {#AppVersion}.',
      mbError, MB_OK, IDOK);
    Result := False;
  end;
end;

// An upgrade waits for a scheduled maintenance run of the installed copy to finish.
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  // Nested so optctl.exe runs only from a folder this installer registered as Cairn's.
  if FileExists(ExpandConstant('{app}\optctl.exe')) then
    if CairnIsInstalledHere() then
      if not WaitForMaintenance() then
        Result := MaintenanceCancelledText;
end;

// Waits for a running maintenance run, then warns when changes that can still be undone are
// recorded for this account, or when that could not be checked.
function InitializeUninstall(): Boolean;
var
  Code: Integer;
begin
  Result := WaitForMaintenance();
  if (not Result) or UninstallSilent() then
    exit;
  if not RunOptctl('journal pending', Code) then
    Result := UninstallAnyway(Format(CouldNotCheckText, [Code]))
  else if Code = ExitPending then
    Result := UninstallAnyway(PendingText)
  else if Code <> 0 then
    Result := UninstallAnyway(Format(CouldNotCheckText, [Code]));
end;
