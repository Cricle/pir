# scripts/test_select.ps1 — changed-file -> test-subset runner.
#
# The problem this solves is the BUILD/LINK phase, not the run phase:
# `cargo test --no-run` links every test binary against the same lib (269
# integration targets in tests/ as of 2026-10-27, plus the lib unit target;
# each linked binary measures ~285 MB without debuginfo), so a one-line edit
# anywhere in src/ (including an include_str! shim, a .js.txt file) re-links
# hundreds of binaries.
# scripts/test_fast.ps1 fixed the RUN phase (concurrent suites); this fixes
# the BUILD phase by asking cargo for a SUBSET of test targets:
# `cargo test --no-run --test g_ai --test ...` compiles the lib once and links
# only the requested binaries.
#
# CI does not use this mapping: CI runs `cargo nextest run --profile ci` over
# the whole suite. Use this for the local edit loop; use
# `bash scripts/prepush.sh` (all suites) before handing work off.
#
# Usage:
#   powershell -NoProfile -File scripts/test_select.ps1                     # auto: files touched in the last 15 min
#   powershell -NoProfile -File scripts/test_select.ps1 -Since 60           # wider window
#   powershell -NoProfile -File scripts/test_select.ps1 -Files src/tui/renderers.rs
#   powershell -NoProfile -File scripts/test_select.ps1 -Broad              # include tier-2 suites
#   powershell -NoProfile -File scripts/test_select.ps1 -List               # print the plan only
#   powershell -NoProfile -File scripts/test_select.ps1 -All                # ignore the mapping, full sweep
#   powershell -NoProfile -File scripts/test_select.ps1 -NoRun              # build the subset, don't run it
#
# Safety model — the mapping is deliberately FAIL-CLOSED:
#   * anything under tests/common/, Cargo.toml, Cargo.lock, src/lib.rs,
#     src/main.rs, src/main_cli/ => full run;
#   * a changed file that matches no rule => full run (never silently
#     "no tests needed");
#   * the run is always announced: the plan prints every target it will run
#     AND the number of targets it is skipping.
# Blind spots are real: the mapping is name/path-based, so a src change that
# only breaks a test whose name does not reference that area can be missed.
# Treat a green subset as "the edit is very likely fine", not as CI. Run
# scripts/test_fast.ps1 (full) before handing work off.
param(
    [string[]]$Files = @(),
    [int]$Since = 15,
    [switch]$Broad,
    [switch]$All,
    [switch]$List,
    [switch]$NoBuild,
    [switch]$NoRun,
    [switch]$NoSmoke,
    [switch]$AllowUnmapped,
    [int]$Concurrency = 0,
    [int]$Threads = 0
)

$ErrorActionPreference = "Stop"
# `powershell -File script.ps1 -Files a,b` hands the list over as ONE string
# (-File mode does not parse PS array literals), so split it back apart.
$Files = @($Files | ForEach-Object { $_ -split ',' } | Where-Object { $_ -ne '' } | ForEach-Object { $_.Trim() })
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# --- always-run smoke set --------------------------------------------------
# Cheap, broad-signal suites that are included in EVERY subset. Keep this
# list tiny: each entry costs a link (~2-4 s) plus its own run time. `cli`
# drives the real binary end to end. `pir` is the crate's own unit-test
# target, but src/ currently contains zero #[test] functions, so that target
# links a ~260 MB harness and runs 0 tests -- pure dev-loop cost. Include it
# only when the lib actually has tests (it comes back automatically then).
$libHasTests = $false
$srcDir = Join-Path $root "src"
if (Test-Path $srcDir) {
    $libHasTests = [bool](Get-ChildItem -Path $srcDir -Recurse -Filter *.rs -File |
        Select-String -Pattern '^\s*#\[(tokio::)?test\]' -List |
        Select-Object -First 1)
}
$SmokeTargets = @()
if ($libHasTests) { $SmokeTargets += "pir" }
$SmokeTargets += "cli"

