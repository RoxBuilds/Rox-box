# Builds Rox-box in one command, from any current directory. Checks what a build needs first
# and says how to get what is missing.
#   scripts\build.ps1            debug build          -> target\debug\victory.exe
#   scripts\build.ps1 -Release   optimised build      -> target\release\victory.exe
#   scripts\build.ps1 -Diag      optimised build with the diagnostics feature -> target\diag\victory.exe
#   scripts\build.ps1 -Dist      hermetic distribution build (`cargo xtask dist`) -> target\release-dist\victory.exe
#   scripts\build.ps1 -Test      the workspace's tests
#   scripts\build.ps1 -Check     what CI checks besides the tests: the asset policy, clippy with warnings as
#                                errors (the default features, then --all-features) and the launcher's self-test
#   scripts\build.ps1 -Check -Test   everything CI runs (.github\workflows\ci.yml)
# Several may be given; they run in the order above. Every Cargo command uses --locked: the
# dependency versions are exactly those of Cargo.lock.
# -Jobs <n>     build with n parallel jobs (default: Cargo's)
# -PrereqOnly   only check the prerequisites
param(
    [switch]$Release,
    [switch]$Diag,
    [switch]$Dist,
    [switch]$Test,
    [switch]$Check,
    [ValidateRange(1, 512)][int]$Jobs,
    [switch]$PrereqOnly
)
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

# The pinned toolchain: `channel = "..."` of rust-toolchain.toml.
function Get-PinnedToolchain([string]$Root) {
    $file = Join-Path $Root 'rust-toolchain.toml'
    if (-not (Test-Path -LiteralPath $file)) { return $null }
    $m = [regex]::Match([System.IO.File]::ReadAllText($file), '(?m)^\s*channel\s*=\s*"([^"]+)"')
    if ($m.Success) { $m.Groups[1].Value } else { $null }
}

# The Visual Studio (or Build Tools) installation that has the x64 C++ compiler and linker.
function Find-MsvcTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere)) { return $null }
    $path = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    if ($LASTEXITCODE -eq 0 -and $path) { @($path)[0] } else { $null }
}

# The newest Windows SDK that has the x64 import libraries the linker needs.
function Find-WindowsSdk {
    foreach ($key in 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots', 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots') {
        $kits = $null
        try { $kits = (Get-ItemProperty -Path $key -ErrorAction Stop).KitsRoot10 } catch { }
        if (-not $kits) { continue }
        $lib = Join-Path $kits 'Lib'
        if (-not (Test-Path -LiteralPath $lib)) { continue }
        $version = Get-ChildItem -LiteralPath $lib -Directory | Sort-Object Name -Descending |
            Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'um\x64\kernel32.lib') } | Select-Object -First 1
        if ($version) { return $version.Name }
    }
    $null
}

# Every prerequisite as a line; returns the number missing.
function Test-Prerequisites([string]$Root) {
    $missing = 0
    function Say([bool]$ok, [string]$what, [string]$detail, [string]$fix) {
        if ($ok) { Write-Host ("[  OK   ] {0,-22} {1}" -f $what, $detail) }
        else { Write-Host ("[MISSING] {0,-22} {1}" -f $what, $detail); Write-Host ("          {0,-22} fix: {1}" -f '', $fix) }
    }
    $rustup = Get-Command rustup -ErrorAction SilentlyContinue
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $rustup -or -not $cargo) {
        Say $false 'Rust (rustup, cargo)' 'not found on PATH' 'install rustup from https://rustup.rs (default options), then open a new terminal'
        $missing++
    } else {
        Say $true 'Rust (rustup, cargo)' $rustup.Source ''
        $pinned = Get-PinnedToolchain $Root
        $installed = @(& rustup toolchain list 2>$null)
        if (-not $pinned) {
            Say $false 'Pinned toolchain' 'rust-toolchain.toml names no channel' 'restore rust-toolchain.toml from the repository'
            $missing++
        } elseif ($installed | Where-Object { $_ -like "$pinned-*" }) {
            Say $true 'Pinned toolchain' "$pinned (rust-toolchain.toml)" ''
        } else {
            Write-Host "Installing the pinned Rust toolchain $pinned (rust-toolchain.toml)..."
            Push-Location $Root
            try { & rustup toolchain install } finally { Pop-Location }
            if ($LASTEXITCODE -eq 0) { Say $true 'Pinned toolchain' "$pinned (installed now)" '' }
            else {
                Say $false 'Pinned toolchain' "$pinned could not be installed" "run  rustup toolchain install $pinned  and read what it says (an internet connection is needed)"
                $missing++
            }
        }
    }
    $msvc = Find-MsvcTools
    if ($msvc) { Say $true 'MSVC C++ build tools' $msvc '' }
    else {
        Say $false 'MSVC C++ build tools' 'no x64 C++ compiler and linker found' 'install "Build Tools for Visual Studio" (https://visualstudio.microsoft.com/downloads/) with the "Desktop development with C++" workload'
        $missing++
    }
    $sdk = Find-WindowsSdk
    if ($sdk) { Say $true 'Windows SDK' $sdk '' }
    else {
        Say $false 'Windows SDK' 'no Windows 10/11 SDK with x64 libraries found' 'in the Visual Studio Installer, add a "Windows 11 SDK" (or "Windows 10 SDK") component to the C++ workload'
        $missing++
    }
    $missing
}

