function Write-RotatingLog {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Message
    )

    $maxBytes = 1MB
    $backup = "$Path.1"
    $utf8 = [System.Text.UTF8Encoding]::new($false)
    $line = "$Message`r`n"
    if ($utf8.GetByteCount($line) -gt $maxBytes) {
        $line = "$($Message.Substring(0, [Math]::Min($Message.Length, 240000))) [truncated]`r`n"
    }
    $lineBytes = $utf8.GetByteCount($line)

    foreach ($file in @($Path, $backup)) {
        if ((Test-Path -LiteralPath $file) -and (Get-Item -LiteralPath $file).Length -gt $maxBytes) {
            Remove-Item -LiteralPath $file -Force
        }
    }
    if ((Test-Path -LiteralPath $Path) -and ((Get-Item -LiteralPath $Path).Length + $lineBytes) -gt $maxBytes) {
        if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Force }
        Move-Item -LiteralPath $Path -Destination $backup
    }
    [System.IO.File]::AppendAllText($Path, $line, $utf8)
}
