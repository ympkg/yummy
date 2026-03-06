$ErrorActionPreference = "Stop"

$Repo = "ympkg/yummy"
$InstallDir = if ($env:YM_INSTALL_DIR) { $env:YM_INSTALL_DIR } else { "$env:USERPROFILE\.ym\bin" }

# Get latest version
Write-Host "Fetching latest release..."
$ReleaseUrl = "https://api.github.com/repos/$Repo/releases/latest"
try {
    $Release = Invoke-RestMethod -Uri $ReleaseUrl -Headers @{ "User-Agent" = "ym-installer" }
    $Version = $Release.tag_name -replace '^v', ''
} catch {
    Write-Host "No stable release found. Trying latest pre-release..."
    $Releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases" -Headers @{ "User-Agent" = "ym-installer" }
    $Version = ($Releases[0].tag_name) -replace '^v', ''
}

if (-not $Version) {
    Write-Host "Error: Could not determine version." -ForegroundColor Red
    exit 1
}

$Target = "x86_64-pc-windows-msvc"
$Archive = "ym-$Version-$Target.zip"
$DownloadUrl = "https://github.com/$Repo/releases/download/v$Version/$Archive"

Write-Host "Installing ym v$Version for Windows..."
Write-Host "Downloading $DownloadUrl..."

# Download
$TmpDir = Join-Path $env:TEMP "ym-install-$(Get-Random)"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null
$ArchivePath = Join-Path $TmpDir $Archive

Invoke-WebRequest -Uri $DownloadUrl -OutFile $ArchivePath

# Extract
Expand-Archive -Path $ArchivePath -DestinationPath $TmpDir -Force

# Install
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null

$ExtractedDir = Join-Path $TmpDir "ym-$Version-$Target"
Copy-Item "$ExtractedDir\ym.exe" "$InstallDir\ym.exe" -Force
Copy-Item "$ExtractedDir\ym.exe" "$InstallDir\ymc.exe" -Force
if (Test-Path "$ExtractedDir\ym-agent.jar") {
    Copy-Item "$ExtractedDir\ym-agent.jar" "$InstallDir\ym-agent.jar" -Force
}

# Cleanup
Remove-Item -Recurse -Force $TmpDir

Write-Host ""
Write-Host "Installed ym v$Version to $InstallDir" -ForegroundColor Green
Write-Host ""

# Check PATH
if ($env:PATH -notlike "*$InstallDir*") {
    Write-Host "Add to your PATH by running:" -ForegroundColor Yellow
    Write-Host ""
    Write-Host "  [Environment]::SetEnvironmentVariable('PATH', `"$InstallDir;`$env:PATH`", 'User')"
    Write-Host ""
}

Write-Host "Run 'ym --version' to verify."
