$c = Get-Content 'h:\My Drive\synergy\SYNERGY.md'
Write-Output ("Total lines: " + $c.Count)
Write-Output "--- Last 100 lines: ---"
$start = [Math]::Max(0, $c.Count - 100)
for ($i = $start; $i -lt $c.Count; $i++) {
    Write-Output $c[$i]
}
