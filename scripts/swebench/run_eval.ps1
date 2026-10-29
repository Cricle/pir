#requires -Version 5.1
<#
.SYNOPSIS
  Reproducible local SWE-bench evaluation harness for one instance.

.DESCRIPTION
  Flow (all git operations happen inside the container / copied testbed only):
    1. Export the instance record from the local parquet via instance_meta.py.
    2. docker create the official eval image and docker cp /testbed out to a
       Windows working copy (the .git directory is preserved).
    3. Start a fresh container with the working copy bind-mounted at /testbed
       and the run directory mounted at /pir-eval.
    4. Inside: git checkout -f <base_commit>, git apply model patch (unless
       -PatchPath is empty), git apply test patch.
    5. Start a hermetic local httpbin (httpbin 0.10.2, ports 80/443, local CA)
       and run pytest with the official command and a container-side timeout.
    6. Parse PASSED/FAILED/ERROR lines, compare with FAIL_TO_PASS/PASS_TO_PASS,
       write result.json.

  The working copy keeps LF bytes (docker cp does not convert line endings);
  git core.autocrlf is disabled inside the container so git apply is not
  affected by the Windows host. Patch files are normalized to LF copies.

  Why a local httpbin: public httpbin.org in 2026 rejects
  /redirect-to?url=http://www.google.co.uk (403; only example.com/example.net
  are allowed), and requests 2.4's bundled 2014 cacert.pem cannot validate
  today's httpbin.org certificate. Both break PASS_TO_PASS tests that are
  unrelated to the code under test. The local httpbin + local CA restores the
  historical behavior hermetically (httpbin.org and www.google.co.uk are
  resolved to 127.0.0.1 with --add-host; no test files are modified).

.PARAMETER Instance
  SWE-bench instance_id, e.g. psf__requests-2317.

.PARAMETER PatchPath
  Model patch file. Empty (default) means a null patch: no model patch is
  applied. The sha256 in the result is computed over the original file bytes.

.PARAMETER WorkDir
  Run directory (default: logs/swebench/work/<instance>/<timestamp>). Must not
  already contain a testbed/ directory.

.PARAMETER Image
  Docker image. Default: swebench/sweb.eval.x86_64.<instance with __ -> _1776_>:latest,
  falling back to the docker.1panel.live/ mirror tag if that is what is local.

.PARAMETER MetaPath
  Instance JSON. Default logs/swebench/meta/<instance>.json is refreshed from
  SWE-bench Verified on every run; an explicitly supplied path is used as-is.

.PARAMETER PythonExe
  Host Python used to run instance_meta.py.

.PARAMETER TimeoutSec
  pytest timeout in seconds (default 900), enforced inside the container.

.PARAMETER KeepContainer
  Skip cleanup of the pir-swe-* containers (debugging only).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Instance,

    [string]$PatchPath = "",

    [string]$WorkDir = "",

    [string]$Image = "",

    [string]$MetaPath = "",

    [string]$PythonExe = "C:\wp\empty\tools\python312\python.exe",

    [int]$TimeoutSec = 900,

    [switch]$KeepContainer
)

$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

function Write-Step {
    param([string]$Message)
    Write-Host ("[{0:yyyy-MM-dd HH:mm:ss}] {1}" -f (Get-Date), $Message)
}

function Write-TextFile {
    param([string]$Path, [string]$Text)
    $normalized = $Text -replace "`r`n", "`n"
    [System.IO.File]::WriteAllText($Path, $normalized, $Utf8NoBom)
}

function Invoke-Docker {
    param([string[]]$Arguments)
    # Native stderr lines must not trip $ErrorActionPreference = Stop.
    $old = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $output = New-Object System.Collections.Generic.List[string]
    try {
        & docker @Arguments 2>&1 | ForEach-Object {
            # Empty native stderr lines arrive as ErrorRecords whose ToString()
            # is the type name; Exception.Message holds the real text.
            $line = if ($_ -is [System.Management.Automation.ErrorRecord]) { $_.Exception.Message } else { $_.ToString() }
            $output.Add($line)
            Write-Host $line
        }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $old
    }
    return [pscustomobject]@{ ExitCode = $code; Output = $output.ToArray() }
}

if (-not (Test-Path -LiteralPath $PythonExe)) {
    $py = Get-Command python -ErrorAction SilentlyContinue
    if (-not $py) { throw "Python not found: $PythonExe (pass -PythonExe)" }
    $PythonExe = $py.Source
}

# ---------------------------------------------------------------- metadata
$explicitMeta = -not [string]::IsNullOrWhiteSpace($MetaPath)
if (-not $explicitMeta) {
    $MetaPath = Join-Path $RepoRoot "logs\swebench\meta\$Instance.json"
} elseif (-not [System.IO.Path]::IsPathRooted($MetaPath)) {
    $MetaPath = Join-Path $RepoRoot $MetaPath
}
# Refresh the default cache from the current dataset on every run; a
# user-supplied -MetaPath is used as-is.
if (-not $explicitMeta -or -not (Test-Path -LiteralPath $MetaPath)) {
    Write-Step "exporting instance metadata (SWE-bench Verified) -> $MetaPath"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $MetaPath) | Out-Null
    & $PythonExe (Join-Path $PSScriptRoot "instance_meta.py") --instance $Instance --out $MetaPath
    if ($LASTEXITCODE -ne 0) { throw "instance_meta.py failed with exit code $LASTEXITCODE" }
}
$meta = Get-Content -LiteralPath $MetaPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ($meta.instance_id -ne $Instance) {
    throw "metadata instance mismatch: $($meta.instance_id) != $Instance"
}

# ------------------------------------------------------------------- image
if (-not $Image) {
    $Image = "swebench/sweb.eval.x86_64." + ($Instance -replace "__", "_1776_") + ":latest"
}
$candidates = @($Image)
if ($Image -notmatch '^[^/]*\.[^/]*/') {
    $candidates += "docker.1panel.live/$Image"
}
$resolvedImage = $null
$imageId = $null
foreach ($candidate in $candidates) {
    $probe = Invoke-Docker @("image", "inspect", "--format", "{{.Id}}", $candidate)
    if ($probe.ExitCode -eq 0) {
        $resolvedImage = $candidate
        $imageId = ($probe.Output -join "").Trim()
        break
    }
}
if (-not $resolvedImage) {
    throw "image not found locally: $($candidates -join ', '). Pull it first (scripts/docker/pull_swebench.ps1) or pass -Image."
}
Write-Step "image: $resolvedImage ($imageId)"

# ---------------------------------------------------------------- work dir
if (-not $WorkDir) {
    $WorkDir = Join-Path $RepoRoot ("logs\swebench\work\{0}\{1}" -f $Instance, (Get-Date -Format "yyyyMMdd-HHmmss"))
} elseif (-not [System.IO.Path]::IsPathRooted($WorkDir)) {
    $WorkDir = Join-Path $RepoRoot $WorkDir
}
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$WorkDir = (Resolve-Path -LiteralPath $WorkDir).Path
$testbedHost = Join-Path $WorkDir "testbed"
if (Test-Path -LiteralPath $testbedHost) {
    throw "WorkDir already contains a testbed: $testbedHost. Use a fresh -WorkDir or remove it yourself."
}
$evalLog = Join-Path $WorkDir "eval.log"
$resultPath = Join-Path $WorkDir "result.json"

# ----------------------------------------------------------------- patch
$hasModelPatch = -not [string]::IsNullOrWhiteSpace($PatchPath)
$patchSha = $null
$modelPatchText = ""
$patchSource = $null
if ($hasModelPatch) {
    $patchSource = (Resolve-Path -LiteralPath $PatchPath).Path
    $patchSha = (Get-FileHash -LiteralPath $patchSource -Algorithm SHA256).Hash.ToLowerInvariant()
    $modelPatchText = [System.IO.File]::ReadAllText($patchSource)
}
Write-TextFile (Join-Path $WorkDir "model.patch") $modelPatchText
Write-TextFile (Join-Path $WorkDir "test.patch") ([string]$meta.test_patch)
Write-Step ("patch: " + $(if ($hasModelPatch) { "$patchSource sha256=$patchSha" } else { "null (no model patch)" }))

# ------------------------------------------------------------- containers
$runId = [guid]::NewGuid().ToString("N").Substring(0, 8)
$safeInstance = $Instance -replace '[^A-Za-z0-9_.-]', '-'
$createName = "pir-swe-$safeInstance-$runId"
$runName = "pir-swe-run-$safeInstance-$runId"

$evalScript = @'
#!/usr/bin/env bash
set -u
export PATH=/opt/miniconda3/envs/testbed/bin:$PATH
cd /testbed || exit 90
git config --global --add safe.directory /testbed
git config core.autocrlf false
git config core.filemode false

echo "=== git checkout -f __BASE__ ==="
git checkout -f __BASE__ || exit 91
echo "--- git status (after checkout) ---"
git status --short

echo "=== git apply model patch ==="
if [ -s /pir-eval/model.patch ]; then
  git apply -v /pir-eval/model.patch || exit 92
else
  echo "(null patch: skipped)"
fi

echo "=== git apply test patch ==="
git apply -v /pir-eval/test.patch || exit 93

# ---------------------------------------------------------------- hermetic httpbin
# Public httpbin.org now 403s redirect-to?url=http://www.google.co.uk and the
# 2014 requests CA bundle cannot validate it. Run httpbin 0.10.2 locally on
# 127.0.0.1:80/443; docker --add-host maps httpbin.org and www.google.co.uk
# to 127.0.0.1, so the test file runs unchanged.
echo "=== local httpbin setup ==="
PIP_LOG=/tmp/pip-httpbin.log
# Old instances (requests 2.x) use a Python 2.7 testbed; httpbin 0.10.2 is
# Python 3-only and the 2.7 pip resolver could not find it. Serve httpbin from
# the base Python 3 env in that case; the testbed python only runs pytest.
TESTBED_PY=/opt/miniconda3/envs/testbed/bin/python
if "$TESTBED_PY" -c 'import sys; raise SystemExit(0 if sys.version_info[0] >= 3 else 1)' 2>/dev/null; then
  HTTPBIN_PY="$TESTBED_PY"
  HTTPBIN_PIP=/opt/miniconda3/envs/testbed/bin/pip
else
  HTTPBIN_PY=/opt/miniconda3/bin/python
  HTTPBIN_PIP=/opt/miniconda3/bin/pip
fi
if [ ! -d /tmp/httpbin-pkg/httpbin ]; then
  echo "installing httpbin==0.10.2 into /tmp/httpbin-pkg ($HTTPBIN_PY)..."
  "$HTTPBIN_PIP" install --disable-pip-version-check --no-input \
    httpbin==0.10.2 --target /tmp/httpbin-pkg -q >"$PIP_LOG" 2>&1 || {
      echo "pip install httpbin failed"; tail -50 "$PIP_LOG"; exit 94; }
fi

CA_DIR=/tmp/pir-ca
mkdir -p "$CA_DIR/shim"
cd "$CA_DIR" || exit 95
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt \
  -days 2 -subj "/CN=pir-swe-local-ca" >/dev/null 2>&1 || exit 95
openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr \
  -subj "/CN=httpbin.org" >/dev/null 2>&1 || exit 95
printf "subjectAltName=DNS:httpbin.org,DNS:localhost,IP:127.0.0.1\n" > ext.cnf
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out server.crt -days 2 -extfile ext.cnf >/dev/null 2>&1 || exit 95
cat ca.crt /etc/ssl/certs/ca-certificates.crt > bundle.crt
printf 'def where():\n    return "%s/bundle.crt"\n' "$CA_DIR" > "$CA_DIR/shim/certifi.py"

cd /testbed || exit 90
PYTHONPATH=/tmp/httpbin-pkg "$HTTPBIN_PY" -c \
  'from httpbin import app; app.run(host="127.0.0.1", port=80, threaded=True)' \
  >/tmp/httpbin-http.log 2>&1 &
PYTHONPATH=/tmp/httpbin-pkg "$HTTPBIN_PY" -c \
  'import ssl; from httpbin import app; ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain("/tmp/pir-ca/server.crt", "/tmp/pir-ca/server.key"); app.run(host="127.0.0.1", port=443, threaded=True, ssl_context=ctx)' \
  >/tmp/httpbin-https.log 2>&1 &

ready=0
for i in $(seq 1 30); do
  http_code=$(curl -s -o /dev/null -w '%{http_code}' http://httpbin.org/get || true)
  https_code=$(curl -s --cacert /tmp/pir-ca/ca.crt -o /dev/null -w '%{http_code}' https://httpbin.org/get || true)
  if [ "$http_code" = "200" ] && [ "$https_code" = "200" ]; then
    echo "local httpbin ready after ${i}s (http=$http_code https=$https_code)"
    ready=1
    break
  fi
  sleep 1
done
if [ "$ready" != "1" ]; then
  echo "local httpbin failed to start (http=$http_code https=$https_code)"
  tail -40 /tmp/httpbin-http.log
  tail -40 /tmp/httpbin-https.log
  exit 96
fi
curl -s -o /dev/null -w "redirect-to self-test: %{http_code} -> %{redirect_url}\n" \
  "http://httpbin.org/redirect-to?url=http%3A%2F%2Fwww.google.co.uk" || true

echo "=== pytest (timeout __TIMEOUT__s) ==="
HTTPBIN_URL=http://httpbin.org/ PYTHONPATH=/tmp/pir-ca/shim REQUESTS_CA_BUNDLE=/tmp/pir-ca/bundle.crt \
  timeout --signal=TERM --kill-after=10 __TIMEOUT__ \
  /opt/miniconda3/envs/testbed/bin/python -m pytest --no-header -rA --tb=no -p no:cacheprovider test_requests.py
rc=$?
echo "PYTEST_EXIT_CODE=$rc"
exit $rc
'@
$evalScript = $evalScript.Replace("__BASE__", [string]$meta.base_commit).Replace("__TIMEOUT__", [string]$TimeoutSec)
Write-TextFile (Join-Path $WorkDir "eval.sh") $evalScript

$startedAt = (Get-Date).ToString("o")
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$runResult = $null
$runExit = $null
try {
    Write-Step "docker create $createName"
    $create = Invoke-Docker @("create", "--name", $createName, "--platform", "linux/amd64", $resolvedImage)
    if ($create.ExitCode -ne 0) {
        throw "docker create failed (exit $($create.ExitCode)): $($create.Output -join [Environment]::NewLine)"
    }

    Write-Step "docker cp ${createName}:/testbed -> $WorkDir"
    $cp = Invoke-Docker @("cp", "${createName}:/testbed", $WorkDir)
    if ($cp.ExitCode -ne 0) {
        throw "docker cp failed (exit $($cp.ExitCode)): $($cp.Output -join [Environment]::NewLine)"
    }

    Write-Step "running evaluation (pytest timeout ${TimeoutSec}s)"
    $dockerArgs = @(
        "run", "--rm", "--name", $runName, "--platform", "linux/amd64",
        "--add-host", "httpbin.org:127.0.0.1",
        "--add-host", "www.google.co.uk:127.0.0.1",
        "-v", "${testbedHost}:/testbed",
        "-v", "${WorkDir}:/pir-eval",
        "-w", "/testbed",
        $resolvedImage,
        "bash", "/pir-eval/eval.sh"
    )
    $runResult = Invoke-Docker $dockerArgs
    $runExit = $runResult.ExitCode
    Write-TextFile $evalLog ((@($runResult.Output) -join "`n") + "`n")
} finally {
    $sw.Stop()
    if (-not $KeepContainer) {
        Invoke-Docker @("rm", "-f", $createName) | Out-Null
        Invoke-Docker @("rm", "-f", $runName) | Out-Null
    }
}
if ($null -eq $runResult) { throw "evaluation did not run; see output above" }

# ---------------------------------------------------------------- parsing
$statusMap = @{}
$summaryLine = $null
$logLines = @()
if (Test-Path -LiteralPath $evalLog) {
    $logLines = [System.IO.File]::ReadAllLines($evalLog)
}
foreach ($line in $logLines) {
    if ($line -match '^(PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS)\s+(\S+)') {
        $statusMap[$Matches[2]] = $Matches[1]
    }
    if ($line -match '^=+ .*\bin\s+[0-9.]+s.*=+$') {
        $summaryLine = $line
    }
}
$pytestSeconds = $null
if ($summaryLine -and $summaryLine -match '\bin\s+([0-9.]+)s') {
    $pytestSeconds = [double]$Matches[1]
}

$f2pFailed = New-Object System.Collections.Generic.List[object]
$p2pFailed = New-Object System.Collections.Generic.List[object]
$f2pPassed = 0
$p2pPassed = 0
foreach ($test in @($meta.FAIL_TO_PASS)) {
    $status = if ($statusMap.ContainsKey($test)) { $statusMap[$test] } else { "MISSING" }
    if ($status -eq "PASSED") {
        $f2pPassed++
    } else {
        $f2pFailed.Add([pscustomobject]@{ test = $test; status = $status; set = "FAIL_TO_PASS" })
    }
}
foreach ($test in @($meta.PASS_TO_PASS)) {
    $status = if ($statusMap.ContainsKey($test)) { $statusMap[$test] } else { "MISSING" }
    if ($status -eq "PASSED") {
        $p2pPassed++
    } else {
        $p2pFailed.Add([pscustomobject]@{ test = $test; status = $status; set = "PASS_TO_PASS" })
    }
}
$f2pTotal = @($meta.FAIL_TO_PASS).Count
$p2pTotal = @($meta.PASS_TO_PASS).Count
$resolved = ($f2pPassed -eq $f2pTotal -and $p2pPassed -eq $p2pTotal)
$observedFailed = @($statusMap.Keys | Where-Object { $statusMap[$_] -in @("FAILED", "ERROR") } | Sort-Object)

$result = [ordered]@{
    instance_id       = $Instance
    resolved          = [bool]$resolved
    f2p_passed        = $f2pPassed
    f2p_total         = $f2pTotal
    p2p_passed        = $p2pPassed
    p2p_total         = $p2pTotal
    failed_tests      = @($f2pFailed.ToArray() + $p2pFailed.ToArray())
    pytest_failed_tests = $observedFailed
    pytest_exit_code  = $runExit
    pytest_summary    = $summaryLine
    pytest_seconds    = $pytestSeconds
    image             = $resolvedImage
    image_id          = $imageId
    patch_sha256      = $patchSha
    patch_path        = $patchSource
    elapsed_sec       = [math]::Round($sw.Elapsed.TotalSeconds, 1)
    started_at        = $startedAt
    work_dir          = $WorkDir
    log               = $evalLog
}
$json = ($result | ConvertTo-Json -Depth 6)
[System.IO.File]::WriteAllText($resultPath, $json + "`n", $Utf8NoBom)
Write-Step "result -> $resultPath"
Write-Host $json

if ($null -ne $runExit -and $runExit -ne 0) { exit $runExit }
exit 0
