#requires -Version 5.1
<#
.SYNOPSIS
  PGO pipeline for pir (Windows): instrumented build -> workload -> merge ->
  profile-use rebuild. Both the QuickJS C runtime (GCC gcov) and the Rust host
  (LLVM instrprof) are covered.

.DESCRIPTION
  All artifacts stay under <repo>\target\pgo\; the normal target\ tree and the
  default `cargo build` are never modified. See docs/pgo.md.

  Stage layout is identical to scripts/pgo/pgo.sh:
    stage0  clean target\pgo\{rust-prof,c-prof,out} (and product unless -KeepProduct)
    stage1  cargo +<toolchain> build --profile release-fast --bin pir --features extensions
            with RUSTFLAGS=-Cprofile-generate=... and CFLAGS=-fprofile-generate=...
    stage2  run scripts/pgo/workload.js n times with the instrumented binary
    stage3  llvm-profdata merge -> target\pgo\out\merged.profdata (GCC reads c-prof in place)
    stage4  rebuild with -Cprofile-use / -fprofile-use into target\pgo\product

  windows-gnu note: the rustup GNU target does not ship libprofiler_builtins, so
  -Cprofile-generate fails with E0463 unless -ProfilerRuntime points at a
  compatible libprofiler_rt.a (the script then uses -Zno-profiler-runtime and
  RUSTC_BOOTSTRAP=1). MSVC ships profiler_builtins but cl.exe cannot use GCC's
  -fprofile-generate; on MSVC the C half is skipped (Rust-only PGO). The local
  default MSVC install may also lack kernel32.lib / the Windows SDK, in which
  case use the GNU toolchain.

.EXAMPLE
  .\scripts\pgo\pgo.ps1 -DryRun
  .\scripts\pgo\pgo.ps1 -Stage all
  .\scripts\pgo\pgo.ps1 -Stage stage2 -Runs 5
  .\scripts\pgo\pgo.ps1 -SkipRust        # C-only PGO on windows-gnu
  .\scripts\pgo\pgo.ps1 -Toolchain stable-x86_64-pc-windows-msvc -SkipC
#>
[CmdletBinding()]
param(
    [ValidateSet('all', 'stage0', 'stage1', 'stage2', 'stage3', 'stage4')]
    [string]$Stage = 'all',

    [string]$Toolchain = $(if ($env:PIR_PGO_TOOLCHAIN) { $env:PIR_PGO_TOOLCHAIN } else { 'stable-x86_64-pc-windows-gnu' }),
    [string]$TargetTriple = $env:PIR_PGO_TARGET_TRIPLE,
    [int]$Runs = $(if ($env:PIR_PGO_RUNS) { [int]$env:PIR_PGO_RUNS } else { 3 }),
    [int]$WorkloadMs = $(if ($env:PIR_PGO_WORKLOAD_MS) { [int]$env:PIR_PGO_WORKLOAD_MS } else { 3000 }),

    [string]$MingwBin = $(if ($env:MINGW_BIN) { $env:MINGW_BIN } else { '' }),
    [string]$Cargo = $(if ($env:CARGO) { $env:CARGO } else { 'cargo' }),

    [string]$ProfilerRuntime = $env:PIR_PGO_PROFILER_RT,
    [switch]$SkipRust,
    [switch]$SkipC,
    [switch]$KeepProduct,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$PgoRoot = Join-Path $RepoRoot 'target\pgo'
$ProductDir = Join-Path $PgoRoot 'product'
$RustProf = Join-Path $PgoRoot 'rust-prof'
$CProf = Join-Path $PgoRoot 'c-prof'
$OutDir = Join-Path $PgoRoot 'out'
$Workload = Join-Path $RepoRoot 'scripts\pgo\workload.js'
$PirExe = Join-Path $ProductDir 'release-fast\pir.exe'
$Merged = Join-Path $OutDir 'merged.profdata'

if (-not $TargetTriple) {
    $TargetTriple = if ($Toolchain -like '*msvc*') { 'x86_64-pc-windows-msvc' } else { 'x86_64-pc-windows-gnu' }
}
if (-not $MingwBin) {
    $MingwBin = [IO.Path]::GetFullPath((Join-Path $RepoRoot '..\tools\mingw\mingw64\bin'))
}

if ($SkipRust) { $env:PIR_PGO_SKIP_RUST = '1' } else { $SkipRust = $env:PIR_PGO_SKIP_RUST -eq '1' }
if ($SkipC) { $env:PIR_PGO_SKIP_C = '1' } else { $SkipC = $env:PIR_PGO_SKIP_C -eq '1' }

function Write-Say([string]$Text) { Write-Host ''; Write-Host "==> $Text" -ForegroundColor Cyan }
function Write-Note([string]$Text) { Write-Host "    $Text" }
function Write-Fail([string]$Text) { throw "pgo.ps1: $Text" }

function Invoke-CommandStep {
    param([string]$Exe, [string[]]$Arguments)
    $printable = (@($Exe) + @($Arguments)) | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }
    Write-Note ("+ " + ($printable -join ' '))
    if ($DryRun) { return }
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "command failed with exit code $LASTEXITCODE`: $Exe $($Arguments -join ' ')"
    }
}

