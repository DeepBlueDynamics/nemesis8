# nemesis8 installer for Windows
# Usage: powershell -c "irm https://nemesis8.nuts.services/install.ps1 | iex"
#        powershell -c "irm https://nemesis8.nuts.services/install.ps1 | iex" -- --no-modify-path

param([switch]$NoModifyPath)

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "  nemesis8 installer" -ForegroundColor Cyan
Write-Host ""

# Detect architecture
if (-not [Environment]::Is64BitOperatingSystem) {
    Write-Host "Error: 32-bit Windows not supported" -ForegroundColor Red
    exit 1
}
$arch = "x86_64-pc-windows-msvc"

# Get latest release
Write-Host "[*] Finding latest release..." -ForegroundColor Yellow
try {
    $release = Invoke-RestMethod `
        -Uri "https://api.github.com/repos/DeepBlueDynamics/nemesis8/releases/latest" `
        -Headers @{"User-Agent" = "nemesis8-installer"} `
        -TimeoutSec 30
} catch {
    Write-Host "Error: could not reach GitHub API: $_" -ForegroundColor Red
    exit 1
}

$tag = $release.tag_name
$asset = $release.assets | Where-Object { $_.name -match $arch } | Select-Object -First 1

if (-not $asset) {
    Write-Host "Error: no Windows binary found in release $tag" -ForegroundColor Red
    exit 1
}

Write-Host "[OK] Found $tag" -ForegroundColor Green

# Download
$tmpZip = Join-Path $env:TEMP "nemesis8-$tag.zip"
$tmpDir = Join-Path $env:TEMP "nemesis8-extract"

# Cleanup any leftover temp files
if (Test-Path $tmpZip) { Remove-Item -Force $tmpZip }
if (Test-Path $tmpDir) { Remove-Item -Recurse -Force $tmpDir }

Write-Host "[*] Downloading $($asset.name)..." -ForegroundColor Yellow
try {
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmpZip -TimeoutSec 120
} catch {
    Write-Host "Error: download failed: $_" -ForegroundColor Red
    exit 1
}

# Extract
try {
    Expand-Archive -Path $tmpZip -DestinationPath $tmpDir -Force
} catch {
    Write-Host "Error: failed to extract archive: $_" -ForegroundColor Red
    Remove-Item -Force $tmpZip -ErrorAction SilentlyContinue
    exit 1
}

# Locate binary
$exe = Get-ChildItem -Path $tmpDir -Filter "nemisis8.exe" -Recurse | Select-Object -First 1
if (-not $exe) {
    Write-Host "Error: nemisis8.exe not found in archive" -ForegroundColor Red
    exit 1
}

# Stop running instances — retry until handles are released
$binDir = Join-Path $env:USERPROFILE ".local\bin"
foreach ($name in @("nemesis8", "nemisis8", "n8")) {
    Stop-Process -Name $name -Force -ErrorAction SilentlyContinue
}
$retries = 0
while ($retries -lt 5) {
    $locked = $false
    foreach ($dest in @("nemesis8.exe", "nemisis8.exe", "n8.exe")) {
        $path = Join-Path $binDir $dest
        if (Test-Path $path) {
            try {
                $stream = [System.IO.File]::Open($path, 'Open', 'ReadWrite', 'None')
                $stream.Close()
            } catch {
                $locked = $true
            }
        }
    }
    if (-not $locked) { break }
    Start-Sleep -Milliseconds 500
    $retries++
}

# Install
if (-not (Test-Path $binDir)) {
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
}

Copy-Item $exe.FullName (Join-Path $binDir "nemesis8.exe") -Force
Copy-Item $exe.FullName (Join-Path $binDir "nemisis8.exe") -Force
Copy-Item $exe.FullName (Join-Path $binDir "n8.exe") -Force

# Cleanup
Remove-Item -Force $tmpZip -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue

# PATH setup
if (-not $NoModifyPath) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $pathParts = $userPath -split ";" | Where-Object { $_ -ne "" } | Select-Object -Unique
    if (-not ($pathParts | Where-Object { $_ -ieq $binDir })) {
        $newPath = ($pathParts + $binDir) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        $env:Path = [System.Environment]::GetEnvironmentVariable("Path", "Machine") + ";" +
                    [System.Environment]::GetEnvironmentVariable("Path", "User")
        Write-Host "[!] Added $binDir to PATH (restart terminal to take effect)" -ForegroundColor Yellow
    }
}

# Verify
try {
    $version = & (Join-Path $binDir "nemesis8.exe") --version 2>&1
    if ($LASTEXITCODE -ne 0) { throw "exit code $LASTEXITCODE" }
} catch {
    Write-Host "Error: installed binary failed to run: $_" -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "nemesis8 installed: $version" -ForegroundColor Green
Write-Host ""
Write-Host "Prerequisites: Docker Desktop (https://docs.docker.com/desktop/)" -ForegroundColor Gray
Write-Host ""
Write-Host "Get started:" -ForegroundColor Cyan
Write-Host "  nemesis8 build                # build the Docker image"
Write-Host "  nemesis8 interactive          # start a session"
Write-Host "  nemesis8 doctor               # check prerequisites"
Write-Host "  nemesis8 --help               # see all commands"
Write-Host ""
