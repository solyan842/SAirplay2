@echo off
setlocal
rem This process-only opt-in affects exactly the named endpoint on port 52266.
set "SAIRPLAY_MITV_NATIVE_NTP_ENDPOINT=192.168.88.15:52266"
if not "%~1"=="" set "SAIRPLAY_MITV_NATIVE_NTP_ENDPOINT=%~1:52266"
start "" "%~dp0sairplay-gui.exe"
endlocal
