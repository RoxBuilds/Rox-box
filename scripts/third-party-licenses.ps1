# Writes THIRD_PARTY_LICENSES.md: THIRD_PARTY_NOTICES.md word for word, then every crate that is
# built into victory.exe or roxbox-launcher.exe (the resolved dependencies of the `victory` package
# and of the launcher's own workspace, launcher\, for Windows x64, with their build-time
# dependencies; nothing used only by tests) grouped by licence, then each distinct licence text
# those crates ship. Everything comes from `cargo metadata --locked` and the crate sources Cargo
# downloaded: the same two Cargo.lock files give the same file.
#   scripts\third-party-licenses.ps1          writes the file
#   scripts\third-party-licenses.ps1 -Check   fails if the file is not what would be written
param(
    [switch]$Check,
    [string]$OutFile
)
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $OutFile) { $OutFile = Join-Path $root 'THIRD_PARTY_LICENSES.md' }
$target = 'x86_64-pc-windows-msvc'
# The same order on every PC, whatever its language settings.
[System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture

function ConvertFrom-BigJson([string]$Json) {
    try { return ConvertFrom-Json -InputObject $Json -ErrorAction Stop } catch { }
    # Windows PowerShell 5.1 refuses documents over 2 MB.
    Add-Type -AssemblyName System.Web.Extensions
    $reader = New-Object System.Web.Script.Serialization.JavaScriptSerializer
    $reader.MaxJsonLength = [int]::MaxValue
    $reader.RecursionLimit = 1000
    $reader.DeserializeObject($Json)
}

# The third-party crates one binary is built from: the package `Package` of the workspace at
# `Manifest`, its normal and build dependencies followed all the way down.
function Get-BinaryCrates([string]$Manifest, [string]$Package) {
    $json = (& cargo metadata --locked --format-version 1 --filter-platform $target --manifest-path $Manifest) -join "`n"
    if ($LASTEXITCODE -ne 0 -or -not $json) { Write-Host "cargo metadata failed for $Manifest (run scripts\build.ps1 -PrereqOnly to check the toolchain)."; exit 1 }
    $meta = ConvertFrom-BigJson $json
    $packages = @{}
    foreach ($p in $meta.packages) { $packages[$p.id] = $p }
    $nodes = @{}
    foreach ($n in $meta.resolve.nodes) { $nodes[$n.id] = $n }
    $start = @($meta.packages | Where-Object { $_.name -eq $Package -and -not $_.source })
    if ($start.Count -ne 1) { Write-Host "$Manifest has no ``$Package`` package."; exit 1 }
    $seen = @{}
    $queue = New-Object System.Collections.Queue
    $queue.Enqueue($start[0].id)
    while ($queue.Count -gt 0) {
        $id = $queue.Dequeue()
        if ($seen.ContainsKey($id)) { continue }
        $seen[$id] = $true
        foreach ($dep in $nodes[$id].deps) {
            $used = $false
            foreach ($kind in $dep.dep_kinds) { if ($kind.kind -ne 'dev') { $used = $true } }
            if ($used) { $queue.Enqueue($dep.pkg) }
        }
    }
    @($seen.Keys | ForEach-Object { $packages[$_] } | Where-Object { $_.source })
}

# Both binaries of a release: the engine, and the launcher (a workspace of its own). A crate both
# are built from is listed once.
$byNameAndVersion = @{}
foreach ($binary in @(@{ Manifest = (Join-Path $root 'Cargo.toml'); Package = 'victory' }, @{ Manifest = (Join-Path $root 'launcher\Cargo.toml'); Package = 'roxbox-launcher' })) {
    foreach ($c in (Get-BinaryCrates $binary.Manifest $binary.Package)) { $byNameAndVersion["$($c.name) $($c.version)"] = $c }
}
$crates = @($byNameAndVersion.Values | Sort-Object { $_.name.ToLowerInvariant() }, { $_.version })
if ($crates.Count -eq 0) { Write-Host 'No third-party crates found: is the dependency graph resolved?'; exit 1 }

function Get-Text([string]$Path) {
    $text = [System.IO.File]::ReadAllText($Path) -replace "`r`n", "`n" -replace "`r", "`n"
    $lines = $text.Split("`n") | ForEach-Object { $_.TrimEnd() }
    (($lines -join "`n").Trim("`n"))
}

$sha = [System.Security.Cryptography.SHA256]::Create()
function Get-TextKey([string]$Text) {
    $flat = ($Text -replace '\s+', ' ').Trim().ToLowerInvariant()
    -join ($sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($flat)) | ForEach-Object { $_.ToString('x2') })
}