# --- mapping table --------------------------------------------------------
# First matching rule wins. 'Targets' = suites that directly exercise the
# changed code (tier 1); 'Broad' = adjacent suites worth running when -Broad
# is passed. '*' = the whole sweep. @() = nothing to run (documented no-op).
$Rules = @(
    # ---- global / build surface: always full -----------------------------
    @{ Re = '(^|/)(Cargo\.toml|Cargo\.lock|rust-toolchain\.toml|rustfmt\.toml|deny\.toml|build\.rs)$'; Targets = @('*') },
    @{ Re = '^\.cargo/|^\.config/|^\.github/'; Targets = @('*') },
    @{ Re = '^src/lib\.rs$|^src/main\.rs$|^src/main_cli/'; Targets = @('*') },
    @{ Re = '^tests/common/'; Targets = @('*') },
    @{ Re = '^src/(prelude|macros)'; Targets = @('*') },
    @{ Re = '^schema|^schemas/'; Targets = @('*') },
    @{ Re = '^benches/'; Targets = @() },   # cargo test does not run criterion

    # ---- node/js shims (the include_str! .js.txt surface) ----------------
    # prelude.js.txt / script_boot / node_cjs_runtime are concatenated into
    # every JS context, so they are shim-global.
    @{ Re = '^src/extensions/api/js/(prelude|script_boot|node_cjs_runtime|node_call_helper)\.js\.txt$'
       Targets = @('node_shims', 'web_globals')
       Broad   = @('node_surface', 'differential_node', 'pi_shims', 'extensions_runtime', 'http_shims', 'sqlite_shims', 'worker_threads_real', 'tui_shims', 'coding_agent_shims', 'bcrypt_shims', 'js_stream_incremental') },
    @{ Re = '^src/extensions/api/js/(better_sqlite3|sqlite3?)_shim\.js\.txt$'
       Targets = @('sqlite_shims', 'node_shims')
       Broad   = @('pi_shims', 'node_surface', 'differential_node') },
    @{ Re = '^src/extensions/api/js/bcrypt_shim\.js\.txt$'
       Targets = @('bcrypt_shims', 'node_shims')
       Broad   = @('node_surface') },
    @{ Re = '^src/extensions/api/js/(worker_threads|worker_boot)'
       Targets = @('worker_threads_real', 'node_shims')
       Broad   = @('node_surface', 'codemode_worker_cancel', 'differential_node') },
    @{ Re = '^src/extensions/api/js/(http|https|http2|http_)'
       Targets = @('http_shims', 'node_shims')
       Broad   = @('web_globals', 'node_surface', 'differential_node') },
    @{ Re = '^src/extensions/api/js/(stream|web_streams)'
       Targets = @('js_stream_incremental', 'node_shims')
       Broad   = @('node_surface', 'differential_node', 'http_shims') },
    @{ Re = '^src/extensions/api/js/fs_'
       Targets = @('node_shims', 'fs_watch_real')
       Broad   = @('pi_shims', 'node_surface', 'differential_node', 'http_shims') },
    @{ Re = '^src/extensions/api/js/(crypto|bcrypt|tls)'
       Targets = @('node_shims', 'bcrypt_shims')
       Broad   = @('http_shims', 'node_surface', 'differential_node') },
    @{ Re = '^src/extensions/api/js/(pi_ai|pi_ai_providers|pi_ai_oauth)'
       Targets = @('g_ai', 'coding_agent_shims')
       Broad   = @('g_agent', 'pi_shims') },
    @{ Re = '^src/extensions/api/js/(pi_agent_core|pi_coding_agent)'
       Targets = @('g_agent', 'g_tools', 'coding_agent_shims')
       Broad   = @('g_cli_server', 'pi_shims') },
    @{ Re = '^src/extensions/api/js/pi_tui'
       Targets = @('g_tui', 'tui_shims')
       Broad   = @('display_parity', 'renderer_parity') },
    @{ Re = '^src/extensions/api/js/pi_codemode'
       Targets = @('g_extensions', 'extensions_runtime', 'codemode_whitelist')
       Broad   = @('codemode_worker_cancel', 'node_shims') },
    @{ Re = '^src/extensions/api/js/typebox'
       Targets = @('g_extensions', 'extensions_runtime')
       Broad   = @('node_shims', 'sdk_extensibility', 'codemode_whitelist') },
    @{ Re = '^src/extensions/api/js/'
       Targets = @('node_shims', 'node_surface')
       Broad   = @('differential_node', 'web_globals', 'extensions_runtime', 'pi_shims', 'coding_agent_shims', 'tui_shims', 'worker_threads_real', 'http_shims', 'sqlite_shims', 'bcrypt_shims', 'js_stream_incremental') },

    # ---- extensions runtime / loader -------------------------------------
    @{ Re = '^src/extensions/runtime/'
       Targets = @('extensions_runtime', 'node_shims')
       Broad   = @('node_surface', 'differential_node', 'web_globals', 'extensions_longtail', 'js_stream_incremental', 'http_shims', 'sqlite_shims', 'worker_threads_real', 'coding_agent_shims', 'tui_shims', 'pi_shims', 'npm_modules') },
    @{ Re = '^src/extensions/(loader|discover|transpile|scan|manifest|registry)'
       Targets = @('g_extensions', 'extensions_runtime')
       Broad   = @('npm_modules', 'session_build_context', 'sdk_extensibility', 'g_tools') },
    @{ Re = '^src/extensions/'
       Targets = @('g_extensions', 'extensions_runtime')
       Broad   = @('node_shims', 'node_surface', 'extensions_longtail', 'pi_shims', 'web_globals', 'widget_factories', 'renderer_parity', 'renderer_in_runtime_fix', 'tui_shims', 'sdk_extensibility') },

    # ---- tui -------------------------------------------------------------
    @{ Re = '^src/tui/(renderers|markdown|mermaid|ansi|messages|image_preview|strip_ansi)'
       Targets = @('display_parity', 'renderer_parity', 'renderer_in_runtime_fix', 'widget_factories', 'g_tui')
       Broad   = @('altscreen_parity', 'tui_bugfix_batch', 'optimization_parity', 'render_diff') },
    @{ Re = '^src/tui/(editor|keybindings|autocomplete|command_dialogs|dialogs|selector|slash|settings_menu|fuzzy|clipboard)'
       Targets = @('tui_commands_parity', 'tui_fixes_round2', 'tui_keybinding_namespace', 'editor_killring_undo', 'slash_and_scope_parity', 'g_tui')
       Broad   = @('idiom_round3', 'tui_bugfix_batch', 'ctx_actions_and_dialog_timeout') },
    @{ Re = '^src/tui/(core|terminal|altscreen|program_status|theme|app|update_check)'
       Targets = @('tui_loop_fixes', 'tui_bugfix_batch', 'altscreen_parity', 'theme_once_semantics', 'g_tui')
       Broad   = @('optimization_parity', 'render_diff', 'display_parity', 'p2_fixes') },
    @{ Re = '^src/tui/'
       Targets = @('g_tui')
       Broad   = @('display_parity', 'tui_bugfix_batch', 'tui_commands_parity', 'renderer_parity', 'widget_factories', 'altscreen_parity') },

    # ---- ai --------------------------------------------------------------
    @{ Re = '^src/ai/api/(bedrock|google_vertex)'
       Targets = @('g_ai', 'bedrock_chain_extensions', 'bedrock_p1_fixes')
       Broad   = @('ai_request_snapshots', 'provider_config_fixes', 'catalog_google_sync') },
    @{ Re = '^src/ai/api/(openai_responses|azure|copilot)'
       Targets = @('g_ai', 'openai_responses_parity')
       Broad   = @('ai_request_snapshots', 'provider_config_fixes', 'cli_parity_fixes') },
    @{ Re = '^src/ai/api/(anthropic|mistral|openai|transform|google)'
       Targets = @('g_ai', 'ai_request_snapshots')
       Broad   = @('openai_responses_parity', 'bedrock_p1_fixes', 'cli_parity_fixes', 'catalog_google_sync', 'e2e_ollama', 'e2e_qwen38max') },
    @{ Re = '^src/ai/(providers|catalog|models)'
       Targets = @('g_ai', 'catalog_cache', 'provider_registration_parity')
       Broad   = @('catalog_google_sync', 'catalog_overlay_install', 'provider_config_fixes', 'e2e_qwen38max', 'node_shims', 'ai_request_snapshots') },
    @{ Re = '^src/ai/(auth|oauth)'
       Targets = @('g_ai', 'cli')
       Broad   = @('e2e_cli_agent_session', 'provider_config_fixes', 'oauth_page_and_load') },
    @{ Re = '^src/ai/'
       Targets = @('g_ai')
       Broad   = @('ai_request_snapshots', 'openai_responses_parity', 'memory_shared', 'bedrock_p1_fixes', 'bedrock_chain_extensions', 'pi_messages_stream_fix', 'cli_parity_fixes', 'provider_config_fixes', 'telemetry_cache', 'core_round2_fixes', 'e2e_ollama', 'e2e_qwen38max') },

    # ---- agent / session -------------------------------------------------
    @{ Re = '^src/agent_session/'
       Targets = @('g_agent', 'extension_fail_cache', 'oauth_page_and_load', 'ctx_actions_and_dialog_timeout', 'session_context_cli')
       Broad   = @('g_extensions', 'g_protocol', 'p2_inbox_toctou', 'rpc_bash_record_fix', 'bash_abort_set', 'memory_shared', 'stream_persist_fixes') },
    @{ Re = '^src/agent/'
       Targets = @('g_agent')
       Broad   = @('subagent_builtin', 'subagent_runtime_isolation', 'tool_registration_fix', 'shell_totals_fix', 'codemode_whitelist', 'agent_mutation_serialization_fix', 'g_tools', 'g_extensions', 'p2_fixes') },
    @{ Re = '^src/coding_agent/'
       Targets = @('g_tools', 'g_cli_server')
       Broad   = @('shell_pool', 'truncate_tail_edge', 'tool_registration_fix', 'session_context_cli', 'cli_parity_fixes', 'codemode_whitelist', 'security_fixes', 'subagent_builtin', 'g_protocol', 'python_plugin') },
    @{ Re = '^src/compaction/'
       Targets = @('g_agent', 'dry_convergence')
       Broad   = @('p2_inbox_toctou', 'core_round2_fixes', 'g_tools', 'session_manager_ext') },

    # ---- service surface -------------------------------------------------
    @{ Re = '^src/(client|server|protocol)/'
       Targets = @('g_protocol', 'g_cli_server', 'client_server')
       Broad   = @('server_cancel_fix', 'server_session_closeout', 'client_remove_session', 'security_fixes', 'stream_persist_fixes', 'catalog_overlay_install', 'g_agent') },
    @{ Re = '^src/mcp/'
       Targets = @('mcp_units', 'mcp_strict', 'mcp_integration')
       Broad   = @('g_ai', 'g_extensions', 'g_cli_server') },
    @{ Re = '^src/plugins/'
       Targets = @('g_extensions', 'python_plugin', 'sdk_extensibility', 'npm_modules')
       Broad   = @('codemode_whitelist', 'codemode_worker_cancel', 'subagent_builtin', 'g_tools', 'mcp_integration', 'extensions_runtime') },
    @{ Re = '^src/session_manager/'
       Targets = @('g_agent', 'g_protocol', 'session_manager_ext')
       Broad   = @('memory_shared', 'stream_persist_fixes', 'p2_inbox_toctou', 'core_round2_fixes', 'session_context_cli') },
    @{ Re = '^src/sdk/'
       Targets = @('sdk_extensibility', 'g_agent')
       Broad   = @('g_extensions', 'g_tools') },
    @{ Re = '^src/config/'
       Targets = @('g_protocol', 'provider_config_fixes', 'tui_fixes_round2', 'tool_registration_fix')
       Broad   = @('g_tools', 'session_context_cli', 'display_parity', 'mcp_units', 'g_agent') },
    @{ Re = '^src/telemetry/'
       Targets = @('telemetry_cache')
       Broad   = @('g_ai', 'core_round2_fixes') },
    @{ Re = '^src/skills\.rs$'
       Targets = @('g_tools', 'g_tui')
       Broad   = @('session_context_cli', 'subagent_builtin') },
    @{ Re = '^src/system_prompt\.rs$'
       Targets = @('g_tools', 'session_context_cli')
       Broad   = @('g_agent', 'subagent_builtin') },
    @{ Re = '^src/cache_warmer\.rs$'
       Targets = @('g_agent')
       Broad   = @('core_round2_fixes', 'g_ai') }
)

