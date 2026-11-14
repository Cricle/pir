# scripts/gen_merged_tests.ps1 — regenerate the merged integration-test harnesses.
#
# WHY: every tests/*.rs file is its own cargo test target, and each target
# statically links the full pir-sdk (RustPython/QuickJS/SWC). With 337
# targets the link phase dominates `cargo test --no-run`. This script folds
# every root-level test file into N generated harness files (8 by default)
# via `#[path = "..."] mod <stem>;`, so cargo builds N test binaries instead
# of 337. Cargo's test auto-discovery is disabled by `autotests = false` in
# Cargo.toml; the explicit [[test]] entries for the generated harnesses live
# in the marked block at the end of Cargo.toml and are rewritten by this
# script.
#
# Original tests/*.rs files stay in place and are not modified. The only
# generated sources are:
#   tests/zz_merged_<n>.rs      one harness per group
#   tests/zz_shadow_<stem>.rs   a copy of a `#[global_allocator]` test whose
#                               duplicate allocator declaration is stripped
#                               (the harness installs one shared allocator)
#
# EXCEPTIONS: a test binary can contain at most one #[global_allocator].
# Seven counting-allocator tests therefore each get their own harness (bins
# 1..7), and the six dhat-profiler tests share harness 8, whose crate root
# installs `static ALLOC: dhat::Alloc`. Everything else is balanced across the
# eight bins by test count.
#
# ISOLATION: a merged binary shares one process between its test functions, so
# env vars / global statics can interact. Run the result under nextest
# (process-per-test), e.g. `cargo nextest run --no-fail-fast`, not bare
# `cargo test` for the full suite.
#
# Usage: powershell -NoProfile -File scripts/gen_merged_tests.ps1 [-HarnessCount 8] [-Check]
#
# `-Check` verifies the on-disk generated files against what this script would
# write, read-only: it reports a delta per drifted artifact and exits non-zero
# when any exist. At HEAD it is red because the harnesses were hand-adjusted
# after generation (PIR-QE-002, option 2; see the final message). It is not
# part of the gate battery or CI.
param(
    [int]$HarnessCount = 8,
    [switch]$Check
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$testsDir = Join-Path $root "tests"
$cargoToml = Join-Path $root "Cargo.toml"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
function Write-NoBom {
    param([string]$Path, [string]$Text)
    [System.IO.File]::WriteAllText($Path, $Text, $utf8NoBom)
}
$beginMarker = "# BEGIN GENERATED MERGED TEST TARGETS"
$endMarker = "# END GENERATED MERGED TEST TARGETS"

# --- `-Check` drift reporting ------------------------------------------------
# `-Check` is read-only and accumulates one entry per drifted artifact instead
# of throwing on the first one (the old shape hid every later difference). It
# prints a generator-vs-on-disk delta per artifact and fails once at the end.
# Drift at HEAD is EXPECTED and tracked by PIR-QE-002: the on-disk harnesses
# were hand-adjusted after the generator last ran, so -Check stays red until
# the dedicated regeneration lane (PIR-QE-002 option 1) lands. -Check is not
# part of the gate battery or CI.
$checkFailures = [System.Collections.Generic.List[string]]::new()

function Get-ShortLine {
    param([string]$Text, [int]$Width = 96)
    if ($null -eq $Text) { return "<none>" }
    if ($Text.Length -le $Width) { return $Text }
    return ($Text.Substring(0, $Width) + "...")
}

# Harness delta summary (only when the file has any `#[path = ...]` lines, so
# the plain shadow copies and Cargo.toml skip it): module-include counts, how
# many modules are placed differently (membership overlap), line counts, and
# the first line that differs, both sides truncated.
function Format-TextDelta {
    param([string]$Expected, [string]$OnDisk)
    $expMods = ([regex]::Matches($Expected, '(?m)^#\[path\s*=')).Count
    $actMods = ([regex]::Matches($OnDisk, '(?m)^#\[path\s*=')).Count
    $exp = @($Expected -split "\r?\n")
    $act = @($OnDisk -split "\r?\n")
    $n = [Math]::Max($exp.Count, $act.Count)
    $at = 0
    $firstExp = ""
    $firstAct = ""
    for ($i = 0; $i -lt $n; $i++) {
        $e = if ($i -lt $exp.Count) { $exp[$i] } else { "<end of file>" }
        $a = if ($i -lt $act.Count) { $act[$i] } else { "<end of file>" }
        if ($e -cne $a) { $at = $i + 1; $firstExp = $e; $firstAct = $a; break }
    }
    $lines = @()
    if ($expMods -gt 0 -or $actMods -gt 0) {
        $expNames = @([regex]::Matches($Expected, '(?m)^mod ([A-Za-z0-9_]+);') | ForEach-Object { $_.Groups[1].Value })
        $actNames = @([regex]::Matches($OnDisk, '(?m)^mod ([A-Za-z0-9_]+);') | ForEach-Object { $_.Groups[1].Value })
        $onlyGen = @($expNames | Where-Object { $actNames -notcontains $_ })
        $onlyAct = @($actNames | Where-Object { $expNames -notcontains $_ })
        $lines += ("      module includes (#[path]): generator {0}, on disk {1}" -f $expMods, $actMods)
        if ($onlyGen.Count -eq 0 -and $onlyAct.Count -eq 0) {
            $lines += ("      module membership: identical ({0} modules)" -f $expNames.Count)
        } else {
            $lines += ("      module membership: {0} shared of {1}; {2} only in generator, {3} only on disk" -f ($expNames.Count - $onlyGen.Count), $expNames.Count, $onlyGen.Count, $onlyAct.Count)
        }
    }
    $lines += ("      lines: generator {0}, on disk {1}" -f $exp.Count, $act.Count)
    if ($at -gt 0) {
        $lines += ("      first difference at line {0}:" -f $at)
        $lines += ("        generator: " + (Get-ShortLine $firstExp))
        $lines += ("        on disk:   " + (Get-ShortLine $firstAct))
    }
    return ($lines -join "`r`n")
}

# --- collect root-level test files -----------------------------------------
# These root files keep their own test binary and are emitted as explicit
# [[test]] targets inside the generated block. A file lands here only when a
# merged harness cannot host it or when its own target is part of a documented
# workflow:
#   - python_session_lifecycle.rs / worker_threads_real.rs: the Python VM and
#     the worker-thread harness both need process-global state/stack that a
#     merged harness cannot provide safely.
#   - durable_memcopy_bench.rs: installs its own counting #[global_allocator]
#     and is an opt-in (#[ignore]) benchmark binary by design; it is also the
#     eighth counting-allocator file, which is why folding it would exceed the
#     HarnessCount-1 counting slots.
#   - g_durable.rs: the durable suites' dedicated link unit (`cargo check
#     --test g_durable` / `cargo test --test g_durable`). Keeping it explicit
#     stops a regeneration from ALSO folding it into a merged harness, which
#     ran its ~760 tests twice.
#   - print_native_session.rs: keeps its own binary; listing it here makes that
#     permanent instead of leaving it to the next regeneration's bin packing.
$standaloneNames = @("python_session_lifecycle.rs", "worker_threads_real.rs",
    "durable_memcopy_bench.rs", "g_durable.rs", "print_native_session.rs")
$files = Get-ChildItem -Path $testsDir -Filter *.rs -File |
    Where-Object {
        $_.Name -notlike "zz_merged_*" -and $_.Name -notlike "zz_shadow_*" -and
        $_.Name -notin $standaloneNames
    } |
    Sort-Object Name

function Get-TestCount {
    param([string]$Path, [hashtable]$Visited)
    $full = [System.IO.Path]::GetFullPath($Path)
    if ($Visited.ContainsKey($full)) { return 0 }
    $Visited[$full] = $true
    $text = Get-Content -LiteralPath $full -Raw -Encoding UTF8
    $count = ([regex]::Matches($text, '#\[(?:tokio::)?test\b')).Count
    $dir = Split-Path -Parent $full
    foreach ($m in [regex]::Matches($text, '#\[path\s*=\s*"([^"]+)"\]\s*(?:pub\s+)?mod\s+[A-Za-z0-9_]+')) {
        $child = Join-Path $dir $m.Groups[1].Value
        if (Test-Path -LiteralPath $child) { $count += Get-TestCount -Path $child -Visited $Visited }
    }
    return $count
}

$modules = foreach ($f in $files) {
    $stem = [System.IO.Path]::GetFileNameWithoutExtension($f.Name)
    if ($stem -notmatch '^[A-Za-z_][A-Za-z0-9_]*$') {
        throw "test file stem is not a valid Rust module name: $($f.Name)"
    }
    $text = Get-Content -LiteralPath $f.FullName -Raw -Encoding UTF8
    $kind = "normal"
    if ($text -match '#\[global_allocator\]') {
        if ($text -match '#\[global_allocator\]\s*\r?\n\s*static\s+[A-Za-z0-9_]+\s*:\s*dhat::Alloc') {
            $kind = "dhat"
        } else {
            $kind = "counting"
        }
    }
    [pscustomobject]@{
        Name  = $stem
        Path  = "tests/$($f.Name)"
        Tests = (Get-TestCount -Path $f.FullName -Visited @{})
        Kind  = $kind
    }
}

$counting = @($modules | Where-Object Kind -eq "counting" | Sort-Object Tests -Descending)
$dhat = @($modules | Where-Object Kind -eq "dhat" | Sort-Object Tests -Descending)
$normal = @($modules | Where-Object Kind -eq "normal" | Sort-Object Tests -Descending)

if ($counting.Count -gt $HarnessCount - 1) {
    throw "need $($counting.Count) counting-allocator harnesses but only $($HarnessCount - 1) are available; raise -HarnessCount"
}

# --- create shadow copies for dhat tests (strip the duplicate allocator) ----
$shadowFiles = @()
foreach ($m in $dhat) {
    $src = Join-Path $root $m.Path
    $text = Get-Content -LiteralPath $src -Raw -Encoding UTF8
    $stripped = [regex]::Replace($text, '(?m)^[ \t]*#\[global_allocator\][ \t]*\r?\n[ \t]*static\s+[A-Za-z0-9_]+\s*:\s*dhat::Alloc\s*=\s*dhat::Alloc\s*;\s*\r?\n', '')
    if ($stripped -eq $text) { throw "failed to strip dhat allocator from $($m.Path)" }
    $shadowRel = "tests/zz_shadow_$($m.Name).rs"
    $shadowPath = Join-Path $root $shadowRel
    # `-Check` must be READ-ONLY: compare the expected text instead of writing
    # it (the old shape rewrote every shadow copy before the comparison, so a
    # failed check still mutated the tree).
    if ($Check) {
        if (-not (Test-Path -LiteralPath $shadowPath)) {
            $checkFailures.Add("  $shadowRel - missing; the generator would write it (the stripped dhat copy).")
        } else {
            $shadowOnDisk = Get-Content -LiteralPath $shadowPath -Raw -Encoding UTF8
            if ($shadowOnDisk -ne $stripped) {
                $checkFailures.Add("  $shadowRel - differs from the stripped dhat copy the generator would write:" + "`r`n" + (Format-TextDelta -Expected $stripped -OnDisk $shadowOnDisk))
            }
        }
    } else {
        Write-NoBom $shadowPath $stripped
    }
    $shadowFiles += $shadowPath
    # #[path] is relative to the directory of the harness file (tests/).
    $m | Add-Member -NotePropertyName IncludePath -NotePropertyValue "zz_shadow_$($m.Name).rs" -Force
}
foreach ($m in $modules) {
    if (-not $m.PSObject.Properties['IncludePath']) {
        $m | Add-Member -NotePropertyName IncludePath -NotePropertyValue "$($m.Name).rs" -Force
    }
}

# --- bin packing -----------------------------------------------------------
$bins = @()
for ($i = 0; $i -lt $HarnessCount; $i++) {
    $bins += [pscustomobject]@{ Index = $i + 1; Modules = [System.Collections.Generic.List[object]]::new(); Load = 0 }
}
for ($i = 0; $i -lt $counting.Count; $i++) {
    $bins[$i].Modules.Add($counting[$i]); $bins[$i].Load += $counting[$i].Tests
}
foreach ($m in $dhat) {
    $last = $bins[$HarnessCount - 1]
    $last.Modules.Add($m); $last.Load += $m.Tests
}
foreach ($m in $normal) {
    $target = $bins | Sort-Object Load, Index | Select-Object -First 1
    $target.Modules.Add($m); $target.Load += $m.Tests
}

foreach ($b in $bins) {
    $b.Modules = @($b.Modules | Sort-Object Name)
}

# --- render harness sources ------------------------------------------------
$harnessRels = @()
foreach ($b in $bins) {
    $rel = "tests/zz_merged_$($b.Index).rs"
    $harnessRels += $rel
    $sb = [System.Text.StringBuilder]::new()
    [void]$sb.AppendLine("// @generated by scripts/gen_merged_tests.ps1 - do not edit by hand.")
    [void]$sb.AppendLine("// Merged integration-test harness $($b.Index)/$($bins.Count): $($b.Modules.Count) modules, ~$($b.Load) tests.")
    [void]$sb.AppendLine("// Original files stay in tests/; each is included as its own module namespace.")
    [void]$sb.AppendLine("//")
    [void]$sb.AppendLine("// Aggregating hundreds of standalone test crates changes lint scope: helpers used")
    [void]$sb.AppendLine("// by another group's tests are dead here, and a file may be loaded twice through")
    [void]$sb.AppendLine("// its own `mod common`. Keep production clippy strict; allow those test-only")
    [void]$sb.AppendLine("// aggregation effects inside generated harnesses.")
    [void]$sb.AppendLine("#![allow(clippy::all)]")
    [void]$sb.AppendLine("#![allow(dead_code, unused_imports, unused_variables)]")
    [void]$sb.AppendLine("")
    [void]$sb.AppendLine("// Shared test helpers (`crate::common::...` in suites/*). One copy per binary.")
    [void]$sb.AppendLine("pub mod common;")
    if ($b.Index -eq $HarnessCount -and $dhat.Count -gt 0) {
        [void]$sb.AppendLine("")
        [void]$sb.AppendLine("// The dhat profiling tests below were written as standalone binaries. They all")
        [void]$sb.AppendLine("// expect a dhat global allocator; install ONE at the harness root and use the")
        [void]$sb.AppendLine("// shadow copies (allocator declaration stripped) under zz_shadow_*.rs.")
        [void]$sb.AppendLine("#[global_allocator]")
        [void]$sb.AppendLine("static ALLOC: dhat::Alloc = dhat::Alloc;")
    }
    [void]$sb.AppendLine("")
    foreach ($m in $b.Modules) {
        [void]$sb.AppendLine("#[path = `"$($m.IncludePath)`"]")
        [void]$sb.AppendLine("mod $($m.Name);")
    }
    $target = Join-Path $root $rel
    if ($Check) {
        if (-not (Test-Path -LiteralPath $target)) {
            $checkFailures.Add("  $rel - missing; the generator would write it.")
        } else {
            $harnessOnDisk = Get-Content -LiteralPath $target -Raw
            if ($harnessOnDisk -ne $sb.ToString()) {
                $checkFailures.Add("  $rel - differs from the generated harness:" + "`r`n" + (Format-TextDelta -Expected $sb.ToString() -OnDisk $harnessOnDisk))
            }
        }
    } else {
        Write-NoBom $target $sb.ToString()
    }
}

# Remove stale generated harnesses from an earlier, larger run.
Get-ChildItem -Path $testsDir -Filter "zz_merged_*.rs" -File | ForEach-Object {
    $rel = "tests/$($_.Name)"
    if ($harnessRels -notcontains $rel) {
        if ($Check) {
            $checkFailures.Add("  $rel - no longer generated; the script would delete it (left over from an earlier, larger run).")
        } else {
            Remove-Item -LiteralPath $_.FullName -Force
        }
    }
}

# --- rewrite the [[test]] block in Cargo.toml ------------------------------
$targets = [System.Text.StringBuilder]::new()
[void]$targets.AppendLine($beginMarker)
[void]$targets.AppendLine("# Regenerated by scripts/gen_merged_tests.ps1; every root tests/*.rs file is")
[void]$targets.AppendLine("# included by exactly one harness module. autotests = false above stops")
[void]$targets.AppendLine("# cargo from also building each original file as its own target.")
foreach ($rel in $harnessRels + @($standaloneNames | ForEach-Object { "tests/$_" })) {
    $name = [System.IO.Path]::GetFileNameWithoutExtension($rel)
    [void]$targets.AppendLine("[[test]]")
    [void]$targets.AppendLine("name = `"$name`"")
    [void]$targets.AppendLine("path = `"$rel`"")
    [void]$targets.AppendLine("")
}
[void]$targets.AppendLine($endMarker)
$block = $targets.ToString()

$toml = Get-Content -LiteralPath $cargoToml -Raw -Encoding UTF8
if ($toml -match [regex]::Escape($beginMarker)) {
    $pattern = "(?s)" + [regex]::Escape($beginMarker) + ".*?" + [regex]::Escape($endMarker)
    $newToml = [regex]::Replace($toml, $pattern, $block.TrimEnd())
} else {
    $newToml = $toml.TrimEnd() + "`r`n`r`n" + $block.TrimEnd() + "`r`n"
}
if ($Check) {
    if ($newToml -ne $toml) {
        $checkFailures.Add("  Cargo.toml - the generated [[test]] block differs from what the script would write:" + "`r`n" + (Format-TextDelta -Expected $newToml -OnDisk $toml))
    }
} else {
    Write-NoBom $cargoToml $newToml
}

# --- summary ---------------------------------------------------------------
Write-Host ("[gen_merged_tests] {0} root test files -> {1} harnesses" -f $modules.Count, $bins.Count)
foreach ($b in $bins) {
    Write-Host ("  zz_merged_{0}: {1,3} modules, ~{2,4} tests" -f $b.Index, $b.Modules.Count, $b.Load)
}
Write-Host ("  exceptions: {0} counting-allocator tests (one per harness 1..{0}), {1} dhat tests (shared harness {2})" -f $counting.Count, $dhat.Count, $HarnessCount)
if ($Check) {
    if ($checkFailures.Count -gt 0) {
        Write-Host ""
        Write-Host ("[gen_merged_tests] -Check: {0} generated artifact(s) differ from what this generator would write (read-only run; nothing was modified):" -f $checkFailures.Count)
        foreach ($failure in $checkFailures) { Write-Host $failure }
        throw (@"
-Check FAILED (PIR-QE-002): $($checkFailures.Count) generated artifact(s) are not what scripts/gen_merged_tests.ps1 would write; the tree was NOT modified.
This drift is EXPECTED at HEAD: the on-disk harnesses were hand-adjusted after the generator last ran. No gate and no CI workflow runs -Check, so it is latent.
Two paths forward:
  (1) PIR-QE-002 option 1 (dedicated quiet-tree lane): run this script WITHOUT -Check, re-derive docs/test-harness-map.md, re-run the gate battery. Only this makes -Check green again (regeneration reshuffles which bin each root test file lands in).
  (2) PIR-QE-002 option 2 (this lane): option 1 deferred on purpose. Leave the tree as is and treat the deltas above as the known-drift record; do not regenerate just to silence this check.
"@).Trim()
    }
    Write-Host "[gen_merged_tests] up to date"
}
