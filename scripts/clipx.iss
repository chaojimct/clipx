; clipx Inno Setup（便携 Data/ 与 exe 同级）
; 先 cargo build -p clipx-app --release，再：
;   iscc /DAppVersion=0.10.11 /DPublishDir=..\target\release scripts\clipx.iss

#ifndef AppVersion
  #define AppVersion "0.10.11"
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
; TSF 光标信标需要 HKLM 注册 + 机器级证书信任 → 必须管理员。
; 走 ShellExec 的 runas 动词弹 UAC，clipx 本体仍保持 lowest 权限安装。
Name: "caretbeacon"; Description: "启用 TSF 光标信标（精确跟随输入光标，需管理员权限）"; GroupDescription: "其他选项:"

[Dirs]
Name: "{app}\Data"

[Files]
Source: "{#PublishDir}\clipx.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#DllDir}\x64\Release\ClipboardXShellNavigate.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#DllDir}\Win32\Release\ClipboardXShellNavigate32.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; TSF 光标信标 DLL：单独构建（cargo build -p clipx-tsf --release），产物名 clipx_caret_beacon.dll
Source: "{#PublishDir}\clipx_caret_beacon.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "register_beacon_hklm.bat"; DestDir: "{app}"; Flags: ignoreversion
Source: "unregister_beacon_hklm.bat"; DestDir: "{app}"; Flags: ignoreversion
Source: "make_beacon_cert.ps1"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\clipx"; Filename: "{app}\clipx.exe"
Name: "{group}\卸载 clipx"; Filename: "{uninstallexe}"
Name: "{autodesktop}\clipx"; Filename: "{app}\clipx.exe"; Tasks: desktopicon

[Run]
; 静默安装（自动更新）也要启动新版本 → 去掉 skipifsilent，实现"更新后自动重启"。
; 安装目录在 %LocalAppData% + PrivilegesRequired=lowest，静默装无 UAC。
Filename: "{app}\clipx.exe"; Description: "启动 clipx"; Flags: nowait postinstall
; 注册 TSF 光标信标：需要管理员，runas 弹 UAC。静默安装时跳过（不打扰自动更新流程）。
Filename: "{app}\register_beacon_hklm.bat"; Parameters: ""{app}\clipx_caret_beacon.dll"""; \
  WorkingDir: "{app}"; Tasks: caretbeacon; Flags: runas shellexec waituntilterminated skipifsilent

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "clipx"; ValueData: """{app}\clipx.exe"""; Tasks: runonstartup; Flags: uninsdeletevalue

[UninstallRun]
Filename: "taskkill"; Parameters: "/f /im clipx.exe"; Flags: runhidden; RunOnceId: "KillClipx"
; 卸载信标注册（管理员）
Filename: "{app}\unregister_beacon_hklm.bat"; WorkingDir: "{app}"; Flags: runas shellexec waituntilterminated; RunOnceId: "UnregBeacon"