# --- reverse index: tests/suites/<x>.rs -> the target that includes it -----
$suiteOwner = @{}
foreach ($tf in Get-ChildItem -Path (Join-Path $root "tests\*.rs")) {
    $target = [IO.Path]::GetFileNameWithoutExtension($tf.Name)
    foreach ($m in [regex]::Matches((Get-Content $tf.FullName -Raw), 'path\s*=\s*"suites/([a-z_0-9]+)\.rs"')) {
        $suiteOwner[$m.Groups[1].Value] = $target
    }
}

# --- which files changed? -------------------------------------------------
function Get-ChangedFiles {
    if ($Files.Count -gt 0) {
        $out = @()
        foreach ($f in $Files) {
            if ([IO.Path]::IsPathRooted($f)) {
                $out += ([IO.Path]::GetFullPath($f)).Substring($root.Length + 1)
            } else { $out += $f }
        }
        return ($out | ForEach-Object { $_ -replace '\\', '/' })
    }
    if ($Since -le 0) {
        # Whole dirty tree (modified + untracked) as reported by git.
        $g = & git -C $root status --porcelain 2>$null
        return @($g | ForEach-Object { ($_ -replace '^...', '').Trim() -replace '\\', '/' } |
            Where-Object { $_ -ne '' -and $_ -notmatch '^target' })
    }
    $cut = (Get-Date).AddMinutes(-$Since)
    $out = @()
    foreach ($d in @("src", "tests", "benches", "schemas", ".github")) {
        $p = Join-Path $root $d
        if (Test-Path $p) {
            $out += Get-ChildItem -Path $p -Recurse -File |
                Where-Object { $_.LastWriteTime -gt $cut } |
                ForEach-Object { $_.FullName.Substring($root.Length + 1) -replace '\\', '/' }
        }
    }
    foreach ($f in @("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")) {
        $p = Join-Path $root $f
        if ((Test-Path $p) -and (Get-Item $p).LastWriteTime -gt $cut) { $out += $f }
    }
    return ($out | Sort-Object -Unique)
}