function Invoke-EnvStep {
    param([hashtable]$Environment, [scriptblock]$Body)
    $saved = @{}
    foreach ($key in $Environment.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
        [Environment]::SetEnvironmentVariable($key, [string]$Environment[$key], 'Process')
    }
    try {
        & $Body
    } finally {
        foreach ($key in $saved.Keys) {
            [Environment]::SetEnvironmentVariable($key, $saved[$key], 'Process')
        }
    }
}

function Show-Environment([hashtable]$Environment) {
    foreach ($key in ($Environment.Keys | Sort-Object)) {
        Write-Note ("{0,-30} {1}" -f $key, $Environment[$key])
    }
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

if (-not (Get-Command $Cargo -ErrorAction SilentlyContinue)) { Write-Fail "cargo not found: $Cargo" }
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) { Write-Fail 'rustup not found' }
if ($RepoRoot -match '\s') {
    Write-Fail "the repository path contains whitespace ($RepoRoot); RUSTFLAGS/CFLAGS split on whitespace. Move the checkout."
}

$gccCmd = $null
if ($Toolchain -like '*gnu*') {
    $candidateGcc = Join-Path $MingwBin 'gcc.exe'
    if (Test-Path $candidateGcc) {
        $env:PATH = $MingwBin + ';' + $env:PATH
        $gccCmd = $candidateGcc
    } else {
        $gccCmd = (Get-Command gcc.exe -ErrorAction SilentlyContinue)
        if (-not $gccCmd) {
            Write-Fail "mingw gcc.exe not found (looked in $MingwBin). Pass -MingwBin <mingw64\bin>."
        }
        $gccCmd = $gccCmd.Source
    }
}

$sysroot = (& rustup run $Toolchain rustc --print sysroot).Trim()
if ($LASTEXITCODE -ne 0) { Write-Fail "rustup run $Toolchain rustc failed" }
$rustcVersion = (& rustup run $Toolchain rustc --version).Trim()
$vV = (& rustup run $Toolchain rustc -vV) -join "`n"
$hostTriple = [regex]::Match($vV, '(?m)^host:\s*(\S+)').Groups[1].Value
$llvmVersion = [regex]::Match($vV, '(?m)^LLVM version:\s*(\S+)').Groups[1].Value
if (-not $hostTriple) { Write-Fail "could not parse host from rustc -vV`n$vV" }

$profdata = Join-Path $sysroot "lib\rustlib\$hostTriple\bin\llvm-profdata.exe"
if (-not (Test-Path $profdata)) {
    $onPath = Get-Command llvm-profdata.exe -ErrorAction SilentlyContinue
    if ($onPath) { $profdata = $onPath.Source }
    else { Write-Fail "llvm-profdata not found. Run: rustup component add llvm-tools-$hostTriple" }
}

if ($Toolchain -like '*msvc*') {
    Write-Note 'toolchain is MSVC: C/QuickJS PGO disabled (cl.exe has no -fprofile-generate)'
    $SkipC = $true
    $env:PIR_PGO_SKIP_C = '1'
}

