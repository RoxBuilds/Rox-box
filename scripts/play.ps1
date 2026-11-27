# Starts a title through Rox-box with optional runtime tuning and optional XML settings overrides.
#   play.cmd                       (or: play.ps1) runs the only .exe in the configured game folder
#   play.ps1 -Exe <path to title executable>
#   play.cmd <folder or .exe>      a folder is the game folder for this run, an .exe is -Exe (drop either on play.cmd)
# Works from a release folder (victory.exe next to this script) and from the source tree
# (..\target\<build>\victory.exe), from any current directory.
# First run: with no victory.toml where it is looked for (next to victory.exe, or at the repository root in the
# source tree) and neither VICTORY_CONFIG nor VICTORY_GAME_DIR set, a person at a normal window is asked for the
# folder with their title files (a folder dialog, or a typed path), victory.toml is written with only game_dir set,
# `victory check` runs, and "Start now?" is asked. Without a person (CI, redirected input, -SelfTest, -Check) nothing
# is asked and the old message from `victory check` stands. An existing victory.toml is never overwritten.
# -Setup        asks for the folder again: the game_dir line of victory.toml is changed (a timestamped copy is kept
#               next to it, every other line stays); no game_dir line under [paths] gets one
# -Path         the first argument: a folder (game folder for this run; offered to be remembered when there is no
#               victory.toml) or a .exe (same as -Exe)
# -Exe          the title executable; optional when the game folder (game_dir in victory.toml) holds exactly one .exe
# -Check        runs `victory check` (every prerequisite as OK / WARN / MISSING with the fix) and starts nothing
# -Fast         low-memory / fast preset: -Console SeriesS, -Display 1920x1080, -Build release (each unless
#               specified), plus VICTORY_CARD_AWAKE=0 and VICTORY_MWAITX_WAIT=1. It sets no -Output and no display
#               under 1920x1080: a title can ship its upscaler for one output size per console model, and stops
#               when it is given a smaller one. -Display / -Output lower them for a title that takes it.
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
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Position = 0)][string]$Path,
    [string]$Exe,
    [switch]$Setup,
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

