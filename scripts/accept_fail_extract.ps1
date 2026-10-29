$ErrorActionPreference = 'Continue'
Set-Location 'C:\Users\XPENG_USER\Desktop\github\pir'
$summary = Get-Content 'accept_summary.txt'
foreach ($line in $summary) {
    if ($line -notmatch 'FAILED') { continue }
    $name = ($line -split ' :: ')[0]
    Write-Host "########## $name ##########"
    $log = "accept_runs\$name.log"
    $failed = (Select-String -Path $log -Pattern '^test (.+) \.\.\. FAILED') | ForEach-Object { $_.Matches[0].Groups[1].Value }
    foreach ($f in $failed) { Write-Host "FAILED: $f" }
    # pull first panic message after each failure block (limited)
    $panics = (Select-String -Path $log -Pattern 'panicked at|assertion|Error:|thread .* panicked') | Select-Object -First 12
    foreach ($p in $panics) {
        $t = $p.Line
        if ($t.Length -gt 220) { $t = $t.Substring(0, 220) }
        Write-Host "  >> $t"
    }
}
