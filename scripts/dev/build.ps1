# Build the desktop app. Closes any running instance first: Windows holds
# an exclusive lock on the running executable, so linking a fresh
# target/(debug|release)/zeron.exe fails with "Access is denied" (os error 5)
# while the app is open.
#
#   ./scripts/dev/build.ps1              # interactive menu (Debug / Release)
#   ./scripts/dev/build.ps1 -Config Debug
#   ./scripts/dev/build.ps1 -Config Release
#   ./scripts/dev/build.ps1 -Portable     # Release build + portable zip
#   ./scripts/dev/build.ps1 -Setup        # Release build + setup installer
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Config,

    [switch]$Interactive,

    # Package after building (both imply a Release build): portable zip
    # only, or the per-user setup installer (needs Inno Setup 6).
    [switch]$Portable,
    [switch]$Setup,

    # Update feed baked into the packaged output.
    [string]$ReleasesUrl = 'https://github.com/K0oRui/zeron/releases/latest/download'
)
$ErrorActionPreference = 'Stop'
if (-not ($IsWindows -or $env:OS -eq 'Windows_NT')) {
    throw 'build.ps1 is Windows-only.'
}

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot '../..')

$Version = ''
try {
    $rootToml = Get-Content (Join-Path $RepoRoot 'Cargo.toml') -Raw -ErrorAction Stop
    if ($rootToml -match '(?m)^version\s*=\s*"([^"]+)"') { $Version = $Matches[1] }
} catch { }

$UI_WIDTH = 52

function Show-Banner {
    # Left-aligned on purpose: no right border, so there is nothing to
    # misalign regardless of console font or wide characters.
    Write-Host ''
    Write-Host '  ZERON ' -NoNewline -ForegroundColor Cyan
    Write-Host 'BUILD' -NoNewline -ForegroundColor White
    if ($Version) { Write-Host "  v$Version" -NoNewline -ForegroundColor DarkGray }
    Write-Host ''
    Write-Host ('  ' + ('─' * ($UI_WIDTH - 6))) -ForegroundColor DarkGray
    Write-Host '  Control your coding agents · local by default' -ForegroundColor DarkGray
    Write-Host ''
}

function Write-Section([string]$Text) {
    $fill = $UI_WIDTH - $Text.Length - 1
    if ($fill -lt 3) { $fill = 3 }
    Write-Host "  ── $Text $('─' * $fill)" -ForegroundColor Cyan
    Write-Host ''
}

function Show-Menu {
    Show-Banner
    Write-Host '  Pick a configuration:' -ForegroundColor Gray
    Write-Host ''
    Write-Host '    [1] ' -NoNewline -ForegroundColor Cyan
    Write-Host 'Debug' -ForegroundColor White
    Write-Host '        cargo build --locked -p zeron' -ForegroundColor DarkGray
    Write-Host ''
    Write-Host '    [2] ' -NoNewline -ForegroundColor Cyan
    Write-Host 'Release' -ForegroundColor White
    Write-Host '        cargo build --release --locked -p zeron' -ForegroundColor DarkGray
    Write-Host ''
    Write-Host '    [3] ' -NoNewline -ForegroundColor Cyan
    Write-Host 'Portable' -ForegroundColor White
    Write-Host '        Release build + portable zip (no Inno Setup needed)' -ForegroundColor DarkGray
    Write-Host ''
    Write-Host '    [4] ' -NoNewline -ForegroundColor Cyan
    Write-Host 'Setup' -ForegroundColor White
    Write-Host '        Release build + setup installer (needs Inno Setup 6)' -ForegroundColor DarkGray
    Write-Host ''
    Write-Host '    [0] ' -NoNewline -ForegroundColor Cyan
    Write-Host 'Exit' -ForegroundColor White
    Write-Host ''
    Write-Host '  Just press Enter for Release.' -ForegroundColor DarkGray
    Write-Host ''
}