# --- map ------------------------------------------------------------------
$targets = New-Object System.Collections.Generic.HashSet[string]
$unmapped = @()
$notes = @()
$full = $false
$changed = @(Get-ChangedFiles)

if ($All) {
    $full = $true
    $notes += "-All: full sweep requested"
} else {
    foreach ($c in $changed) {
        # a changed top-level test file is its own target (exact, no guessing)
        if ($c -match '^tests/([a-z_0-9]+)\.rs$') {
            [void]$targets.Add($Matches[1]); continue
        }
        # a changed suite module -> the mega-target that #[path]-includes it
        if ($c -match '^tests/suites/([a-z_0-9]+)\.rs$') {
            if ($suiteOwner.ContainsKey($Matches[1])) { [void]$targets.Add($suiteOwner[$Matches[1]]) }
            else { $unmapped += $c }
            continue
        }
        if ($c -match '^tests/') { $unmapped += $c; continue }
        $hit = $null
        foreach ($r in $Rules) {
            if ($c -match $r.Re) { $hit = $r; break }
        }
        if ($null -eq $hit) { $unmapped += $c; continue }
        if ($hit.Targets -contains '*') { $full = $true; $notes += "$c -> full (build/global surface)"; continue }
        foreach ($t in $hit.Targets) { [void]$targets.Add($t) }
        if ($Broad -and $hit.ContainsKey('Broad')) { foreach ($t in $hit.Broad) { [void]$targets.Add($t) } }
    }
}