# What -Fast chooses where the command line does not. No output size, and no display smaller than the
# smaller console model's own 1920x1080: a title that ships its upscaler for that one size stops (its
# upscaler refuses the other size, and the title then divides by the size it never got) when the picture
# it is asked for is smaller. The load is lowered by the console model, not by the picture's size.
function Get-FastChoices($Given) {
    $choices = [ordered]@{
        Console = 'SeriesS'; Display = '1920x1080'; Output = ''; Build = 'release'
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

# ---- First-run setup: the game folder, written to victory.toml -------------------------------------

# True when a person can see and answer prompts: not CI, not redirected input or output, not
# `powershell -NonInteractive`. Anything else keeps the old behaviour (no dialog, no question).
function Test-InteractiveSession {
    if ($env:CI -or $env:GITHUB_ACTIONS -or $env:TF_BUILD) { return $false }
    if ([Environment]::GetCommandLineArgs() -icontains '-NonInteractive') { return $false }
    try {
        if (-not [Environment]::UserInteractive) { return $false }
        if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) { return $false }
    } catch { return $false }
    $Host.Name -eq 'ConsoleHost'
}

# What a path given on the command line (or dropped on play.cmd) is: a folder, a .exe, some other
# file, or nothing that exists. Stray quotes are dropped: a folder dropped from a drive's root arrives
# as  E:\"  because of how a command line is split.
function Get-LaunchTarget([string]$Path) {
    $p = "$Path".Trim().Trim('"').Trim()
    if (-not $p) { return @{ Kind = 'None'; Path = '' } }
    if (Test-Path -LiteralPath $p -PathType Container) {
        $full = (Resolve-Path -LiteralPath $p).Path
        if ($full.Length -gt 3) { $full = $full.TrimEnd('\') }
        return @{ Kind = 'Folder'; Path = $full }
    }
    if (Test-Path -LiteralPath $p -PathType Leaf) {
        $full = (Resolve-Path -LiteralPath $p).Path
        return @{ Kind = $(if ([System.IO.Path]::GetExtension($full) -ieq '.exe') { 'Exe' } else { 'NotExe' }); Path = $full }
    }
    @{ Kind = 'Missing'; Path = $p }
}

# Null when the folder can be a game folder (it exists and holds a .exe), else what is wrong with it.
function Test-GameFolder([string]$Path) {
    $p = "$Path".Trim().Trim('"').Trim()
    if (-not $p) { return 'No folder was given.' }
    if (-not (Test-Path -LiteralPath $p -PathType Container)) { return "The folder $p does not exist." }
    try {
        $exes = @(Get-ChildItem -LiteralPath $p -Filter '*.exe' -File -ErrorAction Stop | Where-Object { $_.Extension -ieq '.exe' })
    } catch { return "The folder $p cannot be read: $($_.Exception.Message)" }
    if ($exes.Count -eq 0) { return "The folder $p holds no .exe file. Pick the folder that holds the title's executable and everything shipped beside it." }
    $null
}

# A TOML string for a path. A literal string ('...': no escapes, what the docs recommend) cannot hold a
# single quote, so a path with one (or with a control character) becomes a basic string ("...") with
# its backslashes doubled.
function ConvertTo-TomlString([string]$Value) {
    if ($Value -notmatch "['\x00-\x1f\x7f]") { return "'" + $Value + "'" }
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    foreach ($ch in $Value.ToCharArray()) {
        $c = [int]$ch
        if ($c -eq 92) { [void]$sb.Append('\\') }
        elseif ($c -eq 34) { [void]$sb.Append('\"') }
        elseif ($c -lt 32 -or $c -eq 127) { [void]$sb.Append(('\u{0:X4}' -f $c)) }
        else { [void]$sb.Append($ch) }
    }
    [void]$sb.Append('"')
    $sb.ToString()
}

# The text of a TOML string as written by ConvertTo-TomlString (or by hand in either style); null if
# it is neither.
function ConvertFrom-TomlString([string]$Raw) {
    $single = [regex]::Match($Raw, '^''([^'']*)''')
    if ($single.Success) { return $single.Groups[1].Value }
    $double = [regex]::Match($Raw, '^"((?:[^"\\]|\\.)*)"')
    if ($double.Success) { return [regex]::Unescape($double.Groups[1].Value) }
    $null
}

# Splits text into lines that keep their own line ending.
function Split-KeepingEol([string]$Text) {
    @($Text -split '(?<=\n)' | Where-Object { $_ -ne '' })
}

# Where game_dir is in a victory.toml: the [paths] header and the first uncommented game_dir line
# under it (either may be -1).
function Find-GameDirLine($Lines) {
    $header = -1; $at = -1; $inPaths = $false
    for ($i = 0; $i -lt $Lines.Count; $i++) {
        $body = $Lines[$i].TrimEnd("`r", "`n")
        $h = [regex]::Match($body, '^\s*(\[\[?)\s*([^\]]*?)\s*\]\]?\s*(#.*)?$')
        if ($h.Success) {
            $inPaths = ($h.Groups[1].Value -eq '[' -and $h.Groups[2].Value -eq 'paths')
            if ($inPaths -and $header -lt 0) { $header = $i }
            continue
        }
        if ($inPaths -and $at -lt 0 -and $body -match '^\s*game_dir\s*=') { $at = $i }
    }
    @{ Header = $header; Line = $at }
}

# The game_dir a victory.toml text names (null if it names none).
function Get-GameDirValue([string]$Text) {
    $lines = Split-KeepingEol $Text
    $where = Find-GameDirLine $lines
    if ($where.Line -lt 0) { return $null }
    $m = [regex]::Match($lines[$where.Line].TrimEnd("`r", "`n"), '^\s*game_dir\s*=\s*(.*)$')
    ConvertFrom-TomlString $m.Groups[1].Value
}

# The text with game_dir set under [paths] and nothing else changed: every other line, comment and line
# ending stays. The value of an existing line is replaced (a comment after it stays); with no such line
# one is added right under [paths], or [paths] is added at the end.
function Set-GameDirLine([string]$Text, [string]$GameDir) {
    $new = 'game_dir = ' + (ConvertTo-TomlString $GameDir)
    $eol = $(if ($Text.Contains("`r`n") -or -not $Text.Contains("`n")) { "`r`n" } else { "`n" })
    $lines = [System.Collections.ArrayList]@(Split-KeepingEol $Text)
    $where = Find-GameDirLine $lines
    if ($where.Line -ge 0) {
        $line = $lines[$where.Line]
        $body = $line.TrimEnd("`r", "`n")
        $m = [regex]::Match($body, '^(\s*game_dir\s*=\s*)(?:''[^'']*''|"(?:[^"\\]|\\.)*")(\s*#.*)?$')
        $comment = $(if ($m.Success) { $m.Groups[2].Value } else { '' })
        $indent = [regex]::Match($body, '^\s*').Value
        $lines[$where.Line] = $indent + $new + $comment + $line.Substring($body.Length)
    } elseif ($where.Header -ge 0) {
        $head = $lines[$where.Header]
        if (-not $head.EndsWith("`n")) { $lines[$where.Header] = $head + $eol }
        $lines.Insert($where.Header + 1, $new + $eol)
    } else {
        if ($lines.Count -gt 0 -and -not $lines[$lines.Count - 1].EndsWith("`n")) { $lines[$lines.Count - 1] += $eol }
        if ($lines.Count -gt 0) { [void]$lines.Add($eol) }
        [void]$lines.Add('[paths]' + $eol)
        [void]$lines.Add($new + $eol)
    }
    -join $lines
}

# Writes a victory.toml that does not exist yet from the example file (or, without it, a short one)
# with only game_dir set. Never replaces a file: it fails if one is there.
function New-ConfigFile([string]$ConfigPath, [string]$GameDir, [string]$TemplatePath) {
    $template = $null
    if ($TemplatePath -and (Test-Path -LiteralPath $TemplatePath -PathType Leaf)) {
        $template = (New-Object System.Text.UTF8Encoding($false)).GetString([System.IO.File]::ReadAllBytes($TemplatePath)).TrimStart([char]0xFEFF)
    }
    if (-not $template) {
        $template = "# Rox-box configuration. Created by play.cmd; victory.example.toml lists every other entry.`r`n`r`n[paths]`r`ngame_dir = 'C:\Games\MyTitle'`r`n"
    }
    $text = Set-GameDirLine $template $GameDir
    $bytes = (New-Object System.Text.UTF8Encoding($false)).GetBytes($text)
    $stream = [System.IO.File]::Open($ConfigPath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write)
    try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
}

# Changes only the game_dir line of an existing victory.toml, after a timestamped copy next to it.
# Returns the copy's path.
function Update-ConfigGameDir([string]$ConfigPath, [string]$GameDir) {
    $bytes = [System.IO.File]::ReadAllBytes($ConfigPath)
    $bom = ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)
    $backup = '{0}.bak-{1:yyyyMMdd-HHmmss}' -f $ConfigPath, (Get-Date)
    for ($n = 2; (Test-Path -LiteralPath $backup); $n++) { $backup = '{0}.bak-{1:yyyyMMdd-HHmmss}-{2}' -f $ConfigPath, (Get-Date), $n }
    [System.IO.File]::WriteAllBytes($backup, $bytes)
    $utf8 = New-Object System.Text.UTF8Encoding($bom)
    $text = $utf8.GetString($bytes).TrimStart([char]0xFEFF)
    $out = $utf8.GetPreamble() + $utf8.GetBytes((Set-GameDirLine $text $GameDir))
    [System.IO.File]::WriteAllBytes($ConfigPath, $out)
    $backup
}

# Remembers the game folder: a new victory.toml, or the game_dir line of the one there is. Reads the
# file back and fails if it does not name the folder. Returns @{ Created; Backup }.
function Save-GameDir([string]$ConfigPath, [string]$GameDir, [string]$TemplatePath) {
    $backup = $null
    $exists = Test-Path -LiteralPath $ConfigPath -PathType Leaf
    if ($exists) { $backup = Update-ConfigGameDir $ConfigPath $GameDir } else { New-ConfigFile $ConfigPath $GameDir $TemplatePath }
    $back = Get-GameDirValue ((New-Object System.Text.UTF8Encoding($false)).GetString([System.IO.File]::ReadAllBytes($ConfigPath)).TrimStart([char]0xFEFF))
    if ($back -ne $GameDir) { throw "$ConfigPath reads back game_dir as '$back', not '$GameDir'$(if ($backup) { " (the earlier file is kept as $backup)" })" }
    @{ Created = (-not $exists); Backup = $backup }
}

# The victory.toml victory.exe will load, or null: the file VICTORY_CONFIG names, else victory.toml in
# the launcher's folder, else the one next to victory.exe (the order `victory` itself uses).
function Find-ConfigFile([string]$ConfigEnv, [string]$Root, [string]$VictoryDir) {
    if ($ConfigEnv) { return $ConfigEnv }
    foreach ($dir in $Root, $VictoryDir) {
        if (-not $dir) { continue }
        $p = Join-Path $dir 'victory.toml'
        if (Test-Path -LiteralPath $p -PathType Leaf) { return $p }
    }
    $null
}

# First-run setup is for a PC where nothing says which folder to use: no victory.toml where it is
# looked for, and neither VICTORY_CONFIG nor VICTORY_GAME_DIR set.
function Test-NeedsFirstRun([string]$ConfigEnv, [string]$GameDirEnv, [string]$Root, [string]$VictoryDir) {
    if ($GameDirEnv) { return $false }
    -not (Find-ConfigFile $ConfigEnv $Root $VictoryDir)
}

function ConvertFrom-YesNo([string]$Answer, [bool]$Default) {
    $a = "$Answer".Trim()
    if (-not $a) { return $Default }
    $a -match '^(y|yes|j|ja|o|oui|s|si)$'
}

function Read-YesNo([string]$Question, [bool]$Default = $true) {
    ConvertFrom-YesNo (Read-Host ($Question + $(if ($Default) { ' [Y/n]' } else { ' [y/N]' }))) $Default
}

# One try at getting a folder from the person: Windows' folder dialog, or (if it cannot be shown) a
# typed path. Null means they gave up (cancelled, or typed nothing).
$script:DialogBroken = $false
function Get-FolderChoice([string]$Description) {
    if (-not $script:DialogBroken) {
        $owner = $null; $dialog = $null
        try {
            Add-Type -AssemblyName System.Windows.Forms
            $owner = New-Object System.Windows.Forms.Form
            $owner.TopMost = $true
            $dialog = New-Object System.Windows.Forms.FolderBrowserDialog
            $dialog.Description = $Description
            $dialog.ShowNewFolderButton = $false
            if ($dialog.ShowDialog($owner) -eq [System.Windows.Forms.DialogResult]::OK) { return $dialog.SelectedPath }
            return $null
        } catch {
            $script:DialogBroken = $true
            Write-Host "The folder window could not be shown ($($_.Exception.Message))."
        } finally {
            if ($dialog) { $dialog.Dispose() }
            if ($owner) { $owner.Dispose() }
        }
    }
    $typed = Read-Host 'Type or paste the folder path (leave empty to cancel)'
    $typed = "$typed".Trim().Trim('"').Trim()
    if ($typed) { $typed } else { $null }
}

# Asks until the folder is one that can be used; null when the person cancels. $Pick gives one choice.
function Select-GameFolder([scriptblock]$Pick = { Get-FolderChoice 'Pick the folder that holds your own title files (the title''s .exe and everything beside it)' }) {
    while ($true) {
        $choice = & $Pick
        if (-not $choice) { return $null }
        $problem = Test-GameFolder $choice
        if (-not $problem) { return (Get-LaunchTarget $choice).Path }
        Write-Host "That folder will not do. $problem"
        Write-Host 'Please pick another one.'
    }
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
        $f = Get-FastChoices @{}
        $s = Get-RunSettings -Fast -Display $f.Display -Output $f.Output
        Check ($f.Console -eq 'SeriesS' -and $f.Display -eq '1920x1080' -and -not $f.Output -and $s.VICTORY_DISPLAY_MODE -eq '1920x1080' -and -not $s.ContainsKey('VICTORY_OUTPUT')) 'the fast preset asks for no picture under 1920x1080 and no output size'
        $f = Get-FastChoices @{ Display = '1280x720'; Output = '1280x720' }
        Check ($f.Display -eq '1280x720' -and $f.Output -eq '1280x720') 'the fast preset keeps a display and an output size that were given'
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

        # First-run setup. Only the pure parts run here: no dialog, no prompt.
        $setupDir = Join-Path $dir 'setup'
        New-Item -ItemType Directory -Force -Path $setupDir | Out-Null
        $template = Join-Path $setupDir 'victory.example.toml'
        $templateText = "# Example`r`n[paths]`r`n# REQUIRED.`r`ngame_dir = 'C:\Games\MyTitle'   # your folder`r`n# cache_dir = 'D:\x'`r`n`r`n[other]`r`ntest_titles = []`r`n"
        [System.IO.File]::WriteAllText($template, $templateText)
        $eacute = [string][char]0x00E9; $uuml = [string][char]0x00FC; $snow = [string][char]0x2603
        $games = [ordered]@{
            'plain'   = 'D:\MyTitles\SomeTitle'
            'spaces'  = 'D:\My Titles\Some Title (2)'
            'quote'   = "D:\Bob's Titles\Some Title"
            'unicode' = "D:\Titres\Caf$eacute Stra${uuml}e $snow"
            'both'    = "D:\Bob's Caf$eacute\x"
        }
        foreach ($name in $games.Keys) {
            $g = $games[$name]
            $cfg = Join-Path $setupDir "victory-$name.toml"
            New-ConfigFile $cfg $g $template
            $raw = $utf8.GetString([System.IO.File]::ReadAllBytes($cfg))
            $expectKind = $(if ($g.Contains("'")) { 'basic' } else { 'literal' })
            $line = @($raw -split "`r`n" | Where-Object { $_ -like 'game_dir = *' })
            $kindOk = $(if ($expectKind -eq 'literal') { $line[0] -eq "game_dir = '$g'   # your folder" } else { $line[0] -eq ('game_dir = "' + $g.Replace('\', '\\') + '"   # your folder') })
            Check ($line.Count -eq 1 -and $kindOk) "config: $name path is written as a $expectKind string"
            Check ((Get-GameDirValue $raw) -eq $g) "config: $name path reads back"
            $viaLocal = Read-LocalConfig $cfg
            Check ($viaLocal['paths'].game_dir -eq $g) "config: $name path reads back through the launcher's own reader"
            Check ($raw.Replace($line[0], "game_dir = 'C:\Games\MyTitle'   # your folder") -eq $templateText) "config: $name changes nothing but game_dir"
        }
        Check ([System.IO.File]::ReadAllBytes((Join-Path $setupDir 'victory-unicode.toml'))[0] -ne 0xEF) 'config: written as UTF-8 without a byte-order mark'
        $existing = Join-Path $setupDir 'victory-plain.toml'
        $before = [System.IO.File]::ReadAllBytes($existing)
        $refused = $false
        try { New-ConfigFile $existing 'D:\Other' $template } catch { $refused = $true }
        Check ($refused -and (@([System.IO.File]::ReadAllBytes($existing)) -join ',') -eq (@($before) -join ',')) 'config: an existing victory.toml is never overwritten'
        $noTemplate = Join-Path $setupDir 'victory-notemplate.toml'
        New-ConfigFile $noTemplate "D:\Bob's" (Join-Path $setupDir 'missing.toml')
        Check ((Get-GameDirValue ([System.IO.File]::ReadAllText($noTemplate))) -eq "D:\Bob's") 'config: without the example file a short one is written'

        # -Setup on an existing file: only the game_dir line changes, after a backup.
        $mine = "# my notes`n`n[paths]`n  game_dir = 'D:\Old'  # was here`ncache_dir = 'E:\cache'`n`n[patches]`ngame_dir = 'not this one'`n"
        Check ((Set-GameDirLine $mine 'D:\New One') -eq $mine.Replace("  game_dir = 'D:\Old'  # was here", "  game_dir = 'D:\New One'  # was here")) 'game_dir: the line is replaced in place, the comment, the indent and LF endings stay'
        Check ((Set-GameDirLine $mine "D:\It's") -eq $mine.Replace("game_dir = 'D:\Old'", 'game_dir = "D:\\It''s"')) 'game_dir: a quote in the path turns the value into an escaped basic string'
        $crlf = $templateText
        Check ((Set-GameDirLine $crlf 'D:\New') -eq $crlf.Replace("'C:\Games\MyTitle'", "'D:\New'")) 'game_dir: CRLF endings stay'
        $noLine = "# c`r`n[paths]`r`n# game_dir = 'D:\commented'`r`ncache_dir = 'E:\c'`r`n[x]`r`na = 1`r`n"
        Check ((Set-GameDirLine $noLine 'D:\New') -eq "# c`r`n[paths]`r`ngame_dir = 'D:\New'`r`n# game_dir = 'D:\commented'`r`ncache_dir = 'E:\c'`r`n[x]`r`na = 1`r`n") 'game_dir: with no uncommented line one is added under [paths]'
        Check ((Set-GameDirLine "[x]`na = 1" 'D:\New') -eq "[x]`na = 1`n`n[paths]`ngame_dir = 'D:\New'`n") 'game_dir: with no [paths] it is added at the end'
        Check ((Get-GameDirValue "[x]`ngame_dir = 'D:\no'`n") -eq $null -and (Get-GameDirValue '') -eq $null -and (Get-GameDirValue "[paths]`n# game_dir = 'D:\no'`n") -eq $null) 'game_dir: only an uncommented line under [paths] counts'
        $mineFile = Join-Path $setupDir 'victory.toml'
        [System.IO.File]::WriteAllBytes($mineFile, (New-Object System.Text.UTF8Encoding($true)).GetPreamble() + $utf8.GetBytes($mine))
        $saved = Save-GameDir $mineFile "D:\Titres\Caf$eacute" $template
        $afterBytes = [System.IO.File]::ReadAllBytes($mineFile)
        Check (-not $saved.Created -and $saved.Backup -and (Test-Path -LiteralPath $saved.Backup) -and $afterBytes[0] -eq 0xEF -and $afterBytes[1] -eq 0xBB) 'setup: an existing file is kept as a backup and keeps its byte-order mark'
        Check ((@([System.IO.File]::ReadAllBytes($saved.Backup)) -join ',') -eq ((@((New-Object System.Text.UTF8Encoding($true)).GetPreamble() + $utf8.GetBytes($mine))) -join ',')) 'setup: the backup is the file as it was, byte for byte'
        $afterText = $utf8.GetString($afterBytes, 3, $afterBytes.Length - 3)
        Check ($afterText -eq $mine.Replace("game_dir = 'D:\Old'", "game_dir = 'D:\Titres\Caf$eacute'")) 'setup: nothing but game_dir changed'
        $saved2 = Save-GameDir $mineFile 'D:\Again' $template
        Check ($saved2.Backup -ne $saved.Backup -and @(Get-ChildItem -LiteralPath $setupDir -Filter 'victory.toml.bak-*').Count -eq 2) 'setup: every change keeps its own backup'
        $fresh = Join-Path $setupDir 'fresh.toml'
        $savedNew = Save-GameDir $fresh 'D:\MyTitles\SomeTitle' $template
        Check ($savedNew.Created -and -not $savedNew.Backup -and (Get-GameDirValue ([System.IO.File]::ReadAllText($fresh))) -eq 'D:\MyTitles\SomeTitle') 'setup: a missing file is created, without a backup'

        # Which file counts, and whether first-run setup is needed.
        $relDir = Join-Path $dir 'first-run-release'
        $srcDir = Join-Path $dir 'first-run-tree'
        New-Item -ItemType Directory -Force -Path $relDir, (Join-Path $srcDir 'target\release') | Out-Null
        Check ((Test-NeedsFirstRun '' '' $relDir $relDir) -and (Test-NeedsFirstRun '' '' $srcDir (Join-Path $srcDir 'target\release'))) 'first run: no victory.toml, nothing set: needed'
        Check (-not (Test-NeedsFirstRun 'X:\any.toml' '' $relDir $relDir) -and -not (Test-NeedsFirstRun '' 'D:\g' $relDir $relDir)) 'first run: VICTORY_CONFIG or VICTORY_GAME_DIR set: not needed'
        [System.IO.File]::WriteAllText((Join-Path $relDir 'victory.toml'), "[paths]`n")
        Check ((-not (Test-NeedsFirstRun '' '' $relDir $relDir)) -and (Find-ConfigFile '' $relDir $relDir) -eq (Join-Path $relDir 'victory.toml')) 'first run: a victory.toml in the release folder: not needed'
        [System.IO.File]::WriteAllText((Join-Path $srcDir 'target\release\victory.toml'), "[paths]`n")
        Check ((Find-ConfigFile '' $srcDir (Join-Path $srcDir 'target\release')) -eq (Join-Path $srcDir 'target\release\victory.toml') -and (Find-ConfigFile 'Z:\named.toml' $srcDir $srcDir) -eq 'Z:\named.toml') 'first run: the file next to victory.exe and the one VICTORY_CONFIG names are found'

        # A path given on the command line.
        $dropDir = Join-Path $dir 'Dropped (1) & Folder'
        New-Item -ItemType Directory -Force -Path $dropDir | Out-Null
        $empty = Join-Path $dir 'no exe here'
        New-Item -ItemType Directory -Force -Path $empty | Out-Null
        $dropExe = Join-Path $dropDir 'title one.exe'
        [System.IO.File]::WriteAllText($dropExe, 'x')
        $dropTxt = Join-Path $dropDir 'readme.txt'
        [System.IO.File]::WriteAllText($dropTxt, 'x')
        $t = Get-LaunchTarget $dropDir
        Check ($t.Kind -eq 'Folder' -and $t.Path -eq $dropDir) 'path argument: a folder (spaces, parentheses, &)'
        Check ((Get-LaunchTarget ('"' + $dropDir + '"')).Path -eq $dropDir -and (Get-LaunchTarget ($dropDir + '\')).Path -eq $dropDir -and (Get-LaunchTarget ($dropDir + '"')).Kind -eq 'Folder') 'path argument: quotes and a trailing backslash are dropped'
        $t = Get-LaunchTarget $dropExe
        Check ($t.Kind -eq 'Exe' -and $t.Path -eq $dropExe) 'path argument: an .exe is the title executable'
        Check ((Get-LaunchTarget $dropTxt).Kind -eq 'NotExe' -and (Get-LaunchTarget (Join-Path $dir 'nothing here')).Kind -eq 'Missing' -and (Get-LaunchTarget '').Kind -eq 'None') 'path argument: another file, a missing path and no argument'
        Check ($null -eq (Test-GameFolder $dropDir) -and (Test-GameFolder $empty) -match 'holds no \.exe' -and (Test-GameFolder (Join-Path $dir 'nothing here')) -match 'does not exist' -and (Test-GameFolder '') -match 'No folder') 'game folder: needs to exist and to hold an .exe'
        $script:pickQueue = New-Object System.Collections.Queue
        foreach ($c in @($empty, (Join-Path $dir 'nothing here'), "`"$dropDir`"")) { $script:pickQueue.Enqueue($c) }
        $stub = { if ($script:pickQueue.Count -gt 0) { $script:pickQueue.Dequeue() } else { $null } }
        $picked = Select-GameFolder $stub 6>$null
        Check ($picked -eq $dropDir) 'folder choice: a folder without an .exe or a missing one is refused and asked for again'
        Check ($null -eq (Select-GameFolder { $null } 6>$null)) 'folder choice: cancelling gives nothing'
        Check ((ConvertFrom-YesNo '' $true) -and -not (ConvertFrom-YesNo '' $false) -and (ConvertFrom-YesNo ' Y ' $false) -and -not (ConvertFrom-YesNo 'n' $true) -and -not (ConvertFrom-YesNo 'later' $true)) 'yes/no answers: empty takes the default'
        Check (-not (Test-InteractiveSession)) 'a redirected session is not interactive'

        # Through play.ps1 and play.cmd: a dropped folder with spaces and parentheses reaches the script whole.
        $viaDrop = @(& powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath -ShowSettings $dropDir)
        Check ($LASTEXITCODE -eq 0 -and ($viaDrop -contains "VICTORY_GAME_DIR=$dropDir")) 'a folder argument arrives through powershell -File'
        $viaDropExe = @(& powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath -ShowSettings $dropExe)
        Check ($LASTEXITCODE -eq 0 -and ($viaDropExe -contains "-Exe $dropExe")) 'an .exe argument arrives as -Exe'
        $viaBad = @(& powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath -ShowSettings (Join-Path $dir 'nothing here'))
        Check ($LASTEXITCODE -eq 1 -and ($viaBad -join ' ') -match 'nothing here') 'a path that is not there is named, and nothing starts'
        if (Test-Path -LiteralPath $cmdFile) {
            # As Explorer starts it: the path in quotes as the first argument (written to a file so no shell re-quotes it).
            $dropper = Join-Path $setupDir 'drop.cmd'
            [System.IO.File]::WriteAllText($dropper, "@echo off`r`ncall `"$cmdFile`" `"$dropDir`" -ShowSettings`r`n")
            $env:ROXBOX_NO_PAUSE = '1'
            try { $viaCmdDrop = @(& cmd.exe /d /c $dropper) } finally { Remove-Item Env:ROXBOX_NO_PAUSE -ErrorAction SilentlyContinue }
            Check ($LASTEXITCODE -eq 0 -and ($viaCmdDrop -contains "VICTORY_GAME_DIR=$dropDir")) 'a dropped folder with spaces and parentheses arrives through play.cmd'
        }

        if ($fail) { Write-Host "$fail check(s) failed"; exit 1 }
        Write-Host 'self-test passed'
    } finally { Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
    return
}

try {
    $More = ConvertTo-PairTable $More '-More'
    $SettingsValues = ConvertTo-PairTable $SettingsValues '-SettingsValues'
} catch { Write-Host $_.Exception.Message; exit 1 }
# A path as the first argument (what dropping a folder or an .exe on play.cmd gives).
$dropFolder = $null
$droppedExe = $false
if ($Path) {
    $target = Get-LaunchTarget $Path
    if ($target.Kind -eq 'Folder') { $dropFolder = $target.Path }
    elseif ($target.Kind -eq 'Exe') {
        if ($Exe) { Write-Host "Two title executables were given ($Exe and $Path): give one."; exit 1 }
        $Exe = $target.Path
        $droppedExe = $true
    } elseif ($target.Kind -eq 'NotExe') {
        Write-Host "$($target.Path) is neither a folder nor an .exe file. Drop the folder that holds your title files (or its .exe) on play.cmd."
        exit 1
    } else {
        Write-Host "$($target.Path) was not found. Check the path, or drop the folder that holds your title files on play.cmd."
        exit 1
    }
}
if ($ShowSettings) {
    # ($Output keeps its pattern check: the preset's "none" is not assigned to it.)
    if ($Fast) { $f = Get-FastChoices $PSBoundParameters; $Display = $f.Display; if ($f.Output) { $Output = $f.Output } }
    $shown = Get-RunSettings -Safe:$Safe -RayTracing:$RayTracing -Fast:$Fast -Display $Display -Output $Output -More $More
    $shown.Keys | Sort-Object | ForEach-Object { Write-Host "$_=$($shown[$_])" }
    $SettingsValues.Keys | Sort-Object | ForEach-Object { Write-Host "setting $_=$($SettingsValues[$_])" }
    if ($dropFolder) { Write-Host "VICTORY_GAME_DIR=$dropFolder" }
    if ($Exe) { Write-Host "-Exe $Exe" }
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
    $Console = $f.Console; $Display = $f.Display
    if ($f.Output) { $Output = $f.Output }
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

$keep = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_PATCHES_FILE', 'VICTORY_SHADER_PROFILE', 'VICTORY_BCPACK_TABLES', 'VICTORY_SKIP_PREFLIGHT', 'VICTORY_SKIP_CPU_CHECK', 'VICTORY_REPORT'
Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $keep } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }

# victory.toml is looked for in the current directory first: the folder it belongs in.
Push-Location $root
try {
    # First-run setup and -Setup: which folder holds the title's files, remembered in victory.toml.
    # -Check only reports, so it never asks; nothing asks when nobody can answer (CI, redirected input).
    $interactive = Test-InteractiveSession
    $configFile = Find-ConfigFile $env:VICTORY_CONFIG $root (Split-Path -Parent $victory)
    $needsFirstRun = Test-NeedsFirstRun $env:VICTORY_CONFIG $env:VICTORY_GAME_DIR $root (Split-Path -Parent $victory)
    if ($dropFolder) {
        $problem = Test-GameFolder $dropFolder
        if ($problem) { Write-Host $problem; exit 1 }
        $env:VICTORY_GAME_DIR = $dropFolder
    }
    $afterSetup = $false
    if (-not $Check -or $Setup) {
        $chosen = $null
        $explainedCancel = 'No folder was chosen, so nothing was changed. Run play.cmd again (or play.cmd -Setup) when you are ready.'
        $offer = $(if ($dropFolder) { $dropFolder } elseif ($droppedExe) { Split-Path -Parent $Exe })
        if ($Setup) {
            if ($dropFolder) { $chosen = $dropFolder }
            elseif ($interactive) {
                Write-Host 'Pick the folder that holds your own, legally obtained title files (the title''s .exe and everything beside it).'
                $chosen = Select-GameFolder
                if (-not $chosen) { Write-Host $explainedCancel; exit 0 }
            } else {
                Write-Host '-Setup opens a window to pick the folder in, so it needs a person at a normal window (not CI or redirected input). Run play.cmd -Setup in a command prompt, or drop the folder on play.cmd.'
                exit 1
            }
        } elseif ($needsFirstRun -and $interactive) {
            if ($offer) {
                Write-Host "Your title files are in $offer."
                if (Read-YesNo 'Remember this folder for next time?') { $chosen = $offer }
            } else {
                Write-Host 'Welcome to Rox-box. It needs to know where your own, legally obtained title files are:'
                Write-Host 'the folder that holds the title''s .exe. A window opens next to pick it; it is remembered on this PC only.'
                $chosen = Select-GameFolder
                if (-not $chosen) { Write-Host $explainedCancel; exit 0 }
                $afterSetup = $true
            }
        }
        if ($chosen) {
            $afterSetup = $afterSetup -or $Setup
            $configTarget = $(if ($configFile) { $configFile } else { Join-Path $root 'victory.toml' })
            try { $saved = Save-GameDir $configTarget $chosen (Join-Path $root 'victory.example.toml') }
            catch { Write-Host "Could not save the folder: $($_.Exception.Message)"; exit 1 }
            Write-Host "Saved: game_dir in $configTarget is now $chosen"
            if ($saved.Backup) { Write-Host "The file as it was is kept as $($saved.Backup)" }
            if ($env:VICTORY_GAME_DIR -and $env:VICTORY_GAME_DIR -ne $chosen) {
                Write-Host 'Note: the VICTORY_GAME_DIR setting of this PC takes precedence over victory.toml; remove it for this folder to be used.'
            }
        }
    }
    if ($afterSetup -and -not $Check) {
        Write-Host ''
        Write-Host 'Checking your PC and that folder (victory check):'
        & $victory check @exeArgs
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'Not starting: fix what is MISSING above (QUICKSTART.txt / README.md), then run this again.'
            exit 1
        }
        if (-not $interactive -or -not (Read-YesNo 'Start now?')) {
            Write-Host 'Run play.cmd whenever you want to start.'
            exit 0
        }
    }
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
