# Starts a title through Rox-box with optional runtime tuning and optional XML settings overrides.
#   play.cmd                       (or: play.ps1) runs the only .exe in the configured game folder
#   play.ps1 -Exe <path to title executable>
# Works from a release folder (victory.exe next to this script) and from the source tree
# (..\target\<build>\victory.exe), from any current directory.
# -Exe          the title executable; optional when the game folder (game_dir in victory.toml) holds exactly one .exe
# -Check        runs `victory check` (every prerequisite as OK / WARN / MISSING with the fix) and starts nothing
# -Fast         low-memory / fast preset: -Console SeriesS, -Display 1280x720, -Output 1280x720,
#               -Build release (each unless specified), plus VICTORY_CARD_AWAKE=0 and VICTORY_MWAITX_WAIT=1.
# -Safe         disables newer experimental optimizations
# -RayTracing   enables guest ray-tracing BVH builder translation (`VICTORY_BVH_BUILDER=1`)
# -Build        source tree only: `diag` (`target\diag`), `release`, `release-dist` or `debug`;
#               default: the first of those that is built
# -Console      `SeriesX` (default) or `SeriesS`
# -Display      reported display dimensions, e.g. `1920x1080`
# -Output       output resolution override (`VICTORY_OUTPUT`)
# -Log          writes process stdout/stderr under %LOCALAPPDATA%\victory\logs
# -SettingsXml  optional explicit path to a title XML settings file (overrides `xml` in the local file)
# -SettingsValues `<name value="..."/>` overrides applied atomically (with backup) before launch:
#               "name=value;name2=value2" (works from play.cmd too) or a PowerShell hashtable @{ name = 'value' }
# -Fps          optional frame-cap helper: applies the `[fps.<N>]` table of the local file
# -LocalConfig  the local file (default: local\play.toml next to victory.toml; never committed)
# -More         additional environment variables, as NAME=value pairs separated by ';' (works from play.cmd,
#               a command prompt and PowerShell):  -More "VICTORY_AMBIENT=0.8;VICTORY_PIPELINE_BOOK=0"
#               From PowerShell a hashtable works as well:  -More @{ VICTORY_AMBIENT = '0.8' }
# -ShowSettings prints the environment switches (and settings overrides) these options give, and starts nothing
#
# Every run records the pipelines the title creates (VICTORY_PIPELINE_BOOK=1: each is hashed and written
# once, to the cache folder), which is what `victory warm --used --settle` makes ahead of the next run.
# -More "VICTORY_PIPELINE_BOOK=0" switches the recording off.
#
# The local file holds what belongs to one title and one PC, so that none of it is in this script:
#   [settings]
#   xml = '%LOCALAPPDATA%\victory\data\<title id>\storage\<path>\settings.xml'   # the title's settings file
#   [settings.values]            # <name value="..."/> elements set before every launch
#   someElement = '1'
#   [fps.60]                     # what -Fps 60 sets (one table per frame rate you use)
#   someFrameCapElement = '60'
param(
    [string]$Exe,
    [switch]$Fast,
    [switch]$Safe,
    [switch]$RayTracing,
    [ValidateSet('', 'diag', 'release', 'release-dist', 'debug')][string]$Build = '',
    [ValidateSet('SeriesX', 'SeriesS')][string]$Console = 'SeriesX',
    [ValidatePattern('^\d{3,5}x\d{3,5}$')][string]$Display,
    [ValidatePattern('^\d{3,5}x\d{3,5}$')][string]$Output,
    [switch]$Log,
    [switch]$Check,
    [string]$SettingsXml,
    $SettingsValues = @{},
    [ValidateRange(1, 1000)][int]$Fps,
    [string]$LocalConfig,
    [switch]$SelfTest,
    $More = @{},
    [switch]$ShowSettings
)

