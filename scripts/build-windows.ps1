# Build the Windows release of ATV into dist\:
#   ATV_<version>_x64-setup.exe          installer (per user, no admin rights)
#   ATV_<version>_x64-portable.zip       AnythingToVideo.exe (the app) + atv.exe (command line)
#
# The app is not called ATV.exe: Windows ignores case in file names, so it
# would be the same file as the command-line tool atv.exe.
#
# Needs: Rust, the Tauri CLI (cargo install tauri-cli --version "^2" --locked).
# The first build downloads NSIS, which Tauri uses to make the installer.
# Run from anywhere:  powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path "Cargo.toml" -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Building ATV $version"

Push-Location "crates\atv-gui"
cargo tauri build
if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed" }
Pop-Location
cargo build --release -p atv-cli
if ($LASTEXITCODE -ne 0) { throw "building the command-line tool failed" }

$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force $dist | Out-Null
$setup = "target\release\bundle\nsis\ATV_${version}_x64-setup.exe"
Copy-Item $setup $dist -Force

# Portable: the two programs, the licence and a short note.
$stage = Join-Path $dist "ATV-$version-portable"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory $stage | Out-Null
Copy-Item "target\release\AnythingToVideo.exe" $stage
Copy-Item "target\release\atv.exe" $stage
Copy-Item "LICENSE" (Join-Path $stage "LICENSE.txt")
@"
ATV $version, portable

AnythingToVideo.exe  the app. Double-click to start; the setup wizard installs
                     ffmpeg (and, if you want YouTube links, yt-dlp and Deno)
                     on first run.
atv.exe              the command-line tool. Run "atv --help" in a terminal.

Settings and downloaded tools are kept in your user folders
(%APPDATA%\ATV and %LOCALAPPDATA%\ATV), not next to these files.
Needs Windows 10 or 11 with Microsoft Edge WebView2 (built into Windows 11).

Made by Hesham Nasser (@Hesham0-0Nasser). Licence: GPL-3.0 (LICENSE.txt).
"@ | Set-Content -Encoding utf8 (Join-Path $stage "README.txt")
$zip = Join-Path $dist "ATV_${version}_x64-portable.zip"
Remove-Item -Force $zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Remove-Item -Recurse -Force $stage

Get-ChildItem $dist | ForEach-Object { "{0}  {1:N1} MB" -f $_.Name, ($_.Length / 1MB) }
