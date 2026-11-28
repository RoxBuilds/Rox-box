; Rox-box installer (Inno Setup 6). Built by scripts\package-release.ps1, which passes:
;   /DAppVersion=<workspace version>   /DSourceDir=<the folder that is also zipped>   /O<output dir>
; The installer installs exactly the files of the release zip's folder, per user, without admin
; rights. The settings a player makes (victory.toml, local\) are never installed and so never
; removed by an upgrade; the uninstaller removes them only on an explicit yes. Nothing under
; %LOCALAPPDATA%\victory (caches, saves, logs) is ever touched by setup or uninstall.
; AppId below is fixed forever: changing it makes a new installer a different program.
#ifndef AppVersion
  #error Pass /DAppVersion=<version> (scripts\package-release.ps1 does).
#endif
#ifndef SourceDir
  #error Pass /DSourceDir=<the assembled release folder> (scripts\package-release.ps1 does).
#endif
#ifndef AppVersionNumeric
  #define AppVersionNumeric AppVersion
#endif

[Setup]
AppId={{B5A5868E-48F9-4735-B276-C5C0465CA198}
AppName=Rox-box
AppVersion={#AppVersion}
AppVerName=Rox-box {#AppVersion}
AppPublisher=Rox-box contributors
AppPublisherURL=https://github.com/RoxBuilds/Rox-box
AppSupportURL=https://github.com/RoxBuilds/Rox-box/issues
AppUpdatesURL=https://github.com/RoxBuilds/Rox-box/releases
VersionInfoVersion={#AppVersionNumeric}
VersionInfoProductName=Rox-box
VersionInfoDescription=Rox-box setup
; Per user, no admin rights, and only that: the launcher and the engine write their settings
; (victory.toml, local\) beside the executables, which a per-machine folder under Program Files
; would refuse to a player without admin rights.
PrivilegesRequired=lowest
DefaultDirName={autopf}\Rox-box
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
LicenseFile={#SourceDir}\LICENSE
UninstallDisplayIcon={app}\roxbox-launcher.exe
UninstallDisplayName=Rox-box
WizardStyle=modern
Compression=lzma2/ultra64
SolidCompression=yes
; Close a running launcher or engine politely before replacing files; do not restart them.
CloseApplications=yes
CloseApplicationsFilter=roxbox-launcher.exe,victory.exe
RestartApplications=no
OutputDir={#SourcePath}\..
OutputBaseFilename=Rox-box-{#AppVersion}-windows-x64-setup

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Exactly the release zip's folder. victory.toml and local\ are never part of it (and are
; excluded in case a stray one sits in the folder): they are the player's.
Source: "{#SourceDir}\*"; DestDir: "{app}"; Excludes: "victory.toml,victory.toml.bak-*,local"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Rox-box"; Filename: "{app}\roxbox-launcher.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\Rox-box"; Filename: "{app}\roxbox-launcher.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\roxbox-launcher.exe"; Description: "Launch Rox-box"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent
; The launcher's own update runs this setup quietly with /RELAUNCH=1: it is started again after it.
Filename: "{app}\roxbox-launcher.exe"; WorkingDir: "{app}"; Flags: nowait; Check: Relaunch

; No [UninstallDelete] and no [InstallDelete] on purpose: only the files listed in [Files] are
; removed, nothing the player or Rox-box wrote beside them.

[Code]
// /RELAUNCH=1 (the launcher's update): start the launcher again once the files are in place.
function Relaunch: Boolean;
begin
  Result := ExpandConstant('{param:RELAUNCH|0}') = '1';
end;

// At uninstall time, ask whether the settings in the install folder go too. The default is no,
// and a silent uninstall never removes them.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  AppDir, Local: String;
  Found: TFindRec;
begin
  if CurUninstallStep <> usUninstall then Exit;
  if UninstallSilent then Exit;
  AppDir := ExpandConstant('{app}');
  if MsgBox('Also remove my Rox-box settings in the install folder?' + #13#10 + #13#10 +
      'This removes victory.toml (and its backups) and the local folder next to Rox-box. ' +
      'Your caches, saves and logs (in %LOCALAPPDATA%\victory) are never removed.',
      mbConfirmation, MB_YESNO or MB_DEFBUTTON2) <> IDYES then Exit;
  DeleteFile(AppDir + '\victory.toml');
  if FindFirst(AppDir + '\victory.toml.bak-*', Found) then
  try
    repeat
      DeleteFile(AppDir + '\' + Found.Name);
    until not FindNext(Found);
  finally
    FindClose(Found);
  end;
  Local := AppDir + '\local';
  if DirExists(Local) then DelTree(Local, True, True, True);
end;
