; Summer packaged Windows installer (Inno Setup).
;
; Builds a single setup.exe that installs the built summer.exe per-user
; and adds the install directory to the user PATH, so `summer` is
; runnable from any shell afterward.
;
; Requires administrator is NOT needed: summer is a plain console
; application with no elevation requirement, so this installer runs
; per-user without a UAC prompt.
;
; Build with (from the repo root, after `cargo build --release --workspace`):
;   iscc /DMyAppVersion=0.1.0 packaging\windows\summer.iss
; or just `make installer`, which fills in the version from Cargo.toml.

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#define MyAppName "Summer"
#define MyAppExeName "summer.exe"
#define MyAppPublisher "summer-code contributors"
#define RepoRoot "..\.."

[Setup]
AppId={{E78ABC37-0F88-45AB-B111-8687406E1FE4}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
VersionInfoVersion={#MyAppVersion}
DefaultDirName={localappdata}\Programs\Summer
DisableProgramGroupPage=yes
; No elevation needed: summer is a normal per-user console tool.
PrivilegesRequired=lowest
OutputDir={#RepoRoot}\target\installer
OutputBaseFilename=summer-setup-{#MyAppVersion}
Compression=lzma
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#MyAppExeName}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Files]
Source: "{#RepoRoot}\target\release\summer.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Summer"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"

[Code]
const
  EnvironmentKey = 'Environment';
  WM_SETTINGCHANGE = $001A;
  SMTO_ABORTIFHUNG = $0002;

function SendMessageTimeoutA(hWnd: Longint; Msg: Longint; wParam: Longint;
  lParam: AnsiString; fuFlags, uTimeout: Longint; var lpdwResult: Longint): Longint;
  external 'SendMessageTimeoutA@user32.dll stdcall';

procedure EnvBroadcastChange;
var
  ResultCode: Longint;
begin
  SendMessageTimeoutA(HWND_BROADCAST, WM_SETTINGCHANGE, 0, 'Environment',
    SMTO_ABORTIFHUNG, 5000, ResultCode);
end;

// Adds Path to the current user's PATH, unless already present.
procedure EnvAddPath(Path: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Paths := '';

  if Pos(';' + Uppercase(Path) + ';', ';' + Uppercase(Paths) + ';') > 0 then
    exit;

  Paths := Paths + ';' + Path + ';';

  if not RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Log('Failed to add "' + Path + '" to the user PATH');
  EnvBroadcastChange;
end;

procedure EnvRemovePath(Path: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    exit;

  P := Pos(';' + Uppercase(Path) + ';', ';' + Uppercase(Paths) + ';');
  if P = 0 then
    exit;
  if P > 1 then
    Delete(Paths, P - 1, Length(Path) + 1)
  else
    Delete(Paths, P, Length(Path) + 1);

  if not RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Log('Failed to remove "' + Path + '" from the user PATH');
  EnvBroadcastChange;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    EnvAddPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    EnvRemovePath(ExpandConstant('{app}'));
end;