# Runs Cargo in the workspace root; stops the script if it fails.
function Invoke-Cargo([string]$What, [string[]]$Arguments) {
    Write-Host ''
    Write-Host "== ${What}: cargo $($Arguments -join ' ')"
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $What (cargo exit code $LASTEXITCODE)."
        exit $LASTEXITCODE
    }
}

$missing = Test-Prerequisites $root
if ($missing -gt 0) {
    Write-Host ''
    Write-Host "$missing prerequisite(s) missing: fix them as said above, open a new terminal and run this again."
    exit 1
}
if ($PrereqOnly) { exit 0 }

$jobArgs = @($(if ($Jobs) { '--jobs', "$Jobs" }))
$xtask = 'run', '--locked', '--quiet', '--package', 'xtask'
$built = @()
Push-Location $root
try {
    if ($Jobs) { $env:CARGO_BUILD_JOBS = "$Jobs" }
    if (-not ($Release -or $Diag -or $Dist -or $Test -or $Check)) {
        Invoke-Cargo 'debug build' (@('build', '--locked', '-p', 'victory') + $jobArgs)
        $built += 'target\debug\victory.exe'
    }
    if ($Release) {
        Invoke-Cargo 'release build' (@('build', '--locked', '--release', '-p', 'victory') + $jobArgs)
        $built += 'target\release\victory.exe'
    }
    if ($Diag) {
        Invoke-Cargo 'diagnostics build' (@('build', '--locked', '--profile', 'diag', '-p', 'victory', '--features', 'diagnostics') + $jobArgs)
        $built += 'target\diag\victory.exe'
    }
    if ($Dist) {
        Invoke-Cargo 'hermetic distribution build' ($xtask + @('--', 'dist'))
        $built += 'target\release-dist\victory.exe'
    }
    if ($Test) {
        Invoke-Cargo 'tests' (@('test', '--locked', '--workspace') + $jobArgs)
    }
    if ($Check) {
        Invoke-Cargo 'asset policy' ($xtask + @('--', 'policy'))
        Invoke-Cargo 'clippy' (@('clippy', '--locked', '--workspace', '--all-targets') + $jobArgs + @('--', '-D', 'warnings'))
        Invoke-Cargo 'clippy, diagnostics build' (@('clippy', '--locked', '--workspace', '--all-targets', '--all-features') + $jobArgs + @('--', '-D', 'warnings'))
        Write-Host ''
        Write-Host '== launcher self-test: scripts\play.ps1 -SelfTest'
        & powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'play.ps1') -SelfTest
        if ($LASTEXITCODE -ne 0) {
            Write-Host "FAILED: launcher self-test (exit code $LASTEXITCODE)."
            exit $LASTEXITCODE
        }
    }
} finally { Pop-Location }

Write-Host ''
Write-Host 'Done.'
foreach ($exe in $built) { Write-Host "  $(Join-Path $root $exe)" }
if ($built) { Write-Host 'Next: copy victory.example.toml to victory.toml, set game_dir, then  scripts\play.cmd -Check' }
