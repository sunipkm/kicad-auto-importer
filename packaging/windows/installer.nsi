; NSIS installer for the KiCad Auto Importer suite: both
; `kicad-auto-importer.exe` (the folder-watching library importer,
; `crates/app`) and `bom-app.exe` (the Populate/Generate BOM Tauri app,
; `bom-app`) ship from this single installer/uninstaller rather than two
; separate ones — they're a matched pair (same shared `settings.json`,
; same publisher, same release cadence) and a user setting either up
; wants both.
;
; Built by CI (.github/workflows/test.yml) via `makensis` (installed there
; with choco, since windows-latest dropped NSIS from its preinstalled
; software). Everything platform/build-specific is passed in on the
; command line rather than hardcoded here, so this script never needs to
; know about cargo targets or where in the workspace it's being invoked
; from. Every *_PATH/OUT_FILE define below must be an absolute,
; backslash-separated Windows path: makensis chdirs to this script's own
; directory before resolving relative paths, and its `File` instruction
; has also been observed failing to find an otherwise-real file when
; given a forward-slash path — CI's invocation (PowerShell, not bash;
; see test.yml) sidesteps both by building genuine native paths itself:
;
;   makensis `
;     "-DEXE_PATH=<path to the built kicad-auto-importer.exe>" `
;     "-DICO_PATH=<path to a standalone .ico, see icon::write_ico>" `
;     "-DBOM_EXE_PATH=<path to the built bom-app.exe>" `
;     "-DPRODUCT_VERSION=<full version, e.g. 0.0.1-pre0>" `
;     "-DFILE_VERSION=<strictly-numeric X.Y.Z, e.g. 0.0.1>" `
;     "-DOUT_FILE=<path the built installer .exe should be written to>" `
;     packaging/windows/installer.nsi
;
; `BOM_EXE_PATH` needs no matching `BOM_ICO_PATH`: `bom-app.exe` already
; has its own icon baked in as a Windows resource by `tauri-build`
; (`bom-app/src-tauri/build.rs`, from `tauri.conf.json`'s `bundle.icon`),
; unlike the bare `kicad-auto-importer.exe`, which needs one supplied
; externally (`ICO_PATH`) for its Start Menu shortcut/Add-Remove-Programs
; icon. `CreateShortCut` below omits an explicit icon file for the BOM
; Tool shortcut for exactly this reason — it defaults to the target
; executable's own embedded icon.
;
; `PRODUCT_VERSION` and `FILE_VERSION` are deliberately separate: this
; project tags pre-releases like "v0.0.1-pre0" (see release.yml), but
; NSIS's `VIProductVersion` directive requires a strictly numeric
; "X.X.X.X" — so the CI step strips any "-suffix" for that one directive
; and passes the untouched tag everywhere else (Add/Remove Programs'
; displayed version, the installer UI).
;
; Installs per-user (no admin/UAC prompt) rather than to Program Files —
; these are small companion utilities for KiCad, not system services, and
; per-user install works even on locked-down machines without admin
; rights.

!include "MUI2.nsh"

!ifndef EXE_PATH
  !error "EXE_PATH must be defined, e.g. /DEXE_PATH=..\..\target\release\kicad-auto-importer.exe"
!endif
!ifndef ICO_PATH
  !error "ICO_PATH must be defined, e.g. /DICO_PATH=app.ico"
!endif
!ifndef BOM_EXE_PATH
  !error "BOM_EXE_PATH must be defined, e.g. /DBOM_EXE_PATH=..\..\target\release\bom-app.exe"
!endif
!ifndef PRODUCT_VERSION
  !define PRODUCT_VERSION "0.0.0"
!endif
!ifndef FILE_VERSION
  !define FILE_VERSION "0.0.0"
!endif
!ifndef OUT_FILE
  !define OUT_FILE "KiCadAutoImporter-Setup.exe"
!endif

Name "KiCad Auto Importer"
OutFile "${OUT_FILE}"
Unicode true
InstallDir "$LOCALAPPDATA\Programs\KiCad Auto Importer"
InstallDirRegKey HKCU "Software\KiCad Auto Importer" "InstallDir"
RequestExecutionLevel user

!define MUI_ICON "${ICO_PATH}"
!define MUI_UNICON "${ICO_PATH}"
!define MUI_FINISHPAGE_RUN "$INSTDIR\kicad-auto-importer.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch KiCad Auto Importer"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

VIProductVersion "${FILE_VERSION}.0"
VIFileVersion "${FILE_VERSION}.0"
VIAddVersionKey "ProductName" "KiCad Auto Importer"
VIAddVersionKey "ProductVersion" "${PRODUCT_VERSION}"
VIAddVersionKey "FileVersion" "${PRODUCT_VERSION}"
VIAddVersionKey "FileDescription" "KiCad Auto Importer + BOM Tool installer"
VIAddVersionKey "LegalCopyright" "Sunip K. Mukherjee"

!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\KiCadAutoImporter"

Section "Install"
  SetOutPath "$INSTDIR"
  File "${EXE_PATH}"
  File "${BOM_EXE_PATH}"
  WriteRegStr HKCU "Software\KiCad Auto Importer" "InstallDir" "$INSTDIR"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateDirectory "$SMPROGRAMS\KiCad Auto Importer"
  CreateShortCut "$SMPROGRAMS\KiCad Auto Importer\KiCad Auto Importer.lnk" \
    "$INSTDIR\kicad-auto-importer.exe"
  CreateShortCut "$SMPROGRAMS\KiCad Auto Importer\KiCad BOM Tool.lnk" \
    "$INSTDIR\bom-app.exe"
  CreateShortCut "$SMPROGRAMS\KiCad Auto Importer\Uninstall.lnk" "$INSTDIR\Uninstall.exe"

  ; HKCU (not HKLM): matches the per-user, no-admin install above — a
  ; per-user Add/Remove Programs entry is standard and fully supported.
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "KiCad Auto Importer"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\kicad-auto-importer.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${PRODUCT_VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "Sunip K. Mukherjee"
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\kicad-auto-importer.exe"
  Delete "$INSTDIR\bom-app.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\KiCad Auto Importer\KiCad Auto Importer.lnk"
  Delete "$SMPROGRAMS\KiCad Auto Importer\KiCad BOM Tool.lnk"
  Delete "$SMPROGRAMS\KiCad Auto Importer\Uninstall.lnk"
  RMDir "$SMPROGRAMS\KiCad Auto Importer"

  DeleteRegKey HKCU "Software\KiCad Auto Importer"
  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd
