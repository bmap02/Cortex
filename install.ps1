# Cortex installer for Windows (PowerShell 5.1+ or PowerShell 7).
#   irm https://raw.githubusercontent.com/bmap02/Cortex/main/install.ps1 | iex
# Options (environment variables):
#   CORTEX_REPO         GitHub repo to download from (default below)
#   CORTEX_VERSION      a tag like v0.1.0 (default: latest release)
#   CORTEX_INSTALL_DIR  install folder (default: %LOCALAPPDATA%\Programs\cortex)

# Everything runs inside a script block so that, under `irm | iex`, the preference
# variables and helper variables below don't leak into the user's PowerShell session.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'   # makes Invoke-WebRequest much faster on PowerShell 5

    $Repo    = if ($env:CORTEX_REPO) { $env:CORTEX_REPO } else { 'bmap02/Cortex' }
    $Version = if ($env:CORTEX_VERSION) { $env:CORTEX_VERSION } else { 'latest' }
    $Dir     = if ($env:CORTEX_INSTALL_DIR) { $env:CORTEX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\cortex' }

    # x64 build; Windows 11 on ARM runs it through built-in x64 emulation
    $Target = 'x86_64-pc-windows-msvc'
    $Releases = "https://github.com/$Repo/releases"
    if ($Version -eq 'latest') {
        $Url = "$Releases/latest/download/cortex-$Target.zip"
    } else {
        $Url = "$Releases/download/$Version/cortex-$Target.zip"
    }

    # TLS 1.2 is required by GitHub; add it for this install only and restore the setting afterwards
    $OldProtocol = [Net.ServicePointManager]::SecurityProtocol
    $Tmp = Join-Path ([IO.Path]::GetTempPath()) ("cortex-" + [Guid]::NewGuid())
    try {
        if ($OldProtocol -ne [Net.SecurityProtocolType]::SystemDefault) {
            [Net.ServicePointManager]::SecurityProtocol = $OldProtocol -bor [Net.SecurityProtocolType]::Tls12
        }
        New-Item -ItemType Directory -Force $Tmp | Out-Null
        Write-Host "Downloading $Url"
        try {
            Invoke-WebRequest -Uri $Url -OutFile (Join-Path $Tmp 'cortex.zip') -UseBasicParsing
        } catch {
            $ex = $_.Exception
            $status = $null
            if ($ex.Response) { $status = [int]$ex.Response.StatusCode }
            Write-Host ""
            Write-Host "Couldn't download Cortex: $($ex.Message)" -ForegroundColor Red
            if ($ex.InnerException) { Write-Host "  ($($ex.InnerException.Message))" -ForegroundColor Red }
            if ($status -eq 404) {
                if ($env:CORTEX_VERSION) {
                    Write-Host "  - Make sure the release $Version exists: $Releases"
                } else {
                    Write-Host "  - No release published yet? Check $Releases"
                }
            } else {
                Write-Host "  - Check your internet connection, proxy or firewall, then try again."
            }
            Write-Host "  - Or build from source: download the repo and run install-windows.cmd"
            throw "Cortex was not installed."
        }
        Expand-Archive -Path (Join-Path $Tmp 'cortex.zip') -DestinationPath $Tmp -Force
        New-Item -ItemType Directory -Force $Dir | Out-Null
        $Exe = Join-Path $Dir 'cortex.exe'
        if (Test-Path $Exe) {
            # Windows can't overwrite a running exe but can rename it, so updating works while Cortex is open
            $Old = "$Exe.old"
            Remove-Item $Old -Force -ErrorAction SilentlyContinue
            if (Test-Path $Old) { $Old = "$Exe.$([Guid]::NewGuid().ToString('N')).old" }
            Rename-Item $Exe (Split-Path $Old -Leaf)
        }
        Copy-Item (Join-Path $Tmp 'cortex.exe') $Exe -Force
        # downloaded files are flagged as "from the internet"; clear it so SmartScreen doesn't block the first run
        Unblock-File $Exe
    } finally {
        Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
        [Net.ServicePointManager]::SecurityProtocol = $OldProtocol
    }

    # Add to the user PATH (no admin rights needed). Read the raw registry value so entries
    # like %USERPROFILE%\... stay unexpanded, and keep the value's type (REG_EXPAND_SZ).
    $Key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        $UserPath = $Key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $Kind = if ($UserPath) { $Key.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
        $Entries = @($UserPath -split ';' | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') })
        if ($Entries -notcontains $Dir.TrimEnd('\')) {
            $Key.SetValue('Path', ($UserPath.TrimEnd(';') + ";$Dir").TrimStart(';'), $Kind)
            # tell running programs (Explorer, new terminals) that the environment changed
            [Environment]::SetEnvironmentVariable('CORTEX_PATH_REFRESH', '1', 'User')
            [Environment]::SetEnvironmentVariable('CORTEX_PATH_REFRESH', $null, 'User')
            Write-Host "Added $Dir to your PATH."
        }
    } finally {
        $Key.Close()
    }
    if (($env:Path -split ';') -notcontains $Dir) { $env:Path = "$env:Path;$Dir" }

    $v = & $Exe --version
    if ($LASTEXITCODE -ne 0) { throw "cortex.exe was installed to $Dir but failed to run (exit code $LASTEXITCODE)." }
    Write-Host "Installed $v to $Exe"
    Write-Host ""
    Write-Host "Open a new terminal, then run 'cortex setup' to connect your model server."
}