function Invoke-ZeronBuild([string]$Selected) {
    Write-Section "$Selected build"

    $stopped = @(Get-Process zeron -ErrorAction SilentlyContinue)
    if ($stopped.Count -gt 0) {
        Write-Host "  › Stopping $($stopped.Count) running zeron process(es)…" -ForegroundColor Yellow
        Stop-Process -Name zeron -Force
        # WaitForExit(ms) works on Windows PowerShell 5.1 and PowerShell 7;
        # Wait-Process -Timeout is 7-only.
        foreach ($p in $stopped) {
            try { [void]$p.WaitForExit(15000) } catch { }
        }
        Write-Host '  ✓ Lock released, rebuilding.' -ForegroundColor DarkGray
        Write-Host ''
    }

    $cargoArgs = @('build')
    if ($Selected -eq 'Release') { $cargoArgs += '--release' }
    $cargoArgs += '--locked', '-p', 'zeron'

    Write-Host "  › cargo $($cargoArgs -join ' ')" -ForegroundColor Gray
    Write-Host ''

    # Run from the repo root regardless of where the script was invoked from.
    Push-Location $RepoRoot
    try {
        $elapsed = Measure-Command { & cargo @cargoArgs }
        if ($LASTEXITCODE -ne 0) { throw "cargo exited with code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }

    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $RepoRoot 'target' }
    $short = "target/$($Selected.ToLower())/zeron.exe"
    $exe = Join-Path $targetDir "$($Selected.ToLower())/zeron.exe"
    $info = Get-Item $exe
    $size = [math]::Round($info.Length / 1MB, 1)
    $time = $elapsed.ToString('hh\:mm\:ss')

    $bar = '─' * $UI_WIDTH
    Write-Host "  ✓ $Selected build succeeded" -ForegroundColor Green
    Write-Host "    $short  ·  $size MB  ·  $time" -ForegroundColor DarkGray
    Write-Host "  $bar" -ForegroundColor DarkGray
    Write-Host ''
}

function Invoke-ZeronPackage([switch]$SkipInstaller) {
    if ($SkipInstaller) {
        Write-Section 'Portable package'
    } else {
        Write-Section 'Setup installer'
    }
    $packager = Join-Path $RepoRoot 'scripts/package/package-windows.ps1'
    $pkgArgs = @{ ReleasesUrl = $ReleasesUrl }
    if ($SkipInstaller) { $pkgArgs['SkipInstaller'] = $true }
    Write-Host "  › package-windows.ps1 -ReleasesUrl $($pkgArgs['ReleasesUrl'])$(if ($SkipInstaller) { ' -SkipInstaller' })" -ForegroundColor Gray
    Write-Host ''
    & $packager @pkgArgs
    if ($LASTEXITCODE -ne 0) { throw "packaging exited with code $LASTEXITCODE" }
    Write-Host '  ✓ Packaging succeeded' -ForegroundColor Green
    Write-Host ''
}

# --- interactive menu when -Config was not passed explicitly ---
$explicitConfig = $PSBoundParameters.ContainsKey('Config')
$wantMenu = $Interactive -or (-not $explicitConfig)
if ($wantMenu -and ([Console]::IsInputRedirected -or $env:CI)) {
    # Non-interactive shell (CI): fall back to a plain Release build.
    $wantMenu = $false
    if (-not $explicitConfig) { $Config = 'Release' }
}

if ($wantMenu) {
    while ($true) {
        Show-Menu
        $answer = (Read-Host '  Select').Trim()
        if ($answer -eq '') { $answer = '2' }
        switch ($answer) {
            '1' {
                try { Invoke-ZeronBuild 'Debug' }
                catch { Write-Host "  ✗ Build failed: $_" -ForegroundColor Red; Write-Host '' }
            }
            '2' {
                try { Invoke-ZeronBuild 'Release' }
                catch { Write-Host "  ✗ Build failed: $_" -ForegroundColor Red; Write-Host '' }
            }
            '3' {
                try { Invoke-ZeronBuild 'Release'; Invoke-ZeronPackage -SkipInstaller }
                catch { Write-Host "  ✗ Packaging failed: $_" -ForegroundColor Red; Write-Host '' }
            }
            '4' {
                try { Invoke-ZeronBuild 'Release'; Invoke-ZeronPackage }
                catch { Write-Host "  ✗ Packaging failed: $_" -ForegroundColor Red; Write-Host '' }
            }
            '0' { return }
            'q' { return }
            default {
                Write-Host "  ✗ Unknown choice '$answer' — pick 1, 2, 3, 4, or 0." -ForegroundColor Red
                Write-Host ''
            }
        }
        Read-Host '  Press Enter to continue' | Out-Null
    }
    return
}

if ($Portable -or $Setup) {
    if (-not $explicitConfig) { $Config = 'Release' }
    try {
        Invoke-ZeronBuild $Config
        if ($Portable -and -not $Setup) {
            Invoke-ZeronPackage -SkipInstaller
        } else {
            Invoke-ZeronPackage
        }
    } catch {
        Write-Host "  ✗ Packaging failed: $_" -ForegroundColor Red
        Write-Host ''
        exit 1
    }
    return
}

try {
    Invoke-ZeronBuild $Config
} catch {
    Write-Host "  ✗ Build failed: $_" -ForegroundColor Red
    Write-Host ''
    exit 1
}
