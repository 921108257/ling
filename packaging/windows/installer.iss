; 每用户安装，前台进程，不注册系统服务。
; Per-user installation of foreground binaries; no system service is registered.
#ifndef Version
  #error Version must be provided
#endif
#ifndef SourceRoot
  #error SourceRoot must be provided
#endif
[Setup]
AppId={{BE6952B3-6B4E-47AC-A0A9-2438DBB30B69}
AppName=Linger Cache DB
AppVersion={#Version}
AppPublisher=Linger
DefaultDirName={localappdata}\Programs\Linger Cache DB
DefaultGroupName=Linger Cache DB
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#SourceRoot}\dist
OutputBaseFilename=linger-cache-db_{#Version}_windows_x86_64_setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=Linger Cache DB

[Files]
Source: "{#SourceRoot}\target\x86_64-pc-windows-msvc\release\linger-server.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceRoot}\target\x86_64-pc-windows-msvc\release\linger-cli.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#SourceRoot}\packaging\linger.toml"; DestDir: "{app}"; DestName: "linger.example.toml"; Flags: ignoreversion
Source: "{#SourceRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceRoot}\docs\config.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#SourceRoot}\docs\persistence.md"; DestDir: "{app}\docs"; Flags: ignoreversion

[Icons]
Name: "{group}\Server"; Filename: "{app}\bin\linger-server.exe"; WorkingDir: "{app}"
Name: "{group}\CLI"; Filename: "{app}\bin\linger-cli.exe"; WorkingDir: "{app}"
Name: "{group}\Uninstall"; Filename: "{uninstallexe}"
