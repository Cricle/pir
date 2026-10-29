$ErrorActionPreference = 'Continue'
Set-Location 'C:\Users\XPENG_USER\Desktop\github\pir'
$bins = Get-Content 'test_bins.txt'
$summary = @()
$idx = 0
foreach ($line in $bins) {
    $parts = $line -split '\|', 2
    $exe = $parts[0]
    $name = $parts[1]
    $idx++
    $log = "accept_runs\$name.log"
    cmd /c "`"$exe`" --test-threads=8 --nocapture > `"$log`" 2>&1"
    $resultLine = (Select-String -Path $log -Pattern '^test result:') | ForEach-Object Line
    $summary += "$name :: $($resultLine -join ' | ')"
    Write-Host "[$idx/$($bins.Count)] $name :: $resultLine"
}
$summary | Set-Content 'accept_summary.txt' -Encoding utf8
Write-Host 'ALL_DONE'
