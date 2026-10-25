; Hush installer (Inno Setup 6). Built by scripts\release.ps1, which provides the defines below.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef IconFile
  #define IconFile "..\dist\hush.ico"
#endif

[Setup]
AppId={{6F1E2B7C-3D4A-4E59-9B8C-2A7D1F0E5C43}
AppName=Hush
AppVersion={#AppVersion}
AppVerName=Hush {#AppVersion}
AppPublisher=visiuun
AppPublisherURL=https://github.com/visiuun/hush
AppSupportURL=https://github.com/visiuun/hush/issues
AppUpdatesURL=https://github.com/visiuun/hush/releases
DefaultDirName={autopf}\Hush
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputDir=..\dist
OutputBaseFilename=Hush-Setup-{#AppVersion}
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\hush-app.exe
UninstallDisplayName=Hush
LicenseFile=..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
CloseApplications=no

[Tasks]
Name: "startup"; Description: "Start Hush when I sign in (recommended)"
Name: "path"; Description: "Add the ""hush"" command to PATH"
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\target\release\hush-app.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\hush.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\dist\PawnIO_setup.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall

[Icons]
Name: "{autoprograms}\Hush"; Filename: "{app}\hush-app.exe"
Name: "{autodesktop}\Hush"; Filename: "{app}\hush-app.exe"; Tasks: desktopicon

[Run]
Filename: "{tmp}\PawnIO_setup.exe"; Parameters: "-install -silent"; StatusMsg: "Installing the PawnIO driver..."; Check: not PawnIOInstalled; Flags: waituntilterminated
Filename: "{app}\hush-app.exe"; Parameters: "--startup-on"; StatusMsg: "Registering the startup task..."; Tasks: startup; Flags: waituntilterminated runhidden
Filename: "{app}\hush-app.exe"; Parameters: "--startup-off"; Tasks: not startup; Flags: waituntilterminated runhidden
Filename: "{app}\hush-app.exe"; Description: "Open Hush"; Flags: postinstall nowait skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{app}\hush.exe"; Parameters: "quit"; Flags: runhidden waituntilterminated; RunOnceId: "QuitHush"
Filename: "{app}\hush-app.exe"; Parameters: "--startup-off"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveStartupTask"

[Code]
const
  EnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

function PawnIOInstalled(): Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\PawnIO');
end;

function PathHas(Paths, Dir: string): Boolean;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0;
end;

procedure AddToPath(Dir: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(HKLM, EnvKey, 'Path', Paths) then
    Paths := '';
  if PathHas(Paths, Dir) then
    exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
    Paths := Paths + ';';
  RegWriteExpandStringValue(HKLM, EnvKey, 'Path', Paths + Dir);
end;

procedure RemoveFromPath(Dir: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKLM, EnvKey, 'Path', Paths) then
    exit;
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Paths));
  if P = 0 then
    exit;
  Delete(Paths, P, Length(Dir) + 1);
  Paths := Copy(Paths, 2, Length(Paths) - 2);
  RegWriteExpandStringValue(HKLM, EnvKey, 'Path', Paths);
end;

{ A running Hush keeps its files locked; ask it to exit (fans go back to the BIOS first). }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
  Cli: string;
begin
  Cli := ExpandConstant('{app}\hush.exe');
  if FileExists(Cli) then
  begin
    Exec(Cli, 'quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
    Sleep(1500);
  end;
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('path') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
