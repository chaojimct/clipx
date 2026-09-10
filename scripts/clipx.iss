; clipx Inno Setup（便携 Data/ 与 exe 同级）
; 先 cargo build -p clipx-app --release，再：
;   iscc /DAppVersion=0.10.2 /DPublishDir=..\target\release scripts\clipx.iss

#ifndef AppVersion
  #define AppVersion "0.10.2"
#endif
#ifndef PublishDir
  #define PublishDir "..\target\release"
#endif
; 仓库内自带（CI 打包）；本机旧路径是 ../clipboard 的 WPF 产物
#ifndef DllDir
  #define DllDir "..\native\ShellNavigate"
#endif

[Setup]
AppId={{A7C3E1B2-4D5F-6A78-9B0C-1D2E3F4A5B6C}
AppName=clipx
AppVersion={#AppVersion}
AppVerName=clipx {#AppVersion}
AppPublisher=clipx
AppPublisherURL=https://github.com/chaojimct/clipboardx
DefaultDirName={localappdata}\clipx
DefaultGroupName=clipx
DisableProgramGroupPage=yes
OutputBaseFilename=clipx-{#AppVersion}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=force
UninstallDisplayIcon={app}\clipx.exe
MinVersion=10.0

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "runonstartup"; Description: "开机自动启动"; GroupDescription: "其他选项:"

[Dirs]
Name: "{app}\Data"

[Files]
Source: "{#PublishDir}\clipx.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#DllDir}\x64\Release\ClipboardXShellNavigate.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#DllDir}\Win32\Release\ClipboardXShellNavigate32.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\clipx"; Filename: "{app}\clipx.exe"
Name: "{group}\卸载 clipx"; Filename: "{uninstallexe}"
Name: "{autodesktop}\clipx"; Filename: "{app}\clipx.exe"; Tasks: desktopicon

[Run]
; 静默安装（自动更新）也要启动新版本 → 去掉 skipifsilent，实现"更新后自动重启"。
; 安装目录在 %LocalAppData% + PrivilegesRequired=lowest，静默装无 UAC。
Filename: "{app}\clipx.exe"; Description: "启动 clipx"; Flags: nowait postinstall

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "clipx"; ValueData: """{app}\clipx.exe"""; Tasks: runonstartup; Flags: uninsdeletevalue

[UninstallRun]
Filename: "taskkill"; Parameters: "/f /im clipx.exe"; Flags: runhidden; RunOnceId: "KillClipx"
