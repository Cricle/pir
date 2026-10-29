@echo off
rem check-upstream-drift.cmd - upstream drift summary for the UPDATING.md sync
rem procedure.
rem
rem Reads the recorded baseline commit from this repository's .migration-info,
rem fetches the upstream pi checkout (..\pi by default, or PI_UPSTREAM_DIR),
rem and prints the delta summary: recorded baseline, new upstream commits
rem (oneline), and the diff stat over the packages tree. Pure batch; no
rem PowerShell.

setlocal enabledelayedexpansion

set "PIR_ROOT=%~dp0.."
set "MIGRATION_INFO=%PIR_ROOT%\.migration-info"

if not exist "%MIGRATION_INFO%" (
    echo error: .migration-info not found at "%MIGRATION_INFO%"
    exit /b 1
)

rem --- Parse the recorded baseline (lines look like key=value) --------------
set "RECORDED_COMMIT="
set "RECORDED_DATE="
for /f "usebackq tokens=1,* delims==" %%A in ("%MIGRATION_INFO%") do (
    if /i "%%A"=="commit" set "RECORDED_COMMIT=%%B"
    if /i "%%A"=="date" set "RECORDED_DATE=%%B"
)
if not defined RECORDED_COMMIT (
    echo error: .migration-info has no commit= line
    exit /b 1
)
if "%RECORDED_COMMIT:~7,1%"=="" (
    echo error: recorded commit "%RECORDED_COMMIT%" does not look like a full hash
    exit /b 1
)

rem --- Locate the upstream checkout -----------------------------------------
set "UPSTREAM=%PIR_ROOT%\..\pi"
if defined PI_UPSTREAM_DIR set "UPSTREAM=%PI_UPSTREAM_DIR%"
for %%I in ("%UPSTREAM%") do set "UPSTREAM=%%~fI"
if not exist "%UPSTREAM%\.git" (
    echo error: upstream checkout not found at "%UPSTREAM%" ^(set PI_UPSTREAM_DIR^)
    exit /b 1
)

echo === upstream drift check ===
echo recorded baseline: %RECORDED_COMMIT% (%RECORDED_DATE%)
echo upstream checkout: %UPSTREAM%
echo.

rem --- Fetch upstream --------------------------------------------------------
echo --- git fetch origin ---
git -C "%UPSTREAM%" fetch origin
if errorlevel 1 (
    echo error: git fetch failed in "%UPSTREAM%"
    exit /b 1
)
echo.

rem --- Delta summary ---------------------------------------------------------
set "RANGE=%RECORDED_COMMIT%..HEAD"

echo --- git log --oneline %RANGE% (new upstream commits) ---
git -C "%UPSTREAM%" log --oneline "%RANGE%"
echo.

echo --- commit count ---
for /f %%C in ('git -C "%UPSTREAM%" rev-list --count "%RANGE%"') do set "NEW_COMMITS=%%C"
echo new upstream commits since the recorded baseline (local HEAD): !NEW_COMMITS!

rem The local checkout may lag origin/main; also report the true drift target
rem so the UPDATING.md procedure reviews everything that actually landed.
for /f %%C in ('git -C "%UPSTREAM%" rev-list --count "%RECORDED_COMMIT%..origin/main"') do set "ORIGIN_COMMITS=%%C"
echo new upstream commits since the recorded baseline (origin/main): !ORIGIN_COMMITS!
echo.

echo --- git diff --stat %RECORDED_COMMIT%..origin/main ---
git -C "%UPSTREAM%" diff --stat "%RECORDED_COMMIT%..origin/main"
echo.

echo --- drift hotspots (top-level package trees changed) ---
for /f "delims=" %%D in ('git -C "%UPSTREAM%" diff --name-only "%RECORDED_COMMIT%..origin/main"') do (
    for /f "tokens=1,2 delims=/" %%X in ("%%D") do (
        if "%%X"=="packages" echo   %%X/%%Y
    )
)
echo.

echo Next steps: follow the UPDATING.md procedure (review the full diff, map
echo changes through MAPPING.md, port tests, run the four gates, then update
echo commit/date in .migration-info).
endlocal
exit /b 0
