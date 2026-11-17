; 系统安装、可选自动服务与保留用户数据的卸载。
; System installation, optional automatic service, and data-preserving uninstall.
#ifndef Version
  #error Version must be provided
#endif
#ifndef SourceRoot
  #error SourceRoot must be provided
#endif
[Setup]
AppId={{BE6952B3-6B4E-47AC-A0A9-2438DBB30B69}
AppName=ling
AppVersion={#Version}
AppPublisher=ling
DefaultDirName={autopf}\ling
DefaultGroupName=ling
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#SourceRoot}\dist
OutputBaseFilename=ling_{#Version}_windows_x86_64_setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=ling
ChangesEnvironment=yes

[Tasks]
Name: service; Description: "Register and start ling as an automatic Windows service"; Flags: checkedonce

[Dirs]
Name: "{commonappdata}\ling"; Flags: uninsneveruninstall
Name: "{commonappdata}\ling\state"; Flags: uninsneveruninstall
Name: "{commonappdata}\ling\logs"; Flags: uninsneveruninstall

[Files]
Source: "{#SourceRoot}\target\x86_64-pc-windows-msvc\release\ling.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceRoot}\target\x86_64-pc-windows-msvc\release\ling-cli.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceRoot}\target\package-tools\ling-service.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceRoot}\packaging\windows\ling-service.xml"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceRoot}\packaging\windows\check-service.ps1"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceRoot}\packaging\windows\stop-service.ps1"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceRoot}\packaging\windows\WinSW-LICENSE.txt"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#SourceRoot}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceRoot}\target\package-tools\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceRoot}\packaging\ling.toml"; DestDir: "{app}"; DestName: "ling.example.toml"; Flags: ignoreversion
Source: "{#SourceRoot}\packaging\ling.toml"; DestDir: "{commonappdata}\ling"; DestName: "ling.toml"; Flags: onlyifdoesntexist uninsneveruninstall
Source: "{#SourceRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceRoot}\docs\*.md"; DestDir: "{app}\docs"; Flags: ignoreversion

[Icons]
Name: "{group}\CLI"; Filename: "{app}\bin\ling-cli.exe"; WorkingDir: "{app}"
Name: "{group}\Documentation"; Filename: "{app}\docs\install.md"
Name: "{group}\Uninstall"; Filename: "{uninstallexe}"

[Code]
const
  EnvironmentKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';
  ProductKey = 'Software\ling';

// 所有服务和权限错误均终止安装，不把注册成功当成启动成功。
// Fail on service/permission errors; registration alone is not startup success.
procedure CheckedExec(Executable, Arguments: String);
var
  Code: Integer;
begin
  if not Exec(Executable, Arguments, '', SW_HIDE, ewWaitUntilTerminated, Code) then
    RaiseException('Cannot execute ' + Executable);
  if Code <> 0 then
    RaiseException('Command failed (' + IntToStr(Code) + '): ' + Executable);
end;

function HasService(): Boolean;
begin
  Result := RegKeyExists(HKLM, 'SYSTEM\CurrentControlSet\Services\ling');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if HasService() then begin
    if not FileExists(ExpandConstant('{app}\bin\ling-service.exe')) then begin
      Result := 'The ling service belongs to another installation. Uninstall it first.';
      exit;
    end;
    CheckedExec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + ExpandConstant('{app}\bin\stop-service.ps1') + '"');
  end;
end;

// 仅记录并移除本安装器增加的 PATH 条目，升级沿用标记。
// Track and remove only the PATH entry added here; retain the marker across upgrades.
procedure AddPath();
var
  Value, Entry, AddedText: String;
begin
  Entry := ExpandConstant('{app}\bin');
  RegQueryStringValue(HKLM, EnvironmentKey, 'Path', Value);
  if Pos(';' + Lowercase(Entry) + ';', ';' + Lowercase(Value) + ';') = 0 then begin
    AddedText := Entry;
    if (Value <> '') and (Value[Length(Value)] <> ';') then AddedText := ';' + Entry;
    if not RegWriteExpandStringValue(HKLM, EnvironmentKey, 'Path', Value + AddedText) then
      RaiseException('Cannot update system PATH');
    RegWriteStringValue(HKLM, ProductKey, 'PathAddedText', AddedText);
  end;
end;

procedure RemovePath();
var
  Value, Entry, AddedText: String;
  Position, Start, Count: Integer;
begin
  if not RegQueryStringValue(HKLM, ProductKey, 'PathAddedText', AddedText) then exit;
  Entry := ExpandConstant('{app}\bin');
  RegQueryStringValue(HKLM, EnvironmentKey, 'Path', Value);
  Position := Pos(';' + Lowercase(Entry) + ';', ';' + Lowercase(Value) + ';');
  if Position > 0 then begin
    Start := Position;
    Count := Length(Entry);
    // 精确保留原有分隔符；若随后新增 PATH 项，仅删自己的项。
    // Preserve original delimiters; if new entries followed, remove only our item.
    if (AddedText <> Entry) and (Start > 1) then begin
      Start := Start - 1;
      Count := Count + 1;
    end else if (Start + Count <= Length(Value)) then begin
      Count := Count + 1;
    end;
    Delete(Value, Start, Count);
    if not RegWriteExpandStringValue(HKLM, EnvironmentKey, 'Path', Value) then
      RaiseException('Cannot update system PATH');
  end;
  RegDeleteValue(HKLM, ProductKey, 'PathAddedText');
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  DataPath, Acl: String;
begin
  if CurStep = ssPostInstall then begin
    DataPath := ExpandConstant('{commonappdata}\ling');
    Acl := ' /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-19:(OI)(CI)RX"';
    CheckedExec(ExpandConstant('{sys}\icacls.exe'), '"' + DataPath + '"' + Acl);
    CheckedExec(ExpandConstant('{sys}\icacls.exe'), '"' + DataPath + '\ling.toml" /inheritance:r /grant:r "*S-1-5-18:F" "*S-1-5-32-544:F" "*S-1-5-19:R"');
    CheckedExec(ExpandConstant('{sys}\icacls.exe'), '"' + DataPath + '\state" /grant:r "*S-1-5-19:(OI)(CI)M"');
    CheckedExec(ExpandConstant('{sys}\icacls.exe'), '"' + DataPath + '\logs" /grant:r "*S-1-5-19:(OI)(CI)M"');
    AddPath();
    if WizardIsTaskSelected('service') or HasService() then begin
      if not HasService() then CheckedExec(ExpandConstant('{app}\bin\ling-service.exe'), 'install');
      CheckedExec(ExpandConstant('{app}\bin\ling-service.exe'), 'start');
      CheckedExec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + ExpandConstant('{app}\bin\check-service.ps1') + '"');
    end;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then begin
    if HasService() then begin
      CheckedExec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + ExpandConstant('{app}\bin\stop-service.ps1') + '"');
      CheckedExec(ExpandConstant('{app}\bin\ling-service.exe'), 'uninstall');
    end;
    RemovePath();
  end;
end;