# -More and -SettingsValues as a table: a hashtable as it is, or text of `NAME=value` pairs separated
# by ';' (all that `powershell -File`, and so play.cmd, can pass). A value may hold '=' and ','.
function ConvertTo-PairTable($Value, [string]$Option) {
    $table = @{}
    if ($null -eq $Value) { return $table }
    if ($Value -is [System.Collections.IDictionary]) {
        foreach ($k in $Value.Keys) { $table["$k"] = "$($Value[$k])" }
        return $table
    }
    foreach ($text in @($Value)) {
        $text = "$text".Trim()
        if ($text.Length -ge 2 -and $text[0] -eq $text[$text.Length - 1] -and "$($text[0])" -in "'", '"') { $text = $text.Substring(1, $text.Length - 2) }
        if ($text.StartsWith('@{')) {
            throw "${Option}: a hashtable (@{ ... }) only works when play.ps1 is called from PowerShell; write $Option `"NAME=value;NAME2=value2`" instead"
        }
        foreach ($pair in $text.Split(';')) {
            if (-not $pair.Trim()) { continue }
            $name, $value = $pair.Split([char[]]'=', 2)
            if ($null -eq $value -or -not $name.Trim()) { throw "${Option}: '$($pair.Trim())' is not NAME=value (pairs are separated by ';')" }
            $table[$name.Trim()] = $value.Trim()
        }
    }
    $table
}

# Replaces `<name value="..."/>` attributes in a user-supplied XML settings file after writing a timestamped backup.
function Set-TitleSettings([string]$Path, [System.Collections.IDictionary]$Want, [string]$BackupDir) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $text = $utf8.GetString($bytes)
    $changes = @()
    foreach ($name in $Want.Keys) {
        $re = [regex]('(<{0}\s+value\s*=\s*["''])([^"'']*)(["''])' -f [regex]::Escape($name))
        $found = $re.Matches($text)
        if ($found.Count -eq 0) { throw "settings XML has no <$name value=...> element: $Path" }
        if ($found.Count -gt 1) { throw "settings XML has $($found.Count) <$name value=...> elements: $Path" }
        if ($found[0].Groups[2].Value -ne $Want[$name]) {
            $changes += "$name $($found[0].Groups[2].Value) -> $($Want[$name])"
            $text = $re.Replace($text, ('${1}' + $Want[$name] + '${3}'), 1)
        }
    }
    if (-not $changes) { return @() }
    New-Item -ItemType Directory -Force -Path $BackupDir | Out-Null
    $backup = Join-Path $BackupDir ('settings-{0:yyyyMMdd-HHmmss}.xml' -f (Get-Date))
    [System.IO.File]::WriteAllBytes($backup, $bytes)
    [System.IO.File]::WriteAllBytes($Path, $utf8.GetBytes($text))
    $back = $utf8.GetString([System.IO.File]::ReadAllBytes($Path))
    foreach ($name in $Want.Keys) {
        $got = ([regex]('<{0}\s+value\s*=\s*["'']([^"'']*)["'']' -f [regex]::Escape($name))).Match($back).Groups[1].Value
        if ($got -ne $Want[$name]) { throw "settings XML: $name reads back as '$got', not $($Want[$name]) (backup: $backup)" }
    }
    $changes + "backup: $backup"
}

# Reads the local file: `[table]` headers and `key = 'value'` lines (a string in single or double
# quotes, or a bare number or word). Returns table name -> ordered key/value strings; empty if the
# file is not there.
function Read-LocalConfig([string]$Path) {
    $tables = @{}
    if (-not $Path -or -not (Test-Path -LiteralPath $Path)) { return $tables }
    $table = ''
    $n = 0
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        $n++
        $text = $line.Trim()
        if (-not $text -or $text.StartsWith('#')) { continue }
        $head = [regex]::Match($text, '^\[\s*([A-Za-z0-9_.\-]+)\s*\]\s*(#.*)?$')
        if ($head.Success) {
            $table = $head.Groups[1].Value
            if (-not $tables.ContainsKey($table)) { $tables[$table] = [ordered]@{} }
            continue
        }
        $pair = [regex]::Match($text, '^(?:"([^"]+)"|''([^'']+)''|([A-Za-z0-9_\-]+))\s*=\s*(.+)$')
        if (-not $pair.Success) { throw "${Path}: line $n is neither a [table] nor key = value: $text" }
        $key = $pair.Groups[1].Value + $pair.Groups[2].Value + $pair.Groups[3].Value
        $raw = $pair.Groups[4].Value.Trim()
        $single = [regex]::Match($raw, '^''([^'']*)''\s*(#.*)?$')
        $double = [regex]::Match($raw, '^"((?:[^"\\]|\\.)*)"\s*(#.*)?$')
        if ($single.Success) { $value = $single.Groups[1].Value }
        elseif ($double.Success) { $value = [regex]::Unescape($double.Groups[1].Value) }
        else { $value = ($raw -replace '\s+#.*$', '').Trim() }
        if (-not $tables.ContainsKey($table)) { $tables[$table] = [ordered]@{} }
        $tables[$table][$key] = "$value"
    }
    $tables
}

# What `-Fps <N>` sets: the `[fps.<N>]` table of the local file, or nothing if it has none.
function Get-FpsValues($Local, [int]$Fps) {
    $table = $Local["fps.$Fps"]
    if (-not $table -or $table.Count -eq 0) { return $null }
    $table
}

# Where victory.exe is and the folder its victory.toml belongs in: next to this script (a release
# folder), or the source tree's target\<build> (the build asked for, else the first one built).
function Find-Victory([string]$Here, [string]$Build) {
    $beside = Join-Path $Here 'victory.exe'
    if (Test-Path -LiteralPath $beside) {
        return @{ Victory = $beside; Root = $Here; Build = 'release'; Layout = 'release folder' }
    }
    $root = Split-Path -Parent $Here
    $builds = $(if ($Build) { @($Build) } else { 'diag', 'release', 'release-dist', 'debug' })
    foreach ($b in $builds) {
        $exe = Join-Path $root "target\$b\victory.exe"
        if (Test-Path -LiteralPath $exe) {
            # What `victory warm` calls the build: `diag` has the diagnostics feature, the rest do not.
            return @{ Victory = $exe; Root = $root; Build = $(if ($b -eq 'diag') { 'diag' } else { 'release' }); Layout = "source tree ($b)" }
        }
    }
    $null
}

function Get-FastChoices($Given) {
    $choices = [ordered]@{
        Console = 'SeriesS'; Display = '1280x720'; Output = '1280x720'; Build = 'release'
    }
    foreach ($k in @($choices.Keys)) { if ($Given.Keys -contains $k) { $choices[$k] = $Given[$k] } }
    $choices
}

function Get-RunSettings([switch]$Safe, [switch]$RayTracing, [string]$Display, [string]$Output,
    [switch]$Fast, [hashtable]$More = @{}) {
    $settings = @{
        VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
        VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
        VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'
        VICTORY_CARD_AWAKE = '1'; VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '1'; VICTORY_D3D12_DRED = 'light'
        # What `victory warm` makes ahead of a run is what earlier runs wrote down.
        VICTORY_PIPELINE_BOOK = '1'
    }
    if ($Safe) {
        $settings.VICTORY_UNDER_WIDE = '0'; $settings.VICTORY_LAND_ECHO = '0'
    } else {
        $settings.VICTORY_THIN = '1'; $settings.VICTORY_JOIN_PARTS = '8'; $settings.VICTORY_UPLOAD_AHEAD = '1'
    }
    if ($RayTracing) { $settings.VICTORY_BVH_BUILDER = '1' }
    if ($Display) { $settings.VICTORY_DISPLAY_MODE = $Display }
    if ($Output) { $settings.VICTORY_OUTPUT = $Output }
    if ($Fast) {
        $preset = [ordered]@{ VICTORY_CARD_AWAKE = '0'; VICTORY_MWAITX_WAIT = '1' }
        foreach ($k in $preset.Keys) { $settings[$k] = $preset[$k] }
    }
    foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }
    $settings
}

function Get-HostWarnings([double]$FreeCommitMB, $AutoPagefile, $DevMode, $Top) {
    $w = @()
    if ($FreeCommitMB -lt 12288) {
        $w += ('Free commit is {0:N1} GB (under 12 GB): the title can be refused memory; close other programs.' -f ($FreeCommitMB / 1024))
    }
    if ($AutoPagefile -eq $true) {
        $w += 'The page file is system managed: a fixed page file (Advanced system settings > Performance > Virtual memory) avoids memory refusals while it grows.'
    }
    if ($DevMode -ne $true) {
        $w += 'Windows Developer Mode is off: with it on, the layer can switch the driver''s background shader work off (otherwise those threads burn CPU).'
    }
    if ($Top) { $w += 'Largest other processes (private memory): ' + (($Top | ForEach-Object { '{0} {1:N0} MB' -f $_.Name, $_.MB }) -join ', ') }
    $w
}

function Get-TopProcesses([int]$Count = 5) {
    Get-Process | Where-Object { $_.Id -ne $PID } | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First $Count |
        ForEach-Object { [pscustomobject]@{ Name = $_.ProcessName; MB = [math]::Round($_.PrivateMemorySize64 / 1MB) } }
}

function Get-MachineWarnings {
    $free = 1e9; $auto = $false; $dev = $true; $top = @()
    try { $free = (Get-CimInstance Win32_OperatingSystem).FreeVirtualMemory / 1KB } catch { }
    try { $auto = (Get-CimInstance Win32_ComputerSystem).AutomaticManagedPagefile } catch { }
    try {
        $v = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' -ErrorAction Stop).AllowDevelopmentWithoutDevLicense
        $dev = ($v -eq 1)
    } catch { $dev = $false }
    try { $top = @(Get-TopProcesses) } catch { }
    Get-HostWarnings $free $auto $dev $top
}

function Read-WarmStamp([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $stamp = @{}
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        if ($line -match '^\s*#' -or $line -notmatch '=') { continue }
        $k, $v = $line.Split([char[]]'=', 2)
        $stamp[$k.Trim()] = $v.Trim()
    }
    $stamp
}

function Get-ExeStamp([string]$Path) {
    $file = Get-Item -LiteralPath $Path
    $head = New-Object byte[] ([math]::Min($file.Length, 1MB))
    $n = 0
    $stream = [System.IO.File]::OpenRead($file.FullName)
    try {
        while ($n -lt $head.Length) {
            $got = $stream.Read($head, $n, $head.Length - $n)
            if ($got -le 0) { break }
            $n += $got
        }
    } finally { $stream.Dispose() }
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { $hash = -join ($sha.ComputeHash($head, 0, $n) | ForEach-Object { $_.ToString('x2') }) } finally { $sha.Dispose() }
    $modified = [DateTimeOffset]::new($file.LastWriteTimeUtc).ToUnixTimeSeconds()
    @{ exe = $file.FullName; size = "$($file.Length)"; modified = "$modified"; head_sha256 = $hash }
}

# The warm-up command as it can be typed in a command prompt or PowerShell opened in the folder
# that holds victory.toml: victory.exe named relative to that folder, which is named after it.
function Get-WarmCommand([string]$Victory, [string]$Root) {
    $exe = $Victory
    if ($Root) {
        $base = $Root.TrimEnd('\') + '\'
        if ($Victory.StartsWith($base, [StringComparison]::OrdinalIgnoreCase)) { $exe = '.\' + $Victory.Substring($base.Length) }
    }
    if ($exe -match '\s') { $exe = "`"$exe`"" }
    "$exe warm --used --settle" + $(if ($Root) { "  (in the folder $Root)" })
}

# $Recorded: whether earlier runs wrote pipelines down (there is something for the warm-up to make).
function Compare-WarmStamp($Stamp, $Exe, [string]$Build, [string]$StampPath, [string]$Command = 'victory.exe warm --used --settle', $Recorded = $true) {
    $fix = "to make them ahead of a run, close the title and run:  $Command"
    if (-not $Stamp -and -not $Recorded) {
        return "No pipelines are recorded yet (a first run, or a new cache folder): this run compiles them as the title draws, with stutter, and records them. After it, $fix"
    }
    if (-not $Stamp) {
        return "No warm-up stamp ($StampPath): the pipelines were warmed with another build or not at all, and this run makes them again; $fix"
    }
    $differ = @()
    if (($Stamp.exe -replace '^\\\\\?\\', '') -ne $Exe.exe) { $differ += "the exe ($($Stamp.exe))" }
    if ($Stamp.build -ne $Build) { $differ += "the build ($($Stamp.build))" }
    foreach ($k in 'size', 'modified', 'head_sha256') { if ($Stamp[$k] -ne $Exe[$k]) { $differ += $k } }
    if ($differ) { "The pipelines were warmed with another build (the stamp differs in $($differ -join ', '); $StampPath); $fix" }
}

# `victory config` prints `name : value` lines; the value of one of them.
function Get-ConfigValue($Lines, [string]$Name) {
    foreach ($line in $Lines) {
        $m = [regex]::Match("$line", '^' + [regex]::Escape($Name) + '\s*:\s*(.*?)\s*$')
        if ($m.Success) { return ($m.Groups[1].Value -replace '\s+\(does not exist\)$', '') }
    }
    $null
}

function Get-WarmWarning([string]$Victory, [string]$Build, [string]$CacheDir, [string]$Root) {
    if (-not $CacheDir) { return $null }
    $path = Join-Path $CacheDir 'pipelines\warm-stamp.txt'
    $recorded = Test-Path -LiteralPath (Join-Path $CacheDir 'pipelines\known-pipelines.bin')
    try { Compare-WarmStamp (Read-WarmStamp $path) (Get-ExeStamp $Victory) $Build $path (Get-WarmCommand $Victory $Root) $recorded }
    catch { "The warm-up stamp ($path) could not be compared: $($_.Exception.Message)" }
}

if ($SelfTest) {
    $dir = Join-Path ([System.IO.Path]::GetTempPath()) ('launcher-selftest-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    try {
        $file = Join-Path $dir 'config.xml'
        $orig = "<?xml version=`"1.0`" encoding=`"UTF-8`"?>`r`n<settings>`r`n  <frameCap value=`"30`" />`r`n  <other value=`"30`"/>`n  <frameLimit value=`"30`" />`n  <syncInterval value=`"2`" />`n</settings>`r`n"
        $utf8 = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllBytes($file, $utf8.GetBytes($orig))
        $fail = 0
        function Check($ok, $what) { if ($ok) { Write-Host "ok   $what" } else { Write-Host "FAIL $what"; $script:fail++ } }

        # The element names of a title's settings come from the local file, never from this script.
        $localFile = Join-Path $dir 'play.toml'
        $localText = @(
            '# a local file',
            '[settings]',
            "xml = '$file'   # the settings file",
            '',
            '[settings.values]',
            'other = "3\\1"',
            '',
            '[fps.30]',
            "frameCap = '30'",
            'frameLimit = "30"',
            'syncInterval = 2',
            '[fps.60]',
            'frameCap = 60   # bare',
            "frameLimit = '60'",
            "'syncInterval' = '1'"
        ) -join "`n"
        [System.IO.File]::WriteAllText($localFile, $localText)
        $local = Read-LocalConfig $localFile
        Check ($local['settings'].xml -eq $file -and $local['settings.values'].other -eq '3\1') 'local file: strings, comments and escapes are read'
        Check ((@($local['fps.60'].Keys) -join ',') -eq 'frameCap,frameLimit,syncInterval' -and $local['fps.60'].frameCap -eq '60') 'local file: a table keeps its keys in order'
        Check ($null -eq (Get-FpsValues $local 45) -and (Read-LocalConfig (Join-Path $dir 'none.toml')).Count -eq 0) 'local file: no table for a rate, and no file, give nothing'
        $badFile = Join-Path $dir 'bad.toml'
        [System.IO.File]::WriteAllText($badFile, "[fps.60]`nnot a pair`n")
        $badLine = $false
        try { Read-LocalConfig $badFile | Out-Null } catch { $badLine = $_.Exception.Message -match 'line 2' }
        Check $badLine 'local file: a line that is not key = value is named'

        $r = Set-TitleSettings $file (Get-FpsValues $local 60) (Join-Path $dir 'backups')
        $now = $utf8.GetString([System.IO.File]::ReadAllBytes($file))
        Check ($r.Count -eq 4) "60: three changes and a backup line ($($r -join '; '))"
        Check ($now -eq $orig.Replace('frameCap value="30"', 'frameCap value="60"').Replace('frameLimit value="30"', 'frameLimit value="60"').Replace('syncInterval value="2"', 'syncInterval value="1"')) '60: only the three values differ'
        Check ((Get-ChildItem (Join-Path $dir 'backups')).Count -eq 1) '60: one backup'
        $r = Set-TitleSettings $file (Get-FpsValues $local 30) (Join-Path $dir 'backups')
        Check ($r.Count -eq 4 -and $utf8.GetString([System.IO.File]::ReadAllBytes($file)) -eq $orig) '30: restored byte for byte'

        # Both layouts: victory.exe next to the script, and the source tree's target folder.
        $rel = Join-Path $dir 'release-folder'
        $src = Join-Path $dir 'tree'
        New-Item -ItemType Directory -Force -Path $rel, (Join-Path $src 'scripts'), (Join-Path $src 'target\release'), (Join-Path $src 'target\diag') | Out-Null
        [System.IO.File]::WriteAllText((Join-Path $rel 'victory.exe'), 'x')
        $found = Find-Victory $rel ''
        Check ($found.Root -eq $rel -and $found.Victory -eq (Join-Path $rel 'victory.exe') -and $found.Build -eq 'release') 'layout: a release folder has victory.exe next to the script'
        Check ($null -eq (Find-Victory (Join-Path $src 'scripts') '')) 'layout: a source tree with nothing built finds nothing'
        [System.IO.File]::WriteAllText((Join-Path $src 'target\release\victory.exe'), 'x')
        $found = Find-Victory (Join-Path $src 'scripts') ''
        Check ($found.Root -eq $src -and $found.Victory -eq (Join-Path $src 'target\release\victory.exe') -and $found.Build -eq 'release') 'layout: the source tree takes the build there is'
        [System.IO.File]::WriteAllText((Join-Path $src 'target\diag\victory.exe'), 'x')
        Check ((Find-Victory (Join-Path $src 'scripts') '').Build -eq 'diag' -and (Find-Victory (Join-Path $src 'scripts') 'release').Build -eq 'release') 'layout: diag first, or the build asked for'
        Check ($null -eq (Find-Victory (Join-Path $src 'scripts') 'debug')) 'layout: a build asked for and not built is not replaced'
        Check ((Get-ConfigValue @('config file : (none)', 'cache_dir   : C:\some where\cache', 'game_dir    : D:\g  (does not exist)') 'cache_dir') -eq 'C:\some where\cache' -and (Get-ConfigValue @('game_dir    : D:\g  (does not exist)') 'game_dir') -eq 'D:\g') 'config values are read from `victory config`'

        # Generic user-supplied XML override check (arbitrary schema, duplicate-tag safety).
        $genericXml = @(
            '<?xml version="1.0" encoding="UTF-8"?>',
            '<settings>',
            '  <graphics>',
            '    <renderQuality value="2" />',
            '    <renderQualityBias value="0.0" />',
            '  </graphics>',
            '  <video>',
            '    <passA><width value="1280" /></passA>',
            '    <passB><width value="1920" /></passB>',
            '  </video>',
            '</settings>'
        ) -join "`r`n"
        [System.IO.File]::WriteAllBytes($file, $utf8.GetBytes($genericXml))
        $r = Set-TitleSettings $file ([ordered]@{ renderQuality = '1' }) (Join-Path $dir 'generic-backups')
        $now = $utf8.GetString([System.IO.File]::ReadAllBytes($file))
        Check ($r.Count -eq 2 -and $now.Contains('<renderQuality value="1" />') -and $now.Contains('<renderQualityBias value="0.0" />')) 'generic override preserves sibling keys'
        $dupBlocked = $false
        try { Set-TitleSettings $file ([ordered]@{ width = '1280' }) (Join-Path $dir 'generic-backups') | Out-Null } catch { $dupBlocked = $_.Exception.Message -match '2 <width' }
        Check $dupBlocked 'duplicate element name raises explicit error'

        $w = @(Get-HostWarnings 8192 $true $false @([pscustomobject]@{ Name = 'a'; MB = 900 }, [pscustomobject]@{ Name = 'b'; MB = 100 }))
        Check ($w.Count -eq 4 -and $w[0] -match '8\.0 GB' -and $w[3] -match 'a 900 MB, b 100 MB') 'host warnings formatting'

        $s = Get-RunSettings -Fast -Display '1280x720' -Output '1280x720'
        Check ($s.VICTORY_CARD_AWAKE -eq '0' -and $s.VICTORY_MWAITX_WAIT -eq '1' -and $s.VICTORY_OUTPUT -eq '1280x720') 'fast run settings'
        Check ($s.VICTORY_PIPELINE_BOOK -eq '1' -and (Get-RunSettings -Safe).VICTORY_PIPELINE_BOOK -eq '1') 'runs record the pipelines they create'

        # -More and -SettingsValues: text (all play.cmd can pass) and a hashtable give the same table.
        $m = ConvertTo-PairTable 'VICTORY_AMBIENT=0.8; VICTORY_CRASH_SAVE=10:20,30:40 ;;VICTORY_PIPELINE_BOOK=0' '-More'
        Check ($m.Count -eq 3 -and $m.VICTORY_AMBIENT -eq '0.8' -and $m.VICTORY_CRASH_SAVE -eq '10:20,30:40' -and $m.VICTORY_PIPELINE_BOOK -eq '0') '-More: NAME=value pairs separated by ;'
        $h = ConvertTo-PairTable @{ VICTORY_AMBIENT = '0.8'; VICTORY_PIPELINE_BOOK = 0 } '-More'
        Check ($h.Count -eq 2 -and $h.VICTORY_AMBIENT -eq '0.8' -and $h.VICTORY_PIPELINE_BOOK -eq '0') '-More: a hashtable is taken as it is'
        Check ((ConvertTo-PairTable "'a=b=c'" '-More').a -eq 'b=c' -and (ConvertTo-PairTable @('a=1', 'b=2;c=3') '-More').Count -eq 3 -and (ConvertTo-PairTable '' '-More').Count -eq 0) '-More: quotes, = in a value, several texts, none'
        $badPair = $false; $badTable = $false
        try { ConvertTo-PairTable 'VICTORY_AMBIENT' '-More' | Out-Null } catch { $badPair = $_.Exception.Message -match 'not NAME=value' }
        try { ConvertTo-PairTable '@{' '-More' | Out-Null } catch { $badTable = $_.Exception.Message -match 'NAME=value;NAME2=value2' }
        Check ($badPair -and $badTable) '-More: text that is not pairs, and a hashtable through play.cmd, are explained'
        $s = Get-RunSettings -More (ConvertTo-PairTable 'VICTORY_PIPELINE_BOOK=0;VICTORY_AMBIENT=0.8' '-More')
        Check ($s.VICTORY_PIPELINE_BOOK -eq '0' -and $s.VICTORY_AMBIENT -eq '0.8' -and $s.VICTORY_RELAY -eq '2') '-More overrides what the launcher sets'
        # The whole way in, as play.cmd starts this script (powershell -File gives text, never a hashtable).
        $viaFile = @(& powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath -ShowSettings -More 'VICTORY_AMBIENT=0.8;VICTORY_PIPELINE_BOOK=0' -SettingsValues 'someElement=1')
        Check ($LASTEXITCODE -eq 0 -and ($viaFile -contains 'VICTORY_AMBIENT=0.8') -and ($viaFile -contains 'VICTORY_PIPELINE_BOOK=0') -and ($viaFile -contains 'setting someElement=1')) '-More and -SettingsValues arrive through powershell -File'
        $cmdFile = Join-Path $PSScriptRoot 'play.cmd'
        if (Test-Path -LiteralPath $cmdFile) {
            $viaCmd = @(& cmd.exe /d /c "`"$cmdFile`" -ShowSettings -More `"VICTORY_AMBIENT=0.8;VICTORY_PIPELINE_BOOK=0`" -SettingsValues someElement=1")
            Check ($LASTEXITCODE -eq 0 -and ($viaCmd -contains 'VICTORY_AMBIENT=0.8') -and ($viaCmd -contains 'VICTORY_PIPELINE_BOOK=0') -and ($viaCmd -contains 'setting someElement=1')) '-More and -SettingsValues arrive through play.cmd'
        }

        $exe = Join-Path $dir 'victory.exe'
        $pattern = [byte[]](0..250)
        $bytes = New-Object byte[] (1MB + 1000)
        for ($o = 0; $o -lt $bytes.Length; $o += 251) { [Array]::Copy($pattern, 0, $bytes, $o, [math]::Min(251, $bytes.Length - $o)) }
        [System.IO.File]::WriteAllBytes($exe, $bytes)
        $e = Get-ExeStamp $exe
        $stampFile = Join-Path $dir 'warm-stamp.txt'
        $text = "# victory warm`nexe=\\?\$($exe.ToUpper())`nbuild=release`nsize=$($e.size)`nmodified=$($e.modified)`nhead_sha256=$($e.head_sha256)`nsettled=yes`nwarmed=1`n"
        [System.IO.File]::WriteAllText($stampFile, $text)
        $st = Read-WarmStamp $stampFile
        Check ($null -eq (Compare-WarmStamp $st $e 'release' $stampFile)) 'warm stamp matches exact exe/build'
        Check ((Compare-WarmStamp $st $e 'diag' $stampFile) -match 'warmed with another build') 'warm stamp flags build mismatch'
        $cmd = Get-WarmCommand (Join-Path $src 'target\release\victory.exe') $src
        Check ($cmd -eq ".\target\release\victory.exe warm --used --settle  (in the folder $src)") 'warm command is relative to the config folder'
        Check ((Compare-WarmStamp $null $e 'release' $stampFile $cmd $true) -match '^No warm-up stamp.*run:  \.\\target\\release\\victory\.exe warm --used --settle  \(in the folder ') 'no warm stamp: the command to run is said'
        Check ((Compare-WarmStamp $null $e 'release' $stampFile $cmd $false) -match '^No pipelines are recorded yet.*After it,.*victory\.exe warm --used --settle') 'first run: said to be one, with the command for afterwards'

        if ($fail) { Write-Host "$fail check(s) failed"; exit 1 }
        Write-Host 'self-test passed'
    } finally { Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
    return
}

try {
    $More = ConvertTo-PairTable $More '-More'
    $SettingsValues = ConvertTo-PairTable $SettingsValues '-SettingsValues'
} catch { Write-Host $_.Exception.Message; exit 1 }
if ($ShowSettings) {
    if ($Fast) { $f = Get-FastChoices $PSBoundParameters; $Display = $f.Display; $Output = $f.Output }
    $shown = Get-RunSettings -Safe:$Safe -RayTracing:$RayTracing -Fast:$Fast -Display $Display -Output $Output -More $More
    $shown.Keys | Sort-Object | ForEach-Object { Write-Host "$_=$($shown[$_])" }
    $SettingsValues.Keys | Sort-Object | ForEach-Object { Write-Host "setting $_=$($SettingsValues[$_])" }
    exit 0
}

$found = Find-Victory $PSScriptRoot $(if ($Fast -and -not $Build) { (Get-FastChoices $PSBoundParameters).Build } else { $Build })
if (-not $found) {
    Write-Host "victory.exe was not found next to this script or under ..\target."
    Write-Host 'From a release: keep play.ps1 in the folder it was unzipped to, next to victory.exe.'
    Write-Host 'From the source tree: build it first with  scripts\build.ps1 -Release  (or -Diag; README.md).'
    exit 1
}
$victory = (Resolve-Path -LiteralPath $found.Victory).Path
$root = (Resolve-Path -LiteralPath $found.Root).Path
if ($Fast) {
    $f = Get-FastChoices $PSBoundParameters
    $Console = $f.Console; $Display = $f.Display; $Output = $f.Output
}
$Build = $found.Build
$settings = Get-RunSettings -Safe:$Safe -RayTracing:$RayTracing -Fast:$Fast -Display $Display -Output $Output -More $More

if ($Exe) {
    if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
        Write-Host "The title executable $Exe does not exist. Check the path, or leave -Exe out to use the only .exe in the game folder."
        exit 1
    }
    $Exe = (Resolve-Path -LiteralPath $Exe).Path
}
$exeArgs = @($(if ($Exe) { $Exe }))
# Paths given relative to where this was started from stay so after the change of directory below.
if ($LocalConfig) { $LocalConfig = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LocalConfig) }
if ($SettingsXml) { $SettingsXml = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($SettingsXml) }

$keep = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_PATCHES_FILE', 'VICTORY_SHADER_PROFILE', 'VICTORY_BCPACK_TABLES', 'VICTORY_SKIP_PREFLIGHT', 'VICTORY_SKIP_CPU_CHECK'
Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $keep } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }

# victory.toml is looked for in the current directory first: the folder it belongs in.
Push-Location $root
try {
    if ($Check) {
        & $victory check @exeArgs
        exit $LASTEXITCODE
    }
    # Everything a run needs, before anything is changed or started.
    $report = @(& $victory check @exeArgs 2>&1 | ForEach-Object { "$_" })
    if ($LASTEXITCODE -ne 0) {
        $report | ForEach-Object { Write-Host $_ }
        Write-Host 'Not starting: fix what is MISSING above (QUICKSTART.txt / README.md), then run this again.'
        exit 1
    }
    for ($i = 0; $i -lt $report.Count; $i++) {
        if ($report[$i] -match '^\[ WARN') { Write-Host $report[$i]; if ($report[$i + 1] -match '^\s+fix:') { Write-Host $report[$i + 1] } }
    }
    $config = @(& $victory config 2>&1 | ForEach-Object { "$_" })

    $localPath = $(if ($LocalConfig) { $LocalConfig } else { Join-Path $root 'local\play.toml' })
    if ($LocalConfig -and -not (Test-Path -LiteralPath $LocalConfig)) { Write-Host "No local file at $LocalConfig."; exit 1 }
    $local = Read-LocalConfig $localPath
    $want = [ordered]@{}
    if ($local['settings.values']) { foreach ($k in $local['settings.values'].Keys) { $want[$k] = $local['settings.values'][$k] } }
    foreach ($k in $SettingsValues.Keys) { $want[$k] = "$($SettingsValues[$k])" }
    if ($Fps) {
        $v = Get-FpsValues $local $Fps
        if (-not $v) {
            Write-Host "-Fps ${Fps}: $localPath has no [fps.$Fps] table naming the settings elements to set (see the top of play.ps1)."
            exit 1
        }
        foreach ($k in $v.Keys) { $want[$k] = $v[$k] }
    }
    if ($want.Count -gt 0) {
        $targetXml = $(if ($SettingsXml) { $SettingsXml } elseif ($local['settings'] -and $local['settings'].xml) { [Environment]::ExpandEnvironmentVariables($local['settings'].xml) } else { $null })
        if (-not $targetXml) {
            Write-Host "Skipping XML overrides: pass -SettingsXml <path>, or set xml under [settings] in $localPath."
        } elseif (-not (Test-Path -LiteralPath $targetXml)) {
            Write-Host "No $targetXml found; XML overrides skipped."
        } else {
            $did = Set-TitleSettings $targetXml $want (Join-Path $env:LOCALAPPDATA 'victory\backups')
            if ($did) { $did | ForEach-Object { Write-Host "settings : $_" } } else { Write-Host 'settings : already set' }
        }
    }

    $warnings = @(Get-MachineWarnings) + @(Get-WarmWarning $victory $Build (Get-ConfigValue $config 'cache_dir') $root | Where-Object { $_ })
    $warnings | ForEach-Object { Write-Host "WARNING: $_" }
    $run = @('run') + $exeArgs + @('--execute', '--console', @{ SeriesX = 'series-x'; SeriesS = 'series-s' }[$Console])
    if (-not $Log) { & $victory @run; exit $LASTEXITCODE }
    $logs = Join-Path $env:LOCALAPPDATA 'victory\logs'
    New-Item -ItemType Directory -Force -Path $logs | Out-Null
    $file = Join-Path $logs ('{0:yyyyMMdd-HHmmss}-{1}{2}{3}.log' -f (Get-Date), $Console, $(if ($Display) { "-$Display" }), $(if ($Fast) { '-fast' }))
    $how = $(if ($Fast) { '-Fast ' }) + "-Console $Console -Build $Build" + $(if ($Display) { " -Display $Display" }) + $(if ($Output) { " -Output $Output" })
    $how += $(if ($Safe) { ' -Safe' }) + $(if ($RayTracing) { ' -RayTracing' }) + $(if ($Fps) { " -Fps $Fps" })
    $head = @("# play.ps1 $how", "# victory $($run -join ' ')") + ($warnings | ForEach-Object { "# warning: $_" }) + ($settings.Keys | Sort-Object | ForEach-Object { "# $_=$($settings[$_])" })
    [System.IO.File]::WriteAllLines($file, [string[]]$head)
    Write-Host "Logging to $file"
    $line = 'set NO_COLOR=1&& "{0}" {1} >> "{2}" 2>&1' -f $victory, (($run | ForEach-Object { '"{0}"' -f $_ }) -join ' '), $file
    cmd.exe /d /s /c "`"$line`""
    $code = $LASTEXITCODE
    Write-Host "Exit code $code. Log: $file"
    if ($code -ne 0) { Get-Content -LiteralPath $file -Tail 20 }
    exit $code
} finally { Pop-Location }
