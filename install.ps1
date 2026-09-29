# Installs cf-ts from GitHub releases on Windows.
#
#   irm https://raw.githubusercontent.com/DaBorsten/cf-target-switcher/main/install.ps1 | iex
#
# Environment variables:
#   CF_TS_VERSION          release tag to install, e.g. v0.1.0 (default: latest)
#   CF_TS_INSTALL_DIR      target directory (default: %LOCALAPPDATA%\cf-ts\bin)
#   CF_TS_NO_MODIFY_PATH   set to 1 to leave the user PATH untouched

# Runs in a script block so errors surface via `throw` without closing the
# console when piped into `iex`.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'  # makes Invoke-WebRequest much faster on Windows PowerShell 5.1
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'DaBorsten/cf-target-switcher'
    $bin = 'cf-ts'

    try {
        $arch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    } catch {
        $arch = $env:PROCESSOR_ARCHITECTURE
    }
    switch ($arch) {
        { $_ -in 'X64', 'AMD64' } { $target = 'x86_64-pc-windows-msvc' }
        { $_ -in 'Arm64' } { $target = 'aarch64-pc-windows-msvc' }
        default { throw "Unsupported architecture: $arch" }
    }

    $version = if ($env:CF_TS_VERSION) { $env:CF_TS_VERSION } else { 'latest' }
    $base = if ($version -eq 'latest') {
        "https://github.com/$repo/releases/latest/download"
    } else {
        "https://github.com/$repo/releases/download/$version"
    }
    $asset = "$bin-$target.zip"
    $dir = if ($env:CF_TS_INSTALL_DIR) { $env:CF_TS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'cf-ts\bin' }
    $dir = $dir.TrimEnd('\')

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
    New-Item -ItemType Directory $tmp | Out-Null
    try {
        Write-Host "Downloading $asset ($version)..."
        $zip = Join-Path $tmp $asset
        $sums = Join-Path $tmp 'SHA256SUMS.txt'
        Invoke-WebRequest "$base/$asset" -OutFile $zip -UseBasicParsing
        Invoke-WebRequest "$base/SHA256SUMS.txt" -OutFile $sums -UseBasicParsing

        $line = Get-Content $sums | Where-Object { $_ -match "\s\*?$([regex]::Escape($asset))$" } | Select-Object -First 1
        if (-not $line) { throw "No checksum found for $asset" }
        $expected = ($line -split '\s+')[0]
        $actual = (Get-FileHash $zip -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw "Checksum mismatch for $asset" }

        Expand-Archive $zip -DestinationPath $tmp
        New-Item -ItemType Directory -Force $dir | Out-Null
        Copy-Item (Join-Path $tmp "$bin-$target\$bin.exe") $dir -Force
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }

    $exe = Join-Path $dir "$bin.exe"
    Write-Host "Installed $(& $exe --version) to $exe"

    $onPath = ($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -eq $dir }
    if ($env:CF_TS_NO_MODIFY_PATH -eq '1') {
        if (-not $onPath) { Write-Host "Note: $dir is not on your PATH, add it to run $bin from anywhere." }
        return
    }

    # Read the raw value so entries like %USERPROFILE%\bin stay unexpanded.
    $key = 'HKCU:\Environment'
    $raw = (Get-Item $key).GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $entries = @($raw -split ';' | Where-Object { $_ })
    $present = $entries | Where-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -eq $dir }
    if (-not $present) {
        Set-ItemProperty $key -Name Path -Value (($entries + $dir) -join ';') -Type ExpandString
        # Setting any user variable broadcasts WM_SETTINGCHANGE so new terminals see the new PATH.
        [Environment]::SetEnvironmentVariable('CF_TS_INSTALLER', '1', 'User')
        [Environment]::SetEnvironmentVariable('CF_TS_INSTALLER', $null, 'User')
        Write-Host "Added $dir to your user PATH. Open a new terminal to use $bin everywhere."
    }
    if (-not $onPath) { $env:Path = "$env:Path;$dir" }
}
