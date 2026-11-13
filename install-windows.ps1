# Cortex all-in-one installer for Windows 10/11.
#
# Run it from the unzipped cortex folder, either by double-clicking install-windows.cmd
# or from PowerShell:
#     powershell -ExecutionPolicy Bypass -File .\install-windows.ps1
#
# It installs whatever is missing, then builds and configures Cortex:
#   1. Microsoft C++ Build Tools  (Rust needs them to compile on Windows)
#   2. Rust (rustup + cargo)
#   3. Cortex itself              (cargo install, puts cortex.exe on your PATH)
#   4. Ollama                     (runs the AI models)
#   5. A model                    (you choose; skipped if you already have one)
#   6. Cortex config + health check
#
# Options:
#   -Model qwen2.5:7b   pick the model without being asked
#   -SkipModel          don't download a model
# Safe to run again: anything already installed is skipped.

param(
    [string]$Model = "",
    [switch]$SkipModel
)

$ErrorActionPreference = 'Continue'   # native tools write progress to stderr; we check exit codes instead
$ProgressPreference = 'SilentlyContinue'
$Src = $PSScriptRoot
$OllamaUrl = 'http://localhost:11434'

function Step($n, $msg) { Write-Host ""; Write-Host "[$n/6] $msg" -ForegroundColor White }
function Ok($msg)       { Write-Host "      ok  $msg" -ForegroundColor Green }
function Info($msg)     { Write-Host "      $msg" -ForegroundColor DarkGray }
function Warn($msg)     { Write-Host "      !   $msg" -ForegroundColor Yellow }
function Fail($msg) {
    Write-Host ""
    Write-Host "      X   $msg" -ForegroundColor Red
    Write-Host ""
    Write-Host "Fix the problem above and run the installer again; finished steps are skipped." -ForegroundColor DarkGray
    exit 1
}
function Have($cmd) { [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }

function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$machine;$user"
    foreach ($extra in @((Join-Path $env:USERPROFILE '.cargo\bin'), (Join-Path $env:LOCALAPPDATA 'Programs\Ollama'))) {
        if ((Test-Path $extra) -and (($env:Path -split ';') -notcontains $extra)) { $env:Path = "$env:Path;$extra" }
    }
}

function Install-WithWinget($id, $name, [string[]]$extra) {
    Info "Installing $name with winget..."
    $wingetArgs = @('install', '--id', $id, '-e', '--accept-source-agreements', '--accept-package-agreements') + $extra
    & winget @wingetArgs
    Info "(winget exit code $LASTEXITCODE)"
    Update-SessionPath
}

function Test-BuildTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $false }
    $found = & $vswhere -products * -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    return [bool]$found
}

function Test-Ollama {
    try { Invoke-RestMethod -Uri "$OllamaUrl/api/tags" -TimeoutSec 3 | Out-Null; return $true } catch { return $false }
}

# ------------------------------------------------------------------ checks
Write-Host ""
Write-Host "  CORTEX installer for Windows" -ForegroundColor White
Write-Host "  Installs what's missing, builds Cortex and connects it to a local model." -ForegroundColor DarkGray

if (-not (Test-Path (Join-Path $Src 'Cargo.toml'))) {
    Fail "Run this from the cortex folder (the one with Cargo.toml). It's looking in: $Src"
}
if (-not (Have 'winget')) {
    Fail "winget (App Installer) isn't available. Install 'App Installer' from the Microsoft Store, then run this again."
}
if ($Src -match 'OneDrive') { Info "Source is in OneDrive; building in $env:LOCALAPPDATA\cortex-build so OneDrive doesn't sync build files." }

# ------------------------------------------------------------------ 1. C++ build tools
Step 1 "Microsoft C++ Build Tools"
if (Test-BuildTools) {
    Ok "already installed"
} else {
    Warn "Not found. Installing them (about 2-4 GB, can take 10+ minutes)."
    Warn "Windows will ask for administrator permission; click Yes."
    Install-WithWinget 'Microsoft.VisualStudio.2022.BuildTools' 'C++ Build Tools' @(
        '--override', '--wait --passive --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended')
    if (Test-BuildTools) { Ok "installed" }
    else { Fail "The C++ Build Tools didn't install. Install 'Build Tools for Visual Studio 2022' with the 'Desktop development with C++' workload from https://visualstudio.microsoft.com/visual-cpp-build-tools/ and run this again." }
}

# ------------------------------------------------------------------ 2. Rust
Step 2 "Rust"
Update-SessionPath
if (-not (Have 'cargo')) {
    if (-not (Have 'rustup')) { Install-WithWinget 'Rustlang.Rustup' 'Rust (rustup)' @() }
    if ((Have 'rustup') -and -not (Have 'cargo')) {
        Info "Installing the stable toolchain..."
        & rustup default stable-msvc
        Update-SessionPath
    }
}
if (Have 'cargo') { Ok (& cargo --version) }
else { Fail "Rust didn't install. Get it from https://rustup.rs, then run this again." }

