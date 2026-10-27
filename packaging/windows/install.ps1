param([ValidateSet('Install', 'Uninstall')][string]$Action)

# Installs usbip-win2 if it's missing (the pinned release, shared with other applications such as
# Win AirPlay: it stays installed on uninstall) and starts the sound card service at sign-in.

$ErrorActionPreference = 'Stop'
# Started from PowerShell 7 (e.g. a silent install from a script), Windows PowerShell inherits a module path
# leading to PowerShell 7's modules, which it can't load: use its own
$env:PSModulePath = [Environment]::GetEnvironmentVariable('PSModulePath', 'Machine')
Start-Transcript -Path (Join-Path $env:TEMP 'inferno-soundcard-setup.log') -Force | Out-Null
$taskName = 'Inferno Sound Cards'
$folder = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $folder 'find-usbip.ps1')

if ($Action -eq 'Uninstall') {
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    # its cards are unplugged when its USB/IP connections close
    Get-Process inferno_soundcard -ErrorAction SilentlyContinue | Stop-Process -Force
    exit 0
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Administrator permission is required. Please approve the Windows elevation prompt when starting the installer.'
}

$usbip = Find-UsbipExecutable
if (-not $usbip) {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
    if ($arch -notin @('x64', 'arm64')) { throw "Unsupported Windows architecture: $arch" }
    $upstream = Join-Path $folder "USBip-0.9.8.1-$arch.exe"
    $expected = switch ($arch) {
        'x64' { '38cad6d4432b52d5bb9409d9ad03b72fdffc4ada4cd3a48fbeca1a2752a8518a' }
        'arm64' { 'cab7ff97f79275eeb5c5b8bb2eb3111ff6b6c4eead07d9bec9be5bd1e3a35800' }
    }
    # .NET rather than Get-FileHash: started from PowerShell 7, Windows PowerShell may not load its modules
    $stream = [System.IO.File]::OpenRead($upstream)
    try {
        $sha256 = [System.Security.Cryptography.SHA256]::Create()
        try { $actual = ([System.BitConverter]::ToString($sha256.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
        finally { $sha256.Dispose() }
    } finally { $stream.Dispose() }
    if ($actual -ne $expected) { throw 'The bundled USBip installer checksum does not match.' }
    $programFiles64 = if ($env:ProgramW6432) { $env:ProgramW6432 } else { ${env:ProgramFiles} }
    $installDir = Join-Path $programFiles64 'USBip'
    $upstreamLog = Join-Path $env:TEMP 'inferno-soundcard-usbip-install.log'
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', '/COMPONENTS=main,client', "/DIR=`"$installDir`"", "/LOG=`"$upstreamLog`"")
    $process = Start-Process -FilePath $upstream -ArgumentList $arguments -Wait -PassThru
    if ($process.ExitCode -notin @(0, 3010)) { throw "USBip installer exited with $($process.ExitCode)." }
    $usbip = Find-UsbipExecutable
    if (-not $usbip) {
        throw "USBip setup exited with code $($process.ExitCode), but usbip.exe was not found. Check $upstreamLog and restart Windows if setup requested it."
    }
}
[System.IO.File]::WriteAllText((Join-Path $folder 'usbip-path.txt'), $usbip)

$launcher = Join-Path $folder 'run-soundcard.ps1'
$user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$taskAction = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$launcher`""
$taskTrigger = New-ScheduledTaskTrigger -AtLogOn -User $user
# attaching USB/IP devices needs administrator rights
$taskPrincipal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
Get-Process inferno_soundcard -ErrorAction SilentlyContinue | Stop-Process -Force
Register-ScheduledTask -TaskName $taskName -Action $taskAction -Trigger $taskTrigger -Principal $taskPrincipal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $taskName
