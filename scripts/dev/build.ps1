# Build the desktop app. Closes any running instance first: Windows holds
# an exclusive lock on the running executable, so linking a fresh
# target/(debug|release)/zeron.exe fails with "Access is denied" (os error 5)
# while the app is open.
param(
    [ValidateSet('Debug', 'Release')]
    [string]$Config = 'Release'
)
$ErrorActionPreference = 'Stop'
if (-not ($IsWindows -or $env:OS -eq 'Windows_NT')) {
    throw 'build.ps1 is Windows-only.'
}
Write-Output "Building $Config..."

$stopped = @(Get-Process zeron -ErrorAction SilentlyContinue)
if ($stopped.Count -gt 0) {
    Write-Output "Stopping running zeron ($($stopped.Count) process(es))..."
    Stop-Process -Name zeron -Force
    # WaitForExit(ms) works on Windows PowerShell 5.1 and PowerShell 7;
    # Wait-Process -Timeout is 7-only.
    foreach ($p in $stopped) {
        try { [void]$p.WaitForExit(15000) } catch { }
    }
}

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot '../..')

$cargoArgs = @('build', '--locked', '-p', 'zeron')
if ($Config -eq 'Release') { $cargoArgs += '--release' }

# Run from the repo root regardless of where the script was invoked from.
Push-Location $RepoRoot
try {
    $elapsed = Measure-Command { & cargo @cargoArgs }
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}

$exe = Join-Path $RepoRoot "target/$($Config.ToLower())/zeron.exe"
$info = Get-Item $exe
Write-Output "Built $exe ($([math]::Round($info.Length / 1MB, 1)) MB) in $($elapsed.ToString('hh\:mm\:ss'))"