if ($unmapped.Count -gt 0 -and -not $AllowUnmapped) {
    $full = $true
    $notes += "unmapped change(s) -> full sweep: " + ($unmapped -join ", ")
}
if ($full) {
    # Full sweep == the existing runner, exactly as before.
    Write-Host "[test_select] plan: FULL sweep (all test binaries)"
    foreach ($n in $notes) { Write-Host ("  note: " + $n) }
    if ($List) { exit 0 }
    # -ExecutionPolicy Bypass: this host's LocalMachine policy is Restricted,
    # and a child powershell.exe does not inherit the parent's -File bypass.
    $fullArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", (Join-Path $PSScriptRoot "test_fast.ps1"))
    if ($NoBuild) { $fullArgs += "-NoBuild" }
    if ($NoRun) { Write-Host "[test_select] -NoRun: not running"; exit 0 }
    if ($Concurrency -gt 0) { $fullArgs += @("-Concurrency", $Concurrency) }
    if ($Threads -gt 0) { $fullArgs += @("-Threads", $Threads) }
    & powershell @fullArgs
    exit $LASTEXITCODE
}

if (-not $NoSmoke) { foreach ($t in $SmokeTargets) { [void]$targets.Add($t) } }

# Validate every planned name against the real target list, so a typo in the
# table above degrades into "target dropped + warning" instead of a cargo
# error that aborts the whole subset build.
$knownTargets = @(Get-ChildItem -Path (Join-Path $root "tests\*.rs") | ForEach-Object { [IO.Path]::GetFileNameWithoutExtension($_.Name) }) + @("pir")
$bad = @($targets | Where-Object { $knownTargets -notcontains $_ })
foreach ($b in $bad) { [void]$targets.Remove($b) }
if ($bad.Count -gt 0) { Write-Warning ("[test_select] dropped unknown target name(s): " + ($bad -join ", ")) }

