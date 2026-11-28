# Makes the release a player downloads: dist\Rox-box-<version>-windows-x64.zip (+ .sha256).
#   scripts\package-release.ps1            builds `cargo xtask dist` and `cargo xtask dist-launcher` first
#   scripts\package-release.ps1 -NoBuild   packages the target\release-dist\victory.exe and the
#                                          launcher\target\release\roxbox-launcher.exe there are
# Two binaries are shipped: the engine (victory.exe) and the launcher (roxbox-launcher.exe, the
# window over it; a workspace of its own in launcher\). Both get the same treatment.
# Steps: hermetic builds; check that neither binary names a build-machine path; check that every
# DLL they import is part of Windows (or bundled); check that THIRD_PARTY_LICENSES.md is current;
# assemble the folder; run each packaged binary once; zip it and write the SHA-256.
# The version is `version` under [workspace.package] in Cargo.toml.
# -OutDir <dir>   where the folder and the zip go (default: dist\ in the repository, git-ignored)
# -Bundle <dll>   DLLs to ship next to victory.exe (none are needed today)
# -Installer      also build the installer (needs Inno Setup 6's iscc.exe): its absence is an error;
#                 without the switch the installer is built when iscc.exe is found, else a note is printed
param(
    [switch]$NoBuild,
    [switch]$Installer,
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
$launcher = Join-Path $root 'launcher\target\release\roxbox-launcher.exe'

Push-Location $root
try {
    if (-not $NoBuild) {
        Write-Host '== hermetic distribution build (cargo xtask dist)'
        & cargo @xtask dist
        if ($LASTEXITCODE -ne 0) { Stop-Packaging 'the distribution build failed (scripts\build.ps1 -PrereqOnly checks the toolchain)' }
    }
    if (-not $NoBuild) {
        Write-Host '== hermetic distribution build of the launcher (cargo xtask dist-launcher)'
        & cargo @xtask dist-launcher
        if ($LASTEXITCODE -ne 0) { Stop-Packaging 'the launcher''s distribution build failed, or what it built is not hermetic (see above)' }
    }
    foreach ($binary in $victory, $launcher) {
        if (-not (Test-Path -LiteralPath $binary)) { Stop-Packaging "no ${binary}: run without -NoBuild" }
        Write-Host "== hermeticity of $(Split-Path -Leaf $binary) (cargo xtask verify-dist)"
        & cargo @xtask verify-dist $binary
        if ($LASTEXITCODE -ne 0) { Stop-Packaging "the hermeticity check of $(Split-Path -Leaf $binary) did not pass (see above): it names a build-machine path or a symbol file, or the check could not run" }
    }
} finally { Pop-Location }

$bundled = @($Bundle | ForEach-Object { (Split-Path -Leaf $_).ToLowerInvariant() })
foreach ($binary in $victory, $launcher) {
    $leaf = Split-Path -Leaf $binary
    Write-Host "== DLLs $leaf imports"
    # The engine reads any 64-bit image's imports, the launcher's as its own.
    $dlls = Get-ImportedDlls $victory $binary
    if (-not $dlls) { Stop-Packaging "the imports of $leaf could not be listed (victory inspect)" }
    $needed = @()
    foreach ($dll in $dlls) {
        $kind = $(if ($bundled -contains $dll) { 'bundled' } elseif (Test-SystemDll $dll) { 'Windows' } else { $needed += $dll; 'NOT PROVIDED' })
        Write-Host ("  {0,-40} {1}" -f $dll, $kind)
    }
    if ($needed) { Stop-Packaging "$leaf needs $($needed -join ', '), which Windows does not provide: link it statically, or ship it with -Bundle <path>" }
}

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
    $launcher,
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

# The packaged launcher starts too, and finds the engine beside it with nothing set up: `--probe`
# prints what it found and opens no window. Its output goes through a file: a windowed program
# has no console of its own to write to.
$probeOut = Join-Path $OutDir 'launcher-probe.txt'
$savedHome = $env:ROXBOX_HOME
$env:ROXBOX_HOME = $null
try {
    $probe = Start-Process -FilePath (Join-Path $stage 'roxbox-launcher.exe') -ArgumentList '--probe' -WorkingDirectory $stage -RedirectStandardOutput $probeOut -NoNewWindow -Wait -PassThru
    $probeSaid = $(if (Test-Path -LiteralPath $probeOut) { [System.IO.File]::ReadAllText($probeOut) } else { '' })
} finally {
    $env:ROXBOX_HOME = $savedHome
    Remove-Item -LiteralPath $probeOut -Force -ErrorAction SilentlyContinue
}
if ($probe.ExitCode -ne 0 -or $probeSaid -notmatch '(?m)^home: Release' -or $probeSaid -notmatch '(?m)^victory\.exe: true') {
    Stop-Packaging "the packaged roxbox-launcher.exe did not find the engine beside it (exit code $($probe.ExitCode); it said: $probeSaid)"
}

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

# The installer: the same folder as the zip, as one per-user setup program (dist\installer\rox-box.iss).
$setup = Join-Path $OutDir "$name-setup.exe"
$iscc = $null
$found = Get-Command iscc.exe -ErrorAction SilentlyContinue | Select-Object -First 1
if ($found) { $iscc = $found.Source }
foreach ($dir in @(${env:ProgramFiles(x86)}, $env:ProgramFiles, (Join-Path $env:LOCALAPPDATA 'Programs')) | Where-Object { $_ }) {
    if (-not $iscc) {
        $candidate = Join-Path $dir 'Inno Setup 6\ISCC.exe'
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { $iscc = $candidate }
    }
}
$setupHash = $null
if (-not $iscc) {
    if ($Installer) { Stop-Packaging 'the installer was asked for (-Installer) but Inno Setup 6 (iscc.exe) was not found: install it (winget install --id JRSoftware.InnoSetup -e)' }
    Write-Host '== installer skipped: Inno Setup 6 (iscc.exe) not found (winget install --id JRSoftware.InnoSetup -e), only the zip was made'
} else {
    Write-Host "== $name-setup.exe (Inno Setup: $iscc)"
    if (Test-Path -LiteralPath $setup) { Remove-Item -LiteralPath $setup -Force }
    # The numeric part of the version for the setup's file-version resource (1.2.3 of 1.2.3-rc1).
    $numeric = [regex]::Match($version, '^\d+\.\d+\.\d+').Value
    $isccArgs = @('/Qp', "/DAppVersion=$version", "/DSourceDir=$stage", "/O$OutDir")
    if ($numeric) { $isccArgs += "/DAppVersionNumeric=$numeric" }
    & $iscc @isccArgs (Join-Path $root 'dist\installer\rox-box.iss')
    if ($LASTEXITCODE -ne 0) { Stop-Packaging "Inno Setup failed to build the installer (exit code $LASTEXITCODE)" }
    if (-not (Test-Path -LiteralPath $setup -PathType Leaf)) { Stop-Packaging "Inno Setup did not write $setup" }
    $setupHash = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText("$setup.sha256", "$setupHash  $name-setup.exe`n", (New-Object System.Text.UTF8Encoding($false)))
}

Write-Host ''
Write-Host "Packaged $name ($said)"
Write-Host "  folder : $stage"
Write-Host ("  zip    : {0} ({1:N1} MB)" -f $zip, ((Get-Item -LiteralPath $zip).Length / 1MB))
Write-Host "  sha256 : $hash"
if ($setupHash) {
    Write-Host ("  setup  : {0} ({1:N1} MB)" -f $setup, ((Get-Item -LiteralPath $setup).Length / 1MB))
    Write-Host "  sha256 : $setupHash"
}
