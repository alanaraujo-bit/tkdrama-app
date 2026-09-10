@echo off
title tkdrama instalador
cd /d D:\PROJETOS\tkdrama-app
call npx tauri build -- --bin tkdrama-app > .diag\tauri-build.log 2>&1
echo === BUILD ACABOU (exit %errorlevel%) — log em .diag\tauri-build.log ===
pause
