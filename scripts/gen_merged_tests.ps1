# scripts/gen_merged_tests.ps1 — regenerate the merged integration-test harnesses.
#
# WHY: every tests/*.rs file is its own cargo test target, and each target
# statically links the full pir-sdk (RustPython/QuickJS/SWC). With ~340
# targets the link phase dominates `cargo test --no-run`. This script folds
# every root-level test file into N generated harness files (8 by default)
# via `#[path = "..."] mod <stem>;`, so cargo builds N test binaries instead
# of ~340. Cargo's test auto-discovery is disabled by `autotests = false` in
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

# --- collect root-level test files -----------------------------------------
$files = Get-ChildItem -Path $testsDir -Filter *.rs -File |
    Where-Object { $_.Name -notlike "zz_merged_*" -and $_.Name -notlike "zz_shadow_*" } |
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
    Write-NoBom $shadowPath $stripped
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
        if (-not (Test-Path -LiteralPath $target) -or (Get-Content -LiteralPath $target -Raw) -ne $sb.ToString()) {
            throw "generated harness is stale: $rel (run without -Check)"
        }
    } else {
        Write-NoBom $target $sb.ToString()
    }
}

# Remove stale generated harnesses from an earlier, larger run.
Get-ChildItem -Path $testsDir -Filter "zz_merged_*.rs" -File | ForEach-Object {
    $rel = "tests/$($_.Name)"
    if ($harnessRels -notcontains $rel) {
        if ($Check) { throw "stale generated harness: $rel" }
        Remove-Item -LiteralPath $_.FullName -Force
    }
}

# --- rewrite the [[test]] block in Cargo.toml ------------------------------
$targets = [System.Text.StringBuilder]::new()
[void]$targets.AppendLine($beginMarker)
[void]$targets.AppendLine("# Regenerated by scripts/gen_merged_tests.ps1; every root tests/*.rs file is")
[void]$targets.AppendLine("# included by exactly one harness module. autotests = false above stops")
[void]$targets.AppendLine("# cargo from also building each original file as its own target.")
foreach ($rel in $harnessRels) {
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
    if ($newToml -ne $toml) { throw "Cargo.toml generated block is stale (run without -Check)" }
} else {
    Write-NoBom $cargoToml $newToml
}

# --- summary ---------------------------------------------------------------
Write-Host ("[gen_merged_tests] {0} root test files -> {1} harnesses" -f $modules.Count, $bins.Count)
foreach ($b in $bins) {
    Write-Host ("  zz_merged_{0}: {1,3} modules, ~{2,4} tests" -f $b.Index, $b.Modules.Count, $b.Load)
}
Write-Host ("  exceptions: {0} counting-allocator tests (one per harness 1..{0}), {1} dhat tests (shared harness {2})" -f $counting.Count, $dhat.Count, $HarnessCount)
if ($Check) { Write-Host "[gen_merged_tests] up to date" }
