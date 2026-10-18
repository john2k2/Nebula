@echo off
setlocal
set "XUPER_DATA_DIR=%~dp0analysis\integration-data"
if exist "%~dp0dist\data\session.json" set "XUPER_DATA_DIR=%~dp0dist\data"
if not exist "%~dp0dist\Nebula-Windows\Nebula.exe" (
    echo No se encontro la aplicacion en dist\Nebula-Windows.
    echo Completa el paquete portable antes de usar este acceso.
    pause
    exit /b 1
)
start "" "%~dp0dist\Nebula-Windows\Nebula.exe" %*
