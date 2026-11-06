; Installer hooks for the evo-agent desktop NSIS bundle.
; Wired via tauri.conf.json bundle.windows.nsis.installerHooks.
;
; PREINSTALL runs inside the install section before any file is copied
; (this includes silent /UPDATE upgrades started by the in-app updater),
; so the rescue copy below is taken before the resources tree - which
; ships its own copy of evo-agent.toml - overwrites the deployed file.

!macro NSIS_HOOK_PREINSTALL
  ; Preserve the user's deployed agent configuration (evo-agent.toml,
  ; which may carry user-appended extension blocks) before the installer
  ; replaces the resources tree. The backup lands outside the install
  ; directory so it survives reinstalls and upgrades; it is not consumed
  ; automatically - kept for manual comparison and recovery.
  IfFileExists "$LOCALAPPDATA\evo-agent-desktop\resources\evo-agent\evo-agent.toml" 0 skip_deployed_toml_rescue
  CreateDirectory "$APPDATA\evo-agent-desktop"
  CopyFiles /SILENT "$LOCALAPPDATA\evo-agent-desktop\resources\evo-agent\evo-agent.toml" "$APPDATA\evo-agent-desktop\deployed-toml-migration-backup.toml"
  skip_deployed_toml_rescue:
!macroend

; PREUNINSTALL runs at the start of the uninstall section, before the
; packaged-file deletes. Two duties:
; 1. Back up the user's deployed agent configuration - the app-level exit
;    stash only runs when the app itself performs the update, so a manual
;    uninstall would otherwise remove the deployed toml without a backup.
; 2. Remove directories that accumulate runtime-created files (databases,
;    logs) which the packaged-file manifest does not fully cover, so no
;    non-empty directories are left behind.
!macro NSIS_HOOK_PREUNINSTALL
  IfFileExists "$LOCALAPPDATA\evo-agent-desktop\resources\evo-agent\evo-agent.toml" 0 skip_deployed_toml_uninstall_backup
  CreateDirectory "$APPDATA\evo-agent-desktop"
  CopyFiles /SILENT "$LOCALAPPDATA\evo-agent-desktop\resources\evo-agent\evo-agent.toml" "$APPDATA\evo-agent-desktop\deployed-toml-uninstall-backup.toml"
  skip_deployed_toml_uninstall_backup:
  RMDir /r "$INSTDIR\resources\console"
  RMDir /r "$INSTDIR\resources\evo-agent"
!macroend
