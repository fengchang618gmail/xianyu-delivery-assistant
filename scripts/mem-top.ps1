# mem-top.ps1 - list top processes by commit to find memory hogs.
$os = Get-CimInstance Win32_OperatingSystem
$perf = Get-CimInstance Win32_PerfFormattedData_PerfOS_Memory
$freeMB = [math]::Round($os.FreePhysicalMemory / 1KB)
$commit = [math]::Round($perf.PercentCommittedBytesInUse)
Write-Output ("[mem] free={0}MB commit={1}%" -f $freeMB, $commit)
Get-Process |
    Sort-Object PagedMemorySize64 -Descending |
    Select-Object -First 18 Name, Id,
        @{n = 'WS_MB'; e = { [math]::Round($_.WorkingSet64 / 1MB) } },
        @{n = 'Commit_MB'; e = { [math]::Round($_.PagedMemorySize64 / 1MB) } } |
    Format-Table -AutoSize | Out-String -Width 200 | Write-Output
