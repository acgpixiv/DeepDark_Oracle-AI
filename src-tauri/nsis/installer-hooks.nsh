; DeepDark_Oracle NSIS installer hooks
; ---------------------------------------------------------------------------
; Notes (ASCII only on purpose - NSIS parses .nsh by the ANSI codepage):
;  1) bundle.resources uses the map form, so runtime dependencies land directly
;     in $INSTDIR: tools\bin\ffmpeg.exe. The Tauri/WebView2 and MinGW runtime
;     DLLs ship with the exe itself, so no stale copies are bundled here.
;     Models are user-provided and are never included in the installer.
;  2) The CUDA runtime is large and only needed on NVIDIA machines. It is
;     bundled to $INSTDIR\_up_\.tools\cuda-runtime and copied next to the exe
;     after install when present.
;  3) $INSTDIR\plugins is the reserved extension point. Uninstall keeps it when
;     the user put plugins inside.

!macro NSIS_HOOK_PREINSTALL
  ; Drop stale runtime DLLs from a previous install to avoid version mixing.
  Delete "$INSTDIR\cublas64_*.dll"
  Delete "$INSTDIR\cublasLt64_*.dll"
  Delete "$INSTDIR\cudart64_*.dll"
  Delete "$INSTDIR\curand64_*.dll"
  Delete "$INSTDIR\nvrtc64_*.dll"
  Delete "$INSTDIR\nvrtc-builtins64_*.dll"
  Delete "$INSTDIR\nvJitLink_*.dll"
  Delete "$INSTDIR\vcomp140.dll"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; CUDA runtime: copy only when the files were actually bundled.
  IfFileExists "$INSTDIR\_up_\.tools\cuda-runtime\*.dll" 0 +2
    CopyFiles /SILENT "$INSTDIR\_up_\.tools\cuda-runtime\*.dll" "$INSTDIR"

  ; Reserved plugin / extension directory + a short readme for discoverability.
  CreateDirectory "$INSTDIR\plugins"
  FileOpen $0 "$INSTDIR\plugins\README.txt" w
  FileWrite $0 "DeepDark_Oracle plugins directory$\r$\n"
  FileWrite $0 "Drop a plugin folder or archive here. Uninstall keeps this directory.$\r$\n"
  FileClose $0

  ; Remember the layout so launchers / diagnostics can detect an installed copy.
  WriteRegStr SHCTX "Software\DeepDark_Oracle" "InstallDir" "$INSTDIR"
  WriteRegStr SHCTX "Software\DeepDark_Oracle" "PluginsDir" "$INSTDIR\plugins"
  WriteRegStr SHCTX "Software\DeepDark_Oracle" "Layout" "nsis"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Remove runtime DLLs only. Plugins and user data are intentionally kept.
  Delete "$INSTDIR\cublas64_*.dll"
  Delete "$INSTDIR\cublasLt64_*.dll"
  Delete "$INSTDIR\cudart64_*.dll"
  Delete "$INSTDIR\curand64_*.dll"
  Delete "$INSTDIR\nvrtc64_*.dll"
  Delete "$INSTDIR\nvrtc-builtins64_*.dll"
  Delete "$INSTDIR\nvJitLink_*.dll"
  Delete "$INSTDIR\vcomp140.dll"
  DeleteRegValue SHCTX "Software\DeepDark_Oracle" "InstallDir"
  DeleteRegValue SHCTX "Software\DeepDark_Oracle" "PluginsDir"
  DeleteRegValue SHCTX "Software\DeepDark_Oracle" "Layout"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; RMDir only removes an empty directory, so user plugins survive.
  RMDir "$INSTDIR\plugins"
  RMDir "$INSTDIR"
!macroend