# ------------------------------------------------------------------ 3. Build Cortex
Step 3 "Building Cortex (first build takes a few minutes)"
$env:CARGO_TARGET_DIR = Join-Path $env:LOCALAPPDATA 'cortex-build'
Push-Location $Src
& cargo install --path . --locked --force
$buildCode = $LASTEXITCODE
Pop-Location
Update-SessionPath
if ($buildCode -ne 0 -or -not (Have 'cortex')) {
    Fail "The build failed (exit code $buildCode). Copy the red error text above and send it over."
}
Ok "$(& cortex --version) installed to $env:USERPROFILE\.cargo\bin"

# ------------------------------------------------------------------ 4. Ollama
Step 4 "Ollama"
if (-not (Have 'ollama')) { Install-WithWinget 'Ollama.Ollama' 'Ollama' @() }
if (-not (Have 'ollama')) { Fail "Ollama didn't install. Get it from https://ollama.com/download, then run this again." }
Ok "installed"
if (-not (Test-Ollama)) {
    Info "Starting the Ollama server..."
    Start-Process -FilePath 'ollama' -ArgumentList 'serve' -WindowStyle Hidden
    for ($i = 0; $i -lt 30 -and -not (Test-Ollama); $i++) { Start-Sleep -Seconds 1 }
}
if (Test-Ollama) { Ok "server running at $OllamaUrl" }
else { Fail "Ollama is installed but its server isn't responding. Open the Ollama app from the Start menu and run this again." }

# ------------------------------------------------------------------ 5. Model
Step 5 "Model"
$installed = @()
try { $installed = @((Invoke-RestMethod -Uri "$OllamaUrl/api/tags").models | ForEach-Object { $_.name }) }
catch { Info "couldn't list the installed models ($($_.Exception.Message)); showing recommended ones" }

if (-not $Model -and -not $SkipModel) {
    # Cortex's own recommendations (`cortex models recommend`), so the two never disagree. Point
    # Cortex at Ollama first so they come as Ollama model names.
    & cortex config set backend ollama | Out-Null
    & cortex config set base_url $OllamaUrl | Out-Null
    $recommended = @(& cortex models recommend --names 2>$null)
    $suggest = if ($recommended.Count -gt 0) { $recommended[0] } else { 'qwen2.5:3b' }
    # the numbered choices: the models already here, or else the recommendations. @() keeps a
    # single model a list: without it PowerShell unwraps it to a string, and [0] is its first letter
    $choices = @(if ($installed.Count -gt 0) { $installed } else { $recommended })
    if ($installed.Count -gt 0) {
        Info "Models you already have:"
        for ($i = 0; $i -lt $installed.Count; $i++) { Write-Host ("        {0}) {1}" -f ($i + 1), $installed[$i]) }
        Info "Pick a number, type another model name to download it, or press Enter for $suggest."
        Info "(Run 'cortex models recommend' later to see which models suit this PC.)"
    } else {
        Info "Models that suit this PC, best first:"
        # just the numbered model lines of the table
        & cortex models recommend | Where-Object { $_ -match '^\s+\d+\.\s' } | Select-Object -First 8 | ForEach-Object { Write-Host "    $_" }
        Info "Pick a number, type a model name, or press Enter for $suggest."
    }
    $answer = (Read-Host "      model").Trim()
    if (-not $answer) { $Model = $suggest }
    elseif ($answer -match '^\d+$' -and [int]$answer -ge 1 -and [int]$answer -le $choices.Count) { $Model = $choices[[int]$answer - 1] }
    else { $Model = $answer }
}

if ($SkipModel) {
    Info "skipped"
} elseif ($installed -contains $Model -or $installed -contains "$Model`:latest") {
    Ok "$Model is already downloaded"
} else {
    Info "Downloading $Model..."
    & ollama pull $Model
    if ($LASTEXITCODE -ne 0) { Fail "Couldn't download $Model. Check the name at https://ollama.com/library and run this again." }
    Ok "$Model downloaded"
}

# ------------------------------------------------------------------ 6. Configure + check
Step 6 "Connecting Cortex to Ollama"
& cortex config set backend ollama | Out-Null
& cortex config set base_url $OllamaUrl | Out-Null
if ($Model) { & cortex config set model $Model | Out-Null }
Ok "saved to $env:USERPROFILE\.cortex\config.toml"
Write-Host ""
& cortex doctor

Write-Host ""
Write-Host "  All set." -ForegroundColor Green
Write-Host "  Open a NEW terminal and type:  cortex" -ForegroundColor White
Write-Host "  Tip: type /context 32k inside Cortex for a bigger context window." -ForegroundColor DarkGray
Write-Host ""
