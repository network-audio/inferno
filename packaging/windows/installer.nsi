Unicode true
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "LogicLib.nsh"

!ifndef DIST_DIR
  !error "Pass -DDIST_DIR=<staging directory>"
!endif
!ifndef OUT_FILE
  !define OUT_FILE "inferno-soundcard-windows-setup.exe"
!endif
!ifndef USBIP_ARCH
  !error "Pass -DUSBIP_ARCH=x64 or arm64"
!endif
!ifndef VERSION
  !error "Pass -DVERSION=x.y.z"
!endif

Name "Inferno Sound Cards"
OutFile "${OUT_FILE}"
InstallDir "$PROGRAMFILES64\Inferno Sound Cards"
RequestExecutionLevel admin
VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "Inferno Sound Cards"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "Windows sound cards on the Dante network"
VIAddVersionKey "LegalCopyright" "GPL-3.0-or-later"

!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Open the sound card settings"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenSettings
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${DIST_DIR}\LICENSE.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function OpenSettings
  ExecShell "open" "http://127.0.0.1:47800/"
FunctionEnd

Section "Inferno Sound Cards" Main
  UserInfo::GetAccountType
  Pop $0
  ${If} $0 != "Admin"
    MessageBox MB_ICONSTOP "Administrator permission is required. Please approve the Windows elevation prompt." /SD IDOK
    Abort
  ${EndIf}
  SetRegView 64
  SetOutPath "$INSTDIR"
  ; a running service holds the executable open
  nsExec::ExecToLog 'taskkill /F /IM inferno_soundcard.exe'
  Pop $0
  File "${DIST_DIR}\inferno_soundcard.exe"
  File "${DIST_DIR}\install.ps1"
  File "${DIST_DIR}\find-usbip.ps1"
  File "${DIST_DIR}\run-soundcard.ps1"
  File "${DIST_DIR}\rotating-log.ps1"
  File "${DIST_DIR}\LICENSE.txt"
  File "${DIST_DIR}\USBIP-LICENSE.txt"
  File "${DIST_DIR}\USBip-0.9.8.1-${USBIP_ARCH}.exe"

  nsExec::ExecToLog '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\install.ps1" -Action Install'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "USB/IP installation or the service's start failed: see %TEMP%\inferno-soundcard-setup.log." /SD IDOK
    Abort
  ${EndIf}

  WriteUninstaller "$INSTDIR\Uninstall.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\InfernoSoundCards" "DisplayName" "Inferno Sound Cards"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\InfernoSoundCards" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\InfernoSoundCards" "Publisher" "Inferno"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\InfernoSoundCards" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\InfernoSoundCards" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\InfernoSoundCards" "NoRepair" 1
SectionEnd

Section "Uninstall"
  SetRegView 64
  ; usbip-win2 stays installed: other applications (e.g. Win AirPlay) may use it
  nsExec::ExecToLog '"$WINDIR\Sysnative\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "$INSTDIR\install.ps1" -Action Uninstall'
  Pop $0
  Delete "$INSTDIR\inferno_soundcard.exe"
  Delete "$INSTDIR\install.ps1"
  Delete "$INSTDIR\find-usbip.ps1"
  Delete "$INSTDIR\usbip-path.txt"
  Delete "$INSTDIR\run-soundcard.ps1"
  Delete "$INSTDIR\rotating-log.ps1"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\USBIP-LICENSE.txt"
  Delete "$INSTDIR\USBip-0.9.8.1-${USBIP_ARCH}.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\InfernoSoundCards"
SectionEnd