Write-Host ("[test_select] changed {0} file(s) (window: {1})" -f $changed.Count, $(if ($Since -le 0) { "git dirty tree" } else { "last $Since min" }))
foreach ($c in $changed) { Write-Host ("  changed: " + $c) }
foreach ($n in $notes) { Write-Host ("  note:    " + $n) }
if ($changed.Count -eq 0) {
    Write-Host "[test_select] nothing changed in the window; add -Since <min> or -Files <path> (or -All)."
    exit 0
}
$plan = @($targets | Sort-Object)
Write-Host ("[test_select] plan: {0} target(s), tier-{1}" -f $plan.Count, $(if ($Broad) { "1+2" } else { "1" }))
foreach ($t in $plan) { Write-Host ("  run: " + $t) }
$skipped = @($knownTargets | Where-Object { $targets -notcontains $_ })
Write-Host ("[test_select] skipping {0} of {1} test binaries (tier-2 suites: use -Broad; everything: -All)" -f $skipped.Count, $knownTargets.Count)
if ($List) { exit 0 }
if ($NoRun) { Write-Host "[test_select] -NoRun: not running"; exit 0 }

Write-Host ""
$t0 = Get-Date
$sel = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", (Join-Path $PSScriptRoot "test_fast.ps1"), "-Tests", ($plan -join ","))
if ($NoBuild) { $sel += "-NoBuild" }
if ($Concurrency -gt 0) { $sel += @("-Concurrency", $Concurrency) }
if ($Threads -gt 0) { $sel += @("-Threads", $Threads) }
& powershell @sel
$code = $LASTEXITCODE
$secs = ((Get-Date) - $t0).TotalSeconds
Write-Host ""
if ($code -eq 0) {
    Write-Host ("[test_select] subset GREEN in {0:N1}s ({1} targets)." -f $secs, $plan.Count)
    Write-Host ("[test_select] {0} test binaries were NOT built/run; run scripts/test_fast.ps1 before handoff/CI." -f $skipped.Count)
} else {
    Write-Host ("[test_select] subset RED in {0:N1}s -> the failure is real; fix it, then re-run this command." -f $secs)
}
exit $code
