# Runs the sound card service, restarting it if it exits, with its log in %LOCALAPPDATA%\Inferno.
$folder = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $folder 'rotating-log.ps1')
$service = Join-Path $folder 'inferno_soundcard.exe'
$usbipPathFile = Join-Path $folder 'usbip-path.txt'
if (Test-Path -LiteralPath $usbipPathFile) {
    $env:USBIP_EXE = [System.IO.File]::ReadAllText($usbipPathFile).Trim()
}
$logFolder = Join-Path $env:LOCALAPPDATA 'Inferno'
New-Item -ItemType Directory -Force $logFolder | Out-Null
$log = Join-Path $logFolder 'soundcard.log'

while ($true) {
    Write-RotatingLog -Path $log -Message "$(Get-Date -Format o) Starting Inferno Sound Cards"
    & $service 2>&1 | ForEach-Object {
        Write-RotatingLog -Path $log -Message "$(Get-Date -Format o) $([string]$_)"
    }
    # Quit from the tray icon ends the service with exit code 0: stay stopped until the next sign-in
    if ($LASTEXITCODE -eq 0) { break }
    Write-RotatingLog -Path $log -Message "$(Get-Date -Format o) The service exited ($LASTEXITCODE); restarting in five seconds"
    Start-Sleep -Seconds 5
}
