; Inno Setup script for the Windows installer.
; Built in CI with: iscc /DAppVersion=1.2.3 /DSourceExe=path\to\edittr.exe /DOutputDir=dist packaging\windows\edittr.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\edittr.exe"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

[Setup]
AppId={{7B0C4E7A-5C1E-4D8E-9C55-3F2B6E1A9D41}
AppName=edittr
AppVersion={#AppVersion}
AppVerName=edittr {#AppVersion}
DefaultDirName={autopf}\edittr
DefaultGroupName=edittr
DisableProgramGroupPage=yes
; Installs per-user by default (no admin prompt); users can choose "all users".
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=edittr-{#AppVersion}-windows-x64-setup
SetupIconFile=..\..\assets\icon.ico
UninstallDisplayIcon={app}\edittr.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked
Name: "contextmenu"; Description: "Add ""Open with edittr"" to the Explorer right-click menu"
Name: "addtopath"; Description: "Add edittr to PATH (run ""edittr ."" from a terminal)"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "edittr.exe"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\edittr"; Filename: "{app}\edittr.exe"
Name: "{autodesktop}\edittr"; Filename: "{app}\edittr.exe"; Tasks: desktopicon

[Registry]
; Right-click a folder, a folder background, or a file.
Root: HKA; Subkey: "Software\Classes\Directory\shell\edittr"; ValueType: string; ValueName: ""; ValueData: "Open with edittr"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Directory\shell\edittr"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\edittr.exe"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\edittr\command"; ValueType: string; ValueName: ""; ValueData: """{app}\edittr.exe"" ""%1"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\edittr"; ValueType: string; ValueName: ""; ValueData: "Open with edittr"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\edittr"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\edittr.exe"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\edittr\command"; ValueType: string; ValueName: ""; ValueData: """{app}\edittr.exe"" ""%V"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\*\shell\edittr"; ValueType: string; ValueName: ""; ValueData: "Edit with edittr"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\*\shell\edittr"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\edittr.exe"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\*\shell\edittr\command"; ValueType: string; ValueName: ""; ValueData: """{app}\edittr.exe"" ""%1"""; Tasks: contextmenu

[Run]
Filename: "{app}\edittr.exe"; Description: "Launch edittr"; Flags: nowait postinstall skipifsilent

[Code]
// PATH handling: per-user installs edit HKCU, all-users installs edit HKLM.
function PathRoot: Integer;
begin
  if IsAdminInstallMode then Result := HKEY_LOCAL_MACHINE else Result := HKEY_CURRENT_USER;
end;

function PathKey: String;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

procedure AddToPath(Dir: String);
var Paths: String;
begin
  if not RegQueryStringValue(PathRoot, PathKey, 'Path', Paths) then Paths := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0 then exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then Paths := Paths + ';';
  RegWriteExpandStringValue(PathRoot, PathKey, 'Path', Paths + Dir);
end;

procedure RemoveFromPath(Dir: String);
var Paths: String; P: Integer;
begin
  if not RegQueryStringValue(PathRoot, PathKey, 'Path', Paths) then exit;
  P := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';');
  if P = 0 then exit;
  // P indexes the padded string; in Paths the entry's leading ';' sits at P - 1.
  if P = 1 then
    Delete(Paths, 1, Length(Dir) + 1)   // first entry: remove "Dir;"
  else
    Delete(Paths, P - 1, Length(Dir) + 1); // remove ";Dir"
  RegWriteExpandStringValue(PathRoot, PathKey, 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
