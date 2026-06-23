; Inno Setup script for scryglass. Compiled by `cargo xtask package`, which
; passes AppVersion, BinPath, and SrcRoot via /D defines.
;
; Per-user install (no admin): PrivilegesRequired=lowest + {autopf} resolves to
; %LocalAppData%\Programs\scryglass. The point of the installer is a STABLE path
; so the in-app "Open with" registration (Settings) keeps working across updates.
; File associations are left to the app, not written here.

#define AppName "scryglass"

[Setup]
AppId={{8B6C3F2A-1D4E-4A9B-9C7E-2F5A8D1B3C6E}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Cameron Kinsella
DefaultDirName={autopf}\scryglass
DefaultGroupName=scryglass
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
SetupIconFile={#SrcRoot}\assets\icon.ico
UninstallDisplayIcon={app}\scryglass.exe
LicenseFile={#SrcRoot}\LICENSE
Compression=lzma
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#BinPath}"; DestDir: "{app}"; DestName: "scryglass.exe"; Flags: ignoreversion
Source: "{#SrcRoot}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SrcRoot}\THIRD-PARTY.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SrcRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\scryglass"; Filename: "{app}\scryglass.exe"
Name: "{group}\Uninstall scryglass"; Filename: "{uninstallexe}"

[Run]
Filename: "{app}\scryglass.exe"; Description: "Launch scryglass"; Flags: nowait postinstall skipifsilent