$profilerBuiltins = @()
$runtimeArgs = @()
if (-not $SkipRust) {
    $libDir = Join-Path $sysroot "lib\rustlib\$TargetTriple\lib"
    $profilerBuiltins = @(Get-ChildItem -Path $libDir -Filter 'libprofiler_builtins-*.rlib' -File -ErrorAction SilentlyContinue)
    if ($profilerBuiltins.Count -eq 0) {
        if ($ProfilerRuntime) {
            if (-not (Test-Path $ProfilerRuntime)) { Write-Fail "ProfilerRuntime not found: $ProfilerRuntime" }
            $runtimeArgs = @('-Zno-profiler-runtime', "-Clink-arg=$([IO.Path]::GetFullPath($ProfilerRuntime))")
            $env:RUSTC_BOOTSTRAP = '1'
            Write-Note "profiler_builtins: missing; using -ProfilerRuntime $ProfilerRuntime (-Zno-profiler-runtime)"
        } else {
            Write-Fail @"
Rust PGO is not possible with $Toolchain as installed.

  $TargetTriple does not ship libprofiler_builtins (rustc would fail with
  error[E0463]: can't find crate for ``profiler_builtins``).

Pick one:
  1) Run on MSVC (Windows SDK + VS Build Tools):
       .\scripts\pgo\pgo.ps1 -Toolchain stable-x86_64-pc-windows-msvc -SkipC
     MSVC ships profiler_builtins; C/QuickJS gcov PGO is skipped.
  2) Keep windows-gnu and pass a compatible libprofiler_rt.a built from
     compiler-builtins:
       .\scripts\pgo\pgo.ps1 -ProfilerRuntime C:\path\libprofiler_rt.a
     (adds -Zno-profiler-runtime with RUSTC_BOOTSTRAP=1)
  3) C-only PGO:
       .\scripts\pgo\pgo.ps1 -SkipRust

Not starting a build that is guaranteed to fail.
"@
        }
    } else {
        Write-Note "profiler_builtins: $($profilerBuiltins[0].FullName)"
    }
}

$gcovLib = $null
$gcovLibDir = $null
if (-not $SkipC) {
    $gcovLib = Get-ChildItem -Path (Join-Path $MingwBin '..\lib') -Recurse -Filter 'libgcov.a' -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $gcovLib) { Write-Fail "libgcov.a not found below $MingwBin\..\lib; cannot link the GCC profile runtime" }
    $gcovLibDir = $gcovLib.DirectoryName
    $gccVersion = (& $gccCmd --version | Select-Object -First 1)
    Write-Note "gcov runtime: $($gcovLib.FullName)"
    Write-Note "C compiler:   $gccCmd ($gccVersion)"
}

Write-Note "repo:         $RepoRoot"
Write-Note "toolchain:    $Toolchain ($rustcVersion)"
Write-Note "LLVM:         $llvmVersion"
Write-Note "llvm-profdata: $profdata"
Write-Note "artifact dir: $PgoRoot"

# C flags. rquickjs-sys's build.rs overwrites CFLAGS on Windows; cc 1.4 also
# reads CFLAGS_<target>, which survives and is applied last.
$targetEnvTag = $TargetTriple -replace '-', '_'
$cFlagsGen = "-fprofile-generate=$($CProf -replace '\\', '/') -O3"
$cFlagsUse = "-fprofile-use=$($CProf -replace '\\', '/') -fprofile-correction -O3"
$cFlagsHyphen = "CFLAGS_$TargetTriple"
$cFlagsUnderscore = "CFLAGS_$targetEnvTag"

# Rust flags (LLVM instrprof) and GCC gcov link flags. The final link is done
# by rustc, so instrumented C objects need libgcov injected there.
$rustFlagsGen = "-Cprofile-generate=$($RustProf -replace '\\', '/') -Cllvm-args=-pgo-warn-missing-function"
$rustFlagsUse = "-Cprofile-use=$($Merged -replace '\\', '/') -Cllvm-args=-pgo-warn-missing-function"
if ($runtimeArgs.Count -gt 0) { $rustFlagsGen = "$rustFlagsGen $($runtimeArgs -join ' ')" }
$gcovLinkFlags = ''
if (-not $SkipC) {
    $gcovLinkFlags = "-Clink-arg=-L$($gcovLibDir -replace '\\', '/') -Clink-arg=-lgcov"
    $rustFlagsGen = "$rustFlagsGen $gcovLinkFlags"
}
if ($env:PIR_PGO_EXTRA_RUSTFLAGS) {
    $rustFlagsGen = "$rustFlagsGen $($env:PIR_PGO_EXTRA_RUSTFLAGS)"
    $rustFlagsUse = "$rustFlagsUse $($env:PIR_PGO_EXTRA_RUSTFLAGS)"
}

