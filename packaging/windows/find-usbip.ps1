function Find-UsbipExecutable {
    $candidates = [System.Collections.Generic.List[string]]::new()
    $uninstallKey = 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall'
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry64, [Microsoft.Win32.RegistryView]::Registry32)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, $view)
        try {
            $root = $base.OpenSubKey($uninstallKey)
            if ($root) {
                try {
                    foreach ($name in $root.GetSubKeyNames()) {
                        if ($name -notlike '*199505b0-b93d-4521-a8c7-897818e0205a*') { continue }
                        $entry = $root.OpenSubKey($name)
                        if ($entry) {
                            try {
                                $location = $entry.GetValue('InstallLocation')
                                if ($location) { $candidates.Add((Join-Path $location 'usbip.exe')) }
                            } finally { $entry.Close() }
                        }
                    }
                } finally { $root.Close() }
            }
        } finally { $base.Close() }
    }
    foreach ($base in @($env:ProgramW6432, ${env:ProgramFiles}, ${env:ProgramFiles(x86)})) {
        if ($base) {
            $candidate = Join-Path $base 'USBip\usbip.exe'
            if (-not $candidates.Contains($candidate)) {
                $candidates.Add($candidate)
            }
        }
    }
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    return $null
}
