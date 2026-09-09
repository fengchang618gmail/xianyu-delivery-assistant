# disk-info.ps1 - show physical disk media type and current free memory.
Get-PhysicalDisk |
    Select-Object FriendlyName, MediaType, BusType,
        @{n = 'SizeGB'; e = { [math]::Round($_.Size / 1GB) } } |
    Format-Table -AutoSize | Out-String -Width 120 | Write-Output
$os = Get-CimInstance Win32_OperatingSystem
$perf = Get-CimInstance Win32_PerfFormattedData_PerfOS_Memory
Write-Output ("[mem] free={0}MB commit={1}%" -f [math]::Round($os.FreePhysicalMemory / 1KB), [math]::Round($perf.PercentCommittedBytesInUse))
