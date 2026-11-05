@echo off
REM Double-click to install Cortex and everything it needs (Rust, build tools, Ollama, a model).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install-windows.ps1" %*
echo.
pause
