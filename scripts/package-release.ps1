# Makes the release a player downloads: dist\Rox-box-<version>-windows-x64.zip (+ .sha256).
#   scripts\package-release.ps1            builds `cargo xtask dist` first
#   scripts\package-release.ps1 -NoBuild   packages the target\release-dist\victory.exe there is
# Steps: hermetic build; check that the binary names no build-machine path; check that every DLL
# it imports is part of Windows (or bundled); check that THIRD_PARTY_LICENSES.md is current;
# assemble the folder; run the packaged binary once; zip it and write the SHA-256.
# The version is `version` under [workspace.package] in Cargo.toml.
# -OutDir <dir>   where the folder and the zip go (default: dist\ in the repository, git-ignored)
# -Bundle <dll>   DLLs to ship next to victory.exe (none are needed today)
param(
    [switch]$NoBuild,
    [string]$OutDir,
    [string[]]$Bundle = @()
)
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $OutDir) { $OutDir = Join-Path $root 'dist' }

function Stop-Packaging([string]$Why) { Write-Host "PACKAGING FAILED: $Why"; exit 1 }

function Get-WorkspaceVersion([string]$Root) {
    $text = [System.IO.File]::ReadAllText((Join-Path $Root 'Cargo.toml'))
    $m = [regex]::Match($text, '(?ms)^\[workspace\.package\]\s*$.*?^\s*version\s*=\s*"([^"]+)"')
    if ($m.Success) { $m.Groups[1].Value } else { $null }
}

# The DLLs an image imports, as `victory inspect` lists them (delay-loaded ones too).
function Get-ImportedDlls([string]$Victory, [string]$Image) {
    $lines = @(& $Victory inspect $Image 2>$null)
    if ($LASTEXITCODE -ne 0) { return $null }
    $lines | ForEach-Object { [regex]::Match("$_", '^\s{2}(\S+\.dll)\s+\d+\s*$', 'IgnoreCase') } |
        Where-Object { $_.Success } | ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() } | Sort-Object -Unique
}

# Whether Windows itself provides `Dll` on every PC: an API set, or a file of System32 that is
# not one of the redistributable runtimes a development PC has there and a player's may not.
function Test-SystemDll([string]$Dll) {
    if ($Dll -match '^(api-ms-win-|ext-ms-)') { return $true }
    if ($Dll -match '^(vcruntime|msvcp|msvcr|concrt|vcomp|vccorlib|mfc|atl|ucrtbased|dxcompiler|dxil)') { return $false }
    Test-Path -LiteralPath (Join-Path $env:SystemRoot "System32\$Dll")
}

$version = Get-WorkspaceVersion $root
if (-not $version) { Stop-Packaging 'no version under [workspace.package] in Cargo.toml' }
$name = "Rox-box-$version-windows-x64"
$xtask = 'run', '--locked', '--quiet', '--package', 'xtask', '--'
$victory = Join-Path $root 'target\release-dist\victory.exe'

Push-Location $root
try {
    if (-not $NoBuild) {
        Write-Host '== hermetic distribution build (cargo xtask dist)'
        & cargo @xtask dist
        if ($LASTEXITCODE -ne 0) { Stop-Packaging 'the distribution build failed (scripts\build.ps1 -PrereqOnly checks the toolchain)' }
    }
    if (-not (Test-Path -LiteralPath $victory)) { Stop-Packaging "no ${victory}: run without -NoBuild" }
    Write-Host '== hermeticity (cargo xtask verify-dist)'
    & cargo @xtask verify-dist $victory
    if ($LASTEXITCODE -ne 0) { Stop-Packaging 'the hermeticity check did not pass (see above): the binary names a build-machine path or a symbol file, or the check could not run' }
} finally { Pop-Location }

