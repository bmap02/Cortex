# Cortex installer for Windows (PowerShell 5.1+ or PowerShell 7).
#   irm https://raw.githubusercontent.com/bmap02/Cortex/main/install.ps1 | iex
# Options (environment variables):
#   CORTEX_REPO         GitHub repo to download from (default below)
#   CORTEX_VERSION      a tag like v0.1.0 (default: latest release)
#   CORTEX_INSTALL_DIR  install folder (default: %LOCALAPPDATA%\Programs\cortex)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # makes Invoke-WebRequest much faster on PowerShell 5
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo    = if ($env:CORTEX_REPO) { $env:CORTEX_REPO } else { 'bmap02/Cortex' }
$Version = if ($env:CORTEX_VERSION) { $env:CORTEX_VERSION } else { 'latest' }
$Dir     = if ($env:CORTEX_INSTALL_DIR) { $env:CORTEX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\cortex' }

# x64 build; Windows on ARM runs it through built-in x64 emulation
$Target = 'x86_64-pc-windows-msvc'
if ($Version -eq 'latest') {
    $Url = "https://github.com/$Repo/releases/latest/download/cortex-$Target.zip"
} else {
    $Url = "https://github.com/$Repo/releases/download/$Version/cortex-$Target.zip"
}

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("cortex-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Force $Tmp | Out-Null
try {
    Write-Host "Downloading $Url"
    try {
        Invoke-WebRequest -Uri $Url -OutFile (Join-Path $Tmp 'cortex.zip') -UseBasicParsing
    } catch {
        Write-Host ""
        Write-Host "Couldn't download Cortex: $($_.Exception.Message)" -ForegroundColor Red
        Write-Host "  - No release published yet? Check https://github.com/$Repo/releases"
        Write-Host "  - Pinned a version? Make sure CORTEX_VERSION=$Version exists."
        Write-Host "  - Or build from source: download the repo and run install-windows.cmd"
        return
    }
    Expand-Archive -Path (Join-Path $Tmp 'cortex.zip') -DestinationPath $Tmp -Force
    New-Item -ItemType Directory -Force $Dir | Out-Null
    Copy-Item (Join-Path $Tmp 'cortex.exe') (Join-Path $Dir 'cortex.exe') -Force
    # downloaded files are flagged as "from the internet"; clear it so SmartScreen doesn't block the first run
    Unblock-File (Join-Path $Dir 'cortex.exe')
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

# Add to the user PATH (no admin rights needed)
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $UserPath) { $UserPath = '' }
if (($UserPath -split ';') -notcontains $Dir) {
    [Environment]::SetEnvironmentVariable('Path', ($UserPath.TrimEnd(';') + ";$Dir").TrimStart(';'), 'User')
    Write-Host "Added $Dir to your PATH."
}
$env:Path = "$env:Path;$Dir"

$v = & (Join-Path $Dir 'cortex.exe') --version
Write-Host "Installed $v to $Dir\cortex.exe"
Write-Host ""
Write-Host "Open a new terminal, then run 'cortex setup' to connect your model server."