# ---------------------------------------------------------------------------
# Stages
# ---------------------------------------------------------------------------

function Invoke-Stage0 {
    Write-Say 'stage0: clean target\pgo'
    Write-Note "rust profiles: $RustProf"
    Write-Note "C profiles:    $CProf"
    Write-Note "outputs:       $OutDir"
    if ($DryRun) {
        Write-Note "+ Remove-Item -Recurse -Force $RustProf, $CProf, $OutDir"
        if (-not $KeepProduct) { Write-Note "+ Remove-Item -Recurse -Force $ProductDir" }
        return
    }
    foreach ($dir in @($RustProf, $CProf, $OutDir)) {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -Path $dir
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
    foreach ($dir in @((Join-Path $OutDir 'agent'), (Join-Path $OutDir 'workload-tmp'))) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
    if (-not $KeepProduct) {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -Path $ProductDir
    }
    Write-Note 'stage0 done'
}

function Invoke-Stage1 {
    Write-Say 'stage1: instrumented build (release-fast, features=extensions)'
    $environment = @{
        CARGO_TARGET_DIR = $ProductDir
        LLVM_PROFILE_FILE = (Join-Path $RustProf 'pir-%p-%m.profraw')
    }
    if (-not $SkipRust) {
        $environment['RUSTFLAGS'] = $rustFlagsGen
    } elseif ($gcovLinkFlags) {
        $environment['RUSTFLAGS'] = $gcovLinkFlags
    }
    if (-not $SkipC) {
        $environment['CFLAGS'] = $cFlagsGen
        $environment[$cFlagsHyphen] = $cFlagsGen
        $environment[$cFlagsUnderscore] = $cFlagsGen
        $environment['LDFLAGS'] = "-fprofile-generate=$($CProf -replace '\\', '/')"
    }
    Show-Environment $environment
    if ($SkipRust) { Write-Note 'SkipRust: omitting -Cprofile-generate (C-only PGO)' }
    if ($SkipC) { Write-Note 'SkipC: omitting GCC -fprofile-generate' }

    if ($DryRun) {
        Write-Note "+ cargo +$Toolchain build --profile release-fast --bin pir --features extensions"
        return
    }
    Invoke-EnvStep -Environment $environment -Body {
        Invoke-CommandStep -Exe $Cargo -Arguments @("+$Toolchain", 'build', '--profile', 'release-fast', '--bin', 'pir', '--features', 'extensions')
    }
    if (-not (Test-Path $PirExe)) { Write-Fail "instrumented binary missing: $PirExe" }
    Write-Note "instrumented binary: $PirExe ($((Get-Item $PirExe).Length) bytes)"
}

function Invoke-Stage2 {
    Write-Say "stage2: workload x$Runs (>= ${WorkloadMs}ms each)"
    if (-not $DryRun -and -not (Test-Path $PirExe)) { Write-Fail "missing instrumented binary $PirExe (run stage1)" }
    Write-Note "workload: $Workload"
    Write-Note "agent dir: $(Join-Path $OutDir 'agent')"
    Write-Note "scratch:   $(Join-Path $OutDir 'workload-tmp')"

    $environment = @{
        PIR_AGENT_DIR = (Join-Path $OutDir 'agent')
        PIR_PGO_TMP = (Join-Path $OutDir 'workload-tmp')
        PIR_PGO_WORKLOAD_MS = [string]$WorkloadMs
        LLVM_PROFILE_FILE = (Join-Path $RustProf 'pir-%p-%m.profraw')
    }
    Show-Environment $environment
    for ($i = 1; $i -le $Runs; $i++) {
        $log = Join-Path $OutDir "workload-$i.log"
        Write-Note "run $i/$Runs -> $log"
        if ($DryRun) {
            Write-Note "+ $PirExe js $Workload (PIR_AGENT_DIR, PIR_PGO_TMP, LLVM_PROFILE_FILE set)"
            continue
        }
        $output = Invoke-EnvStep -Environment $environment -Body {
            & $PirExe js $Workload 2>&1
        }
        $rc = $LASTEXITCODE
        $output | Set-Content -Path $log -Encoding UTF8
        if ($rc -ne 0 -or -not ($output | Where-Object { $_ -match '^PGO_WORKLOAD_OK ' })) {
            $output | Select-Object -Last 20 | ForEach-Object { Write-Host "    $_" }
            Write-Fail "workload run $i failed (exit $rc); log: $log"
        }
        $output | Where-Object { $_ -match '^PGO_WORKLOAD_OK ' } | Select-Object -Last 1 | ForEach-Object { Write-Note $_ }
    }
    if ($DryRun) { return }

    if ($env:PIR_PGO_EXTRA_JS) {
        $extra = $env:PIR_PGO_EXTRA_JS
        if (-not (Test-Path $extra)) { Write-Fail "PIR_PGO_EXTRA_JS not found: $extra" }
        Write-Note "extra JS entry: $extra"
        $extraEnv = $environment.Clone()
        $extraEnv['LLVM_PROFILE_FILE'] = (Join-Path $RustProf 'extra-%p-%m.profraw')
        $extraOutput = Invoke-EnvStep -Environment $extraEnv -Body {
            & $PirExe js $extra 2>&1
        }
        $extraOutput | Set-Content -Path (Join-Path $OutDir 'extra-js.log') -Encoding UTF8
        if ($LASTEXITCODE -ne 0) { Write-Fail "extra JS entry failed (log: $(Join-Path $OutDir 'extra-js.log'))" }
    }

    $profrawCount = @(Get-ChildItem -Path $RustProf -Recurse -Filter '*.profraw' -File -ErrorAction SilentlyContinue).Count
    $gcdaCount = @(Get-ChildItem -Path $CProf -Recurse -Filter '*.gcda' -File -ErrorAction SilentlyContinue).Count
    Write-Note "rust .profraw files: $profrawCount"
    Write-Note "gcc .gcda files:     $gcdaCount"
    if (-not $SkipRust -and $profrawCount -eq 0) {
        Write-Fail "no Rust .profraw under $RustProf; instrumented binary was not used or crashed"
    }
    if (-not $SkipC -and $gcdaCount -eq 0) {
        Write-Fail "no GCC .gcda under $CProf; CFLAGS was overridden (set $cFlagsUnderscore) or the process did not exit normally"
    }
    Write-Note 'stage2 done'
}

function Invoke-Stage3 {
    Write-Say 'stage3: merge and validate profiles'
    if (-not $SkipRust) {
        Write-Note "llvm-profdata: $profdata"
        if ($DryRun) {
            Write-Note "+ $profdata merge -o $Merged (all .profraw)"
        } else {
            $profraws = @(Get-ChildItem -Path $RustProf -Recurse -Filter '*.profraw' -File)
            if ($profraws.Count -eq 0) { Write-Fail "no .profraw files to merge under $RustProf" }
            Write-Note "merging $($profraws.Count) file(s)"
            Invoke-CommandStep -Exe $profdata -Arguments (@('merge', '-o', $Merged) + @($profraws | ForEach-Object { $_.FullName }))
            if (-not (Test-Path $Merged) -or (Get-Item $Merged).Length -eq 0) { Write-Fail "merged profile is empty: $Merged" }
            Write-Note "merged: $Merged ($((Get-Item $Merged).Length) bytes)"
            & $profdata show --detailed-summary $Merged 2>$null | Select-Object -First 12 | ForEach-Object { Write-Note $_ }
        }
    } else {
        Write-Note 'SkipRust: no Rust merge'
    }

    if (-not $SkipC) {
        Write-Note "GCC profiles stay in place; -fprofile-use=$CProf reads them directly."
        if (-not $DryRun) {
            $gcdaCount = @(Get-ChildItem -Path $CProf -Recurse -Filter '*.gcda' -File -ErrorAction SilentlyContinue).Count
            if ($gcdaCount -eq 0) { Write-Fail "no .gcda under $CProf" }
            Write-Note "gcda files: $gcdaCount"
            $gcovTool = Get-Command gcov-tool.exe -ErrorAction SilentlyContinue
            if ($gcovTool) { Write-Note "gcov-tool: $((& $gcovTool.Source --version | Select-Object -First 1))" }
        }
    }
    Write-Note 'stage3 done'
}

function Invoke-Stage4 {
    Write-Say 'stage4: optimized rebuild (profile-use)'
    if (-not $SkipRust -and -not $DryRun) {
        if (-not (Test-Path $Merged) -or (Get-Item $Merged).Length -eq 0) { Write-Fail "missing merged profile $Merged (run stage3)" }
    }
    $environment = @{ CARGO_TARGET_DIR = $ProductDir }
    if (-not $SkipRust) { $environment['RUSTFLAGS'] = $rustFlagsUse }
    if (-not $SkipC) {
        $environment['CFLAGS'] = $cFlagsUse
        $environment[$cFlagsHyphen] = $cFlagsUse
        $environment[$cFlagsUnderscore] = $cFlagsUse
    }
    Show-Environment $environment
    if ($SkipRust) { Write-Note 'SkipRust: no -Cprofile-use (C-only PGO)' }
    Write-Note "CARGO_TARGET_DIR=$ProductDir (same as stage1: GCC .gcda names encode object paths)"

    if ($DryRun) {
        Write-Note "+ cargo +$Toolchain build --profile release-fast --bin pir --features extensions"
        return
    }
    Invoke-EnvStep -Environment $environment -Body {
        Invoke-CommandStep -Exe $Cargo -Arguments @("+$Toolchain", 'build', '--profile', 'release-fast', '--bin', 'pir', '--features', 'extensions')
    }
    if (-not (Test-Path $PirExe)) { Write-Fail "PGO binary missing after build: $PirExe" }
    $pgoOut = Join-Path $OutDir 'pir-pgo.exe'
    Copy-Item -Force -Path $PirExe -Destination $pgoOut
    $sha = (Get-FileHash -Algorithm SHA256 -Path $pgoOut).Hash.ToLowerInvariant()
    Write-Note "PGO binary: $pgoOut ($((Get-Item $pgoOut).Length) bytes, sha256=$sha)"

    $gitHead = 'unknown'
    $gitDirty = 'unknown'
    try {
        $gitHead = (& git -C $RepoRoot rev-parse HEAD 2>$null).Trim()
        $porcelain = & git -C $RepoRoot status --porcelain 2>$null
        $gitDirty = if ($porcelain) { 'yes' } else { 'no' }
    } catch { }
    $profrawCount = @(Get-ChildItem -Path $RustProf -Recurse -Filter '*.profraw' -File -ErrorAction SilentlyContinue).Count
    $gcdaCount = @(Get-ChildItem -Path $CProf -Recurse -Filter '*.gcda' -File -ErrorAction SilentlyContinue).Count
    $manifest = @(
        'pir PGO manifest',
        "generated_utc=$([DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ'))",
        "git_head=$gitHead",
        "git_dirty=$gitDirty",
        "toolchain=$Toolchain",
        "rustc=$rustcVersion",
        "llvm=$llvmVersion",
        "host=$hostTriple",
        "target=$TargetTriple",
        "cpu=$($env:PROCESSOR_IDENTIFIER)",
        'profile=release-fast',
        'features=extensions',
        "rustflags_generate=$rustFlagsGen",
        "rustflags_use=$rustFlagsUse",
        "cflags_generate=$cFlagsGen",
        "cflags_use=$cFlagsUse",
        "workload_runs=$Runs",
        "workload_ms=$WorkloadMs",
        "profraw_count=$profrawCount",
        "gcda_count=$gcdaCount",
        "pir_sha256=$sha"
    )
    $manifestPath = Join-Path $OutDir 'profile-manifest.txt'
    $manifest | Set-Content -Path $manifestPath -Encoding ASCII
    Write-Note "manifest: $manifestPath"
    Write-Note 'A/B: scripts\pgo\compare.sh (Git Bash) or compare manually'
    Write-Note 'stage4 done'
}

switch ($Stage) {
    'stage0' { Invoke-Stage0 }
    'stage1' { Invoke-Stage1 }
    'stage2' { Invoke-Stage2 }
    'stage3' { Invoke-Stage3 }
    'stage4' { Invoke-Stage4 }
    'all' {
        Invoke-Stage0
        Invoke-Stage1
        Invoke-Stage2
        Invoke-Stage3
        Invoke-Stage4
    }
}