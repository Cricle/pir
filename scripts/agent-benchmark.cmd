@echo off
rem agent-benchmark.cmd - run the fixed pir-vs-pi agent-efficiency benchmark.
rem
rem Both tools run the SAME five tasks through the SAME OpenAI-compatible
rem endpoint (tests/agent_benchmark.rs): pir's `pir print --mode json` and
rem upstream pi's `pi --mode json -p`, both pointed at one shared models.json
rem via PI_CODING_AGENT_DIR / PIR_AGENT_DIR so model, endpoint, and thinking
rem level are identical. The test prints a per-task comparison table
rem (wall time / turns / tool calls / token usage) and enforces the parity
rem gate (both tools complete >=4 of 5 tasks). Pure batch; PowerShell only
rem for the ollama HTTP probe.
rem
rem Remote endpoint mode: set PIR_BENCH_BASE_URL (plus PIR_BENCH_API_KEY,
rem optional PIR_BENCH_PROVIDER) to skip the local ollama probes/pull and run
rem against any OpenAI-compatible gateway, e.g.:
rem   set PIR_BENCH_BASE_URL=http://api.openai.com/v1
rem   set PIR_BENCH_API_KEY=<key>
rem   set PIR_BENCH_MODEL=gpt-5

setlocal enabledelayedexpansion

set "PIR_ROOT=%~dp0.."

rem --- Tunables --------------------------------------------------------------
rem Benchmark model (default qwen3:8b; e.g. set qwen3:0.6b for a fast smoke run)
if defined PIR_BENCH_MODEL (
    set "BENCH_MODEL=%PIR_BENCH_MODEL%"
) else (
    set "BENCH_MODEL=qwen3:8b"
)
set "REMOTE_MODE="
if defined PIR_BENCH_BASE_URL set "REMOTE_MODE=1"
rem Where the upstream pi checkout lives (used for the drift/parity reference)
set "UPSTREAM=%PIR_ROOT%\..\pi"
if defined PI_UPSTREAM_DIR set "UPSTREAM=%PI_UPSTREAM_DIR%"

echo === pir-vs-pi agent benchmark ===
echo model:    !BENCH_MODEL! ^(override with PIR_BENCH_MODEL^)
if defined REMOTE_MODE (
    echo endpoint: %PIR_BENCH_BASE_URL% ^(remote gateway mode^)
) else (
    echo endpoint: http://localhost:11434 ^(local ollama^)
)
echo upstream: %UPSTREAM%
echo.

rem --- Locate the built pir binary -------------------------------------------
rem The benchmark is a cargo integration test; cargo builds the pir binary.
set "PIR_EXE=%PIR_ROOT%\target\debug\pir.exe"
if defined PIR_BENCH_PIR_EXE set "PIR_EXE=%PIR_BENCH_PIR_EXE%"
if not exist "%PIR_EXE%" (
    echo --- pir binary not found at "%PIR_EXE%", building (cargo build^) ---
    pushd "%PIR_ROOT%"
    cargo build
    if errorlevel 1 (
        popd
        echo error: cargo build failed
        exit /b 1
    )
    popd
)
if not exist "%PIR_EXE%" (
    echo error: pir binary still missing at "%PIR_EXE%"
    exit /b 1
)

rem --- Probe the endpoint -----------------------------------------------------
if defined REMOTE_MODE goto remote_probe

echo --- probing ollama on http://localhost:11434 ---
powershell -NoProfile -Command "try { $r = Invoke-WebRequest -UseBasicParsing -Uri 'http://localhost:11434/api/tags' -TimeoutSec 5; if ($r.StatusCode -eq 200) { exit 0 } else { exit 1 } } catch { exit 1 }"
if errorlevel 1 (
    echo error: ollama is not reachable on http://localhost:11434.
    echo Start it ^(e.g. "ollama serve"^) or set PIR_BENCH_BASE_URL to a remote
    echo OpenAI-compatible gateway and re-run this script.
    exit /b 1
)
echo ollama is up.
echo.

rem --- Ensure the model is pulled ---------------------------------------------
echo --- checking model "!BENCH_MODEL!" is available locally ---
set "MODEL_PRESENT="
for /f "usebackq delims=" %%L in (`powershell -NoProfile -Command "try { $tags = Invoke-RestMethod -Uri 'http://localhost:11434/api/tags' -TimeoutSec 10; foreach ($m in $tags.models) { if ($m.name -eq '!BENCH_MODEL!' -or $m.name -eq '!BENCH_MODEL!'.Split(':')[0] + ':latest') { exit 0 } }; exit 1 } catch { exit 1 }"`) do (
    set "MODEL_PRESENT=1"
)
if not defined MODEL_PRESENT (
    echo model "!BENCH_MODEL!" not found locally; running: ollama pull !BENCH_MODEL!
    ollama pull "!BENCH_MODEL!"
    if errorlevel 1 (
        echo error: ollama pull failed for "!BENCH_MODEL!".
        echo Pull it manually or point PIR_BENCH_MODEL at an already-pulled model.
        exit /b 1
    )
) else (
    echo model "!BENCH_MODEL!" is available.
)
echo.

goto endpoint_ok

:remote_probe
echo --- probing remote gateway %PIR_BENCH_BASE_URL% ---
powershell -NoProfile -Command "try { $h = @{ }; if ($env:PIR_BENCH_API_KEY) { $h.Authorization = 'Bearer ' + $env:PIR_BENCH_API_KEY }; $r = Invoke-WebRequest -UseBasicParsing -Uri ($env:PIR_BENCH_BASE_URL + '/models') -Headers $h -TimeoutSec 10; if ($r.StatusCode -eq 200) { exit 0 } else { exit 1 } } catch { exit 1 }"
if errorlevel 1 (
    echo error: remote gateway %PIR_BENCH_BASE_URL% is not reachable ^(or /models failed^).
    exit /b 1
)
echo remote gateway is up.
echo.

:endpoint_ok

rem --- Locate the pi CLI (npm global shim on PATH) ----------------------------
where pi >nul 2>nul
if errorlevel 1 (
    echo warning: "pi" not found on PATH; the parity gate needs the upstream CLI.
    echo          Install it ^(npm i -g @earendil-works/pi-coding-agent^) or set
    echo          PIR_BENCH_PI_BIN to the pi executable before re-running.
)

rem --- Run the benchmark (cargo test drives both CLIs) -------------------------
echo --- cargo test --test agent_benchmark -- --nocapture --ignored ---
echo.
pushd "%PIR_ROOT%"
set "PIR_RUN_AGENT_BENCH=1"
set "PIR_BENCH_MODEL=!BENCH_MODEL!"
cargo test --test agent_benchmark -- --nocapture --ignored
set "BENCH_EXIT=%ERRORLEVEL%"
popd

echo.
if not "%BENCH_EXIT%"=="0" (
    echo benchmark FAILED ^(exit %BENCH_EXIT%^). See the per-task reasons above;
    echo the comparison table is printed before the parity assertion.
) else (
    echo benchmark PASSED. Compare the table printed above:
    echo   - per task: wall_s / turns / calls / execs / tokens for pir vs pi
    echo   - timing is informational; correctness parity ^(both ^=^>4/5^) is the gate
    echo   - offline fixed-cost delta: cargo bench --bench benchmark
    echo     ^(system_prompt_build_fixed_run + json_event_shaping_fixed_run;
    echo     pi's Node startup is not measurable from Rust, see the bench docs^)
    echo   - upstream reference: "%UPSTREAM%"
)
endlocal & exit /b %BENCH_EXIT%
