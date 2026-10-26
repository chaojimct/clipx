; clipx Inno Setup（便携 Data/ 与 exe 同级）
; 先 cargo build -p clipx-app --release，再：
;   iscc /DAppVersion=0.10.13 /DPublishDir=..\target\release scripts\clipx.iss

#ifndef AppVersion
  #define AppVersion "0.10.13"
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
; 自家单实例互斥体（见 main.rs ensure_single_instance）：安装前自动处理正在跑的 clipx，
; 不靠用户手动退托盘。注意它管不了「注入到别家对话框里」的 DLL，那是 restartreplace 的活。
AppMutex=clipx-single-instance
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
Source: "{#PublishDir}\clipx.exe"; DestDir: "{app}"; Flags: ignoreversion restartreplace
; ShellNavigate / 信标 DLL 会被注入到资源管理器与各类「打开/保存」对话框进程里：
; Restart Manager 关不掉这些宿主（关了等于杀用户应用），不加 restartreplace 就报
; "DeleteFile failed; code 5" 硬错。加了之后占着就转重启后替换，用户最多重启一次。
Source: "{#DllDir}\x64\Release\ClipboardXShellNavigate.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist restartreplace uninsrestartdelete
Source: "{#DllDir}\Win32\Release\ClipboardXShellNavigate32.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist restartreplace uninsrestartdelete
; TSF 光标信标 DLL：单独构建（cargo build -p clipx-tsf --release），产物名 clipx_caret_beacon.dll
Source: "{#PublishDir}\clipx_caret_beacon.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist restartreplace uninsrestartdelete
; x86 信标：32 位宿主（企业微信 / WPS / 腾讯会议…）由 x64 clipx 注入它、
; 再由 DLL 在宿主进程内自装 WH_GETMESSAGE 钩子（跨位数装钩子是不可能的）。
Source: "{#PublishDir}\clipx_caret_beacon32.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist restartreplace uninsrestartdelete

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

[Code]
// 安装前尽力清掉自家进程（AppMutex / CloseApplications 的补充：提权实例与 WPF
// 遗留进程可能不在 Restart Manager 视野里）。只杀自家两个进程名，不碰宿主应用；
// 宿主（资源管理器/对话框）占着的 DLL 由 restartreplace 转重启后替换。
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Exec('taskkill', '/f /im clipx.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec('taskkill', '/f /im ClipboardX-filejump.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := '';
end;
