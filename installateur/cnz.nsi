; CNz Windows installer (NSIS).
;
; Per-user installation, without administrator rights, in
; %LOCALAPPDATA%\Programs\CNZ (like VS Code): Start menu, desktop shortcut,
; right-click menu and .cnz files (cnz menu installer), entry in "Installed
; apps" for uninstalling.
;
; Build: installateur/fabriquer.sh (makensis, from Linux).

Unicode true
!ifndef VERSION
  !define VERSION "0.1.0"
!endif
!define CLE_DESINSTALLATION "Software\Microsoft\Windows\CurrentVersion\Uninstall\CNZ"

Name "CNz"
OutFile "CNz-installation-${VERSION}.exe"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\CNZ"
SetCompressor /SOLID lzma
BrandingText "CN Technology"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=1036 "ProductName" "CNz"
VIAddVersionKey /LANG=1036 "CompanyName" "CN Technology"
VIAddVersionKey /LANG=1036 "FileDescription" "Installation de CNz"
VIAddVersionKey /LANG=1036 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1036 "LegalCopyright" "CN Technology — MIT OU Apache-2.0"

!include "MUI2.nsh"
!define MUI_ICON "icone.ico"
!define MUI_UNICON "icone.ico"
!define MUI_WELCOMEPAGE_TITLE "Installation de CNz"
!define MUI_WELCOMEPAGE_TEXT "CNz compresse et extrait vos fichiers et dossiers, gratuitement.$\r$\n$\r$\nL'installation ne demande pas de droits d'administrateur. Elle ajoute CNz au menu Démarrer et au clic droit de l'Explorateur (« Compresser avec CNz », « Compresser en ZIP », « Extraire ici » sur les .cnz, .zip, .7z, .rar…).$\r$\n$\r$\nCliquez sur Suivant pour continuer."
!define MUI_FINISHPAGE_RUN "$INSTDIR\cnz-fenetre.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Ouvrir CNz"
; "Default app" box: Windows leaves this choice to the user, so its
; settings are opened on CNz (registered by "cnz menu installer").
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Ouvrir les archives (.zip, .7z, .rar…) avec CNz"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION ParDefaut

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "French"

Function ParDefaut
  ExecShell "open" "ms-settings:defaultapps?registeredAppUser=CNZ"
FunctionEnd

Section "CNz"
  SetOutPath "$INSTDIR"
  File "cnz.exe"
  File "cnz-fenetre.exe"
  File "NOTICE.txt"
  WriteUninstaller "$INSTDIR\desinstaller.exe"

  CreateShortcut "$SMPROGRAMS\CNz.lnk" "$INSTDIR\cnz-fenetre.exe" "" "$INSTDIR\cnz-fenetre.exe" 0
  CreateShortcut "$DESKTOP\CNz.lnk" "$INSTDIR\cnz-fenetre.exe" "" "$INSTDIR\cnz-fenetre.exe" 0

  ; Right-click menu and .cnz files (user registry).
  nsExec::ExecToLog '"$INSTDIR\cnz.exe" menu installer'
  Pop $0

  ; Windows "Installed apps".
  WriteRegStr HKCU "${CLE_DESINSTALLATION}" "DisplayName" "CNz"
  WriteRegStr HKCU "${CLE_DESINSTALLATION}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${CLE_DESINSTALLATION}" "Publisher" "CN Technology"
  WriteRegStr HKCU "${CLE_DESINSTALLATION}" "DisplayIcon" "$INSTDIR\cnz-fenetre.exe"
  WriteRegStr HKCU "${CLE_DESINSTALLATION}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${CLE_DESINSTALLATION}" "UninstallString" '"$INSTDIR\desinstaller.exe"'
  WriteRegStr HKCU "${CLE_DESINSTALLATION}" "QuietUninstallString" '"$INSTDIR\desinstaller.exe" /S'
  WriteRegDWORD HKCU "${CLE_DESINSTALLATION}" "NoModify" 1
  WriteRegDWORD HKCU "${CLE_DESINSTALLATION}" "NoRepair" 1
  WriteRegDWORD HKCU "${CLE_DESINSTALLATION}" "EstimatedSize" 12500

  ; Tell Explorer about the new file associations.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
SectionEnd

Section "Uninstall"
  nsExec::ExecToLog '"$INSTDIR\cnz.exe" menu desinstaller'
  Pop $0
  Delete "$SMPROGRAMS\CNz.lnk"
  Delete "$DESKTOP\CNz.lnk"
  Delete "$INSTDIR\cnz.exe"
  Delete "$INSTDIR\cnz-fenetre.exe"
  Delete "$INSTDIR\NOTICE.txt"
  Delete "$INSTDIR\desinstaller.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${CLE_DESINSTALLATION}"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
SectionEnd