# Each distinct licence text, with the crates that ship it.
$texts = [ordered]@{}
$without = @()
foreach ($c in $crates) {
    $dir = Split-Path -Parent $c.manifest_path
    $files = @(Get-ChildItem -LiteralPath $dir -File | Where-Object { $_.Name -match '^(LICEN[SC]E|COPYING|COPYRIGHT|UNLICENSE|NOTICE)' } | Sort-Object Name)
    if ($files.Count -eq 0) { $without += "$($c.name) $($c.version)"; continue }
    foreach ($f in $files) {
        $text = Get-Text $f.FullName
        if (-not $text) { continue }
        $key = Get-TextKey $text
        if (-not $texts.Contains($key)) { $texts[$key] = @{ Text = $text; Name = $f.Name; Crates = @() } }
        $texts[$key].Crates += "$($c.name) $($c.version) ($($f.Name))"
    }
}

$out = New-Object System.Collections.Generic.List[string]
$out.Add('# Third-Party Licenses')
$out.Add('')
$out.Add('<!-- Generated by scripts/third-party-licenses.ps1 from Cargo.lock. Do not edit: run the script again. -->')
$out.Add('')
$out.Add('This file accompanies every binary distribution of Rox-box. Part 1 repeats')
$out.Add('THIRD_PARTY_NOTICES.md (upstream lineage and ported code). Part 2 lists every')
$out.Add('third-party Rust crate compiled into `victory.exe` or `roxbox-launcher.exe`, by')
$out.Add('license. Part 3 holds the license texts those crates ship. Rox-box itself is')
$out.Add('licensed under `GPL-3.0-or-later` (see LICENSE).')
$out.Add('')
$out.Add('---')
$out.Add('')
$out.Add('## Part 1: THIRD_PARTY_NOTICES.md')
$out.Add('')
$notices = Join-Path $root 'THIRD_PARTY_NOTICES.md'
if (-not (Test-Path -LiteralPath $notices)) { Write-Host "No $notices."; exit 1 }
$out.Add((Get-Text $notices))
$out.Add('')
$out.Add('---')
$out.Add('')
$out.Add("## Part 2: Rust crates compiled into the binaries ($($crates.Count))")
$out.Add('')
$groups = $crates | Group-Object { if ($_.license) { $_.license } elseif ($_.license_file) { "see the crate's license file ($($_.license_file))" } else { 'not declared' } } | Sort-Object Name
foreach ($g in $groups) {
    $out.Add("### $($g.Name)")
    $out.Add('')
    $out.Add('| Crate | Version | Authors | Repository |')
    $out.Add('| --- | --- | --- | --- |')
    foreach ($c in $g.Group) {
        $authors = (@($c.authors) | Where-Object { $_ } | ForEach-Object { ($_ -replace '\s*<[^>]*>', '').Trim() }) -join ', '
        if (-not $authors) { $authors = '(not stated)' }
        $repo = $(if ($c.repository) { $c.repository } elseif ($c.homepage) { $c.homepage } else { "https://crates.io/crates/$($c.name)" })
        $out.Add("| $($c.name) | $($c.version) | $($authors -replace '\|', '/') | $repo |")
    }
    $out.Add('')
}
$out.Add('---')
$out.Add('')
$out.Add("## Part 3: License texts ($($texts.Count))")
$out.Add('')
if ($without) {
    $out.Add('These crates ship no license file of their own; the text of the license they')
    $out.Add('declare (Part 2) is among those below: ' + ($without -join ', ') + '.')
    $out.Add('')
}
$n = 0
foreach ($key in $texts.Keys) {
    $n++
    $t = $texts[$key]
    $out.Add("### Text $n")
    $out.Add('')
    $out.Add('Shipped by: ' + (($t.Crates | Sort-Object) -join ', ') + '.')
    $out.Add('')
    $out.Add('````text')
    $out.Add($t.Text)
    $out.Add('````')
    $out.Add('')
}
$text = (($out -join "`n").TrimEnd("`n")) + "`n"
$utf8 = New-Object System.Text.UTF8Encoding($false)

if ($Check) {
    $have = $(if (Test-Path -LiteralPath $OutFile) { [System.IO.File]::ReadAllText($OutFile) -replace "`r`n", "`n" } else { '' })
    if ($have -ne $text) {
        Write-Host "$OutFile is out of date: run scripts\third-party-licenses.ps1 and commit the result."
        exit 1
    }
    Write-Host "$OutFile is up to date ($($crates.Count) crates, $($texts.Count) license texts)."
    exit 0
}
[System.IO.File]::WriteAllBytes($OutFile, $utf8.GetBytes($text))
Write-Host "Wrote $OutFile ($($crates.Count) crates, $($texts.Count) license texts, $($utf8.GetByteCount($text)) bytes)."
