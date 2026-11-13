; Fragtape installer (Inno Setup 6). Build it with installer\build.ps1.
; Per-user install: no admin prompt, same folder Fragtape always used.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6F3C2A51-8E4B-4D7A-9C1E-FA9E7B2D4C10}
AppName=Fragtape
AppVersion={#AppVersion}
AppVerName=Fragtape {#AppVersion}
AppPublisher=qtDani
AppPublisherURL=https://github.com/qtDani/fragtape
AppComments=Automatic game recording and highlights
DefaultDirName={localappdata}\Programs\Fragtape
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=Fragtape-Setup-{#AppVersion}
SetupIconFile=..\fragtape\assets\fragtape.ico
UninstallDisplayIcon={app}\Fragtape.exe
UninstallDisplayName=Fragtape
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; Fragtape is closed with --quit below, so Windows' "close apps" prompt isn't needed
CloseApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\fragtape\target\release\fragtape.exe"; DestDir: "{app}"; DestName: "Fragtape.exe"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Fragtape"; Filename: "{app}\Fragtape.exe"; Comment: "Fragtape - automatic game recording and highlights"
Name: "{userdesktop}\Fragtape"; Filename: "{app}\Fragtape.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\Fragtape.exe"; Description: "Start Fragtape"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; finish any recording and close Fragtape (and its hidden OBS) before removing it
Filename: "{app}\Fragtape.exe"; Parameters: "--quit"; Flags: waituntilterminated runhidden; RunOnceId: "QuitFragtape"

[UninstallDelete]
Type: files; Name: "{localappdata}\Fragtape\Fragtape.ico"

[Code]
// Updating: close the running copy cleanly first (it finishes a recording before closing).
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Result := '';
  if FileExists(ExpandConstant('{app}\Fragtape.exe')) then
    Exec(ExpandConstant('{app}\Fragtape.exe'), '--quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

// Uninstalling: remove "start with Windows", and optionally Fragtape's settings and cache.
// Videos in Videos\Fragtape are never touched.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'Fragtape');
    if (not UninstallSilent) and (MsgBox('Also delete Fragtape''s settings, game list and cache?' + #13#10 +
        'Your videos in Videos\Fragtape are kept either way.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
    begin
      DelTree(ExpandConstant('{userappdata}\Fragtape'), True, True, True);
      DelTree(ExpandConstant('{localappdata}\Fragtape'), True, True, True);
    end;
  end;
end;