Write-Host '== imported DLLs'
$dlls = Get-ImportedDlls $victory $victory
if (-not $dlls) { Stop-Packaging 'the binary could not list its own imports (victory inspect)' }
$bundled = @($Bundle | ForEach-Object { (Split-Path -Leaf $_).ToLowerInvariant() })
$needed = @()
foreach ($dll in $dlls) {
    $kind = $(if ($bundled -contains $dll) { 'bundled' } elseif (Test-SystemDll $dll) { 'Windows' } else { $needed += $dll; 'NOT PROVIDED' })
    Write-Host ("  {0,-40} {1}" -f $dll, $kind)
}
if ($needed) { Stop-Packaging "the binary needs $($needed -join ', '), which Windows does not provide: link it statically, or ship it with -Bundle <path>" }

Write-Host '== third-party licenses'
& powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'third-party-licenses.ps1') -Check
if ($LASTEXITCODE -ne 0) { Stop-Packaging 'THIRD_PARTY_LICENSES.md is out of date: run scripts\third-party-licenses.ps1' }

Write-Host "== assembling $name"
$stage = Join-Path $OutDir $name
$zip = Join-Path $OutDir "$name.zip"
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
$files = @(
    $victory,
    (Join-Path $root 'scripts\play.ps1'),
    (Join-Path $root 'scripts\play.cmd'),
    (Join-Path $root 'victory.example.toml'),
    (Join-Path $root 'LICENSE'),
    (Join-Path $root 'THIRD_PARTY_LICENSES.md'),
    (Join-Path $root 'QUICKSTART.txt')
) + @($Bundle)
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { Stop-Packaging "missing $file" }
    Copy-Item -LiteralPath $file -Destination $stage
}

# Nothing that is packaged may name this PC: the user's folder or the repository's.
$private = @($env:USERPROFILE, $root) | Where-Object { $_ -and $_.Length -ge 4 }
foreach ($file in Get-ChildItem -LiteralPath $stage -File | Where-Object { $_.Extension -ne '.exe' -and $_.Extension -ne '.dll' }) {
    $text = [System.IO.File]::ReadAllText($file.FullName)
    foreach ($p in $private) {
        if ($text.IndexOf($p, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or $text.IndexOf($p.Replace('\', '/'), [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            Stop-Packaging "$($file.Name) names a folder of this PC ($p)"
        }
    }
}

# The packaged binary starts, from its own folder, and is the version being packaged.
Push-Location $stage
try { $said = "$(& (Join-Path $stage 'victory.exe') --version 2>&1)" } finally { Pop-Location }
if ($LASTEXITCODE -ne 0 -or $said -notmatch [regex]::Escape($version)) { Stop-Packaging "the packaged victory.exe did not report version $version (it said: $said)" }

Write-Host "== $name.zip"
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
# Entry names are written here, with forward slashes: what Windows PowerShell's own zipping
# writes (backslashes) other unzip tools read as one odd file name.
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$stream = [System.IO.File]::Open($zip, [System.IO.FileMode]::CreateNew)
try {
    $archive = New-Object System.IO.Compression.ZipArchive($stream, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        # Every entry carries one fixed time: a zip stores the packaging PC's local clock time
        # (its time zone, read against the commit time), and the same input gives the same zip.
        $fixedTime = New-Object System.DateTimeOffset(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
        foreach ($file in Get-ChildItem -LiteralPath $stage -File | Sort-Object Name) {
            $entry = $archive.CreateEntry("$name/$($file.Name)", [System.IO.Compression.CompressionLevel]::Optimal)
            $entry.LastWriteTime = $fixedTime
            $to = $entry.Open()
            try {
                $from = [System.IO.File]::OpenRead($file.FullName)
                try { $from.CopyTo($to) } finally { $from.Dispose() }
            } finally { $to.Dispose() }
        }
    } finally { $archive.Dispose() }
} finally { $stream.Dispose() }
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
[System.IO.File]::WriteAllText("$zip.sha256", "$hash  $name.zip`n", (New-Object System.Text.UTF8Encoding($false)))

Write-Host ''
Write-Host "Packaged $name ($said)"
Write-Host "  folder : $stage"
Write-Host ("  zip    : {0} ({1:N1} MB)" -f $zip, ((Get-Item -LiteralPath $zip).Length / 1MB))
Write-Host "  sha256 : $hash"
