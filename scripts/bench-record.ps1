# Records the self-test benchmark (and, optionally, a run of the open demo title) as numbers, and can compare two records.
#   scripts\bench-record.ps1 -Victory <victory.exe> -Out <folder> [-Runs 5] [-DemoTitle <demo-title.exe>] [-DemoFrames 600]
#   scripts\bench-record.ps1 -Victory <victory.exe> -Out <folder> -Compare <older bench.json> [-Threshold 30]
#   scripts\bench-record.ps1 -SelfTest        tests the parsing, median and comparison functions; needs no victory.exe
# Windows PowerShell 5.1 and PowerShell 7 both run it; it needs no module.
#
# What it does
#   1. Runs `victory.exe selftest --warp --bench` -Runs times (on the software adapter, as docs\selftest.md says) and keeps,
#      per rate, the median, the minimum and the maximum of the runs.
#   2. With -DemoTitle: one run of the demo title (docs\demo-title.md; the executable in a folder that also holds its
#      DLLs, which are taken along) with `victory.exe run <demo> --execute -- --frames
#      <n>`, in a game, a cache and a data folder made under -Out (never anywhere else; the three VICTORY_*_DIR variables and
#      VICTORY_D3D12_WARP are set for that child process only), from a current folder that holds no victory.toml. The figures
#      are read from the newest run report (average_fps, frame_ms_p50, frame_ms_p99, preparing_ms, waiting_for_card_ms).
#   3. Writes bench.json (flat, sorted, the same keys every time) and bench.md (a table) into -Out and prints the table.
#
# Exit code: non-zero only when a run itself fails (a self-test stage FAIL or a non-zero exit code, the demo's exit code
# not 0, or a report that does not say title_exited with code 0), never because of a number.
#
# Why it warns and does not gate: hosted CI runners are shared machines, and the same build moves by tens of percent from
# one run to the next (docs\selftest.md). A hard limit on a number would fail builds at random and teach people to ignore
# it. -Compare prints, per figure, the old median, the new median and the change, marks a change beyond -Threshold percent
# with a "!" and a GitHub Actions ::warning:: line, and still exits 0. A person reads it; nothing fails on it.
#
# The self-test runs set VICTORY_REPORT=0 for their child process: they write no report file of their own.
[CmdletBinding()]
param(
    [string]$Victory,
    [string]$Out,
    [int]$Runs = 5,
    [string]$DemoTitle,
    [int]$DemoFrames = 600,
    [string]$Compare,
    [double]$Threshold = 30,
    [switch]$SelfTest
)

$inv = [System.Globalization.CultureInfo]::InvariantCulture

# ---------------------------------------------------------------------------------------------------------------------
# Pure functions (covered by -SelfTest)
# ---------------------------------------------------------------------------------------------------------------------

# The benchmark's lines of the self-test's output: `name value unit`, e.g. `buffer_copy 10350.96 MB/s`, between the
# "Benchmark" heading and the next empty line. Returns the rates, the adapter (from the Device stage's line) and the
# stage lines that say FAIL.
function ConvertFrom-SelfTestOutput([string[]]$Lines) {
    $rates = @()
    $adapter = ''
    $failed = @()
    $inBench = $false
    foreach ($raw in $Lines) {
        $line = "$raw"
        if ($line -match '^\[\s*FAIL\s*\]') { $failed += $line.Trim() }
        if ($line -match '^\[\s*OK\s*\]\s+Device\s+([^;]+)') { $adapter = $Matches[1].Trim() }
        if ($line -match '^Benchmark\b') { $inBench = $true; continue }
        if ($inBench) {
            if ($line.Trim() -eq '') { $inBench = $false; continue }
            if ($line -match '^\s*([a-z][a-z0-9_]*)\s+(-?\d+(?:\.\d+)?)\s+(\S+)\s*$') {
                $rates += [pscustomobject]@{
                    Name  = $Matches[1]
                    Value = [double]::Parse($Matches[2], $inv)
                    Unit  = $Matches[3]
                }
            }
        }
    }
    [pscustomobject]@{ Rates = $rates; Adapter = $adapter; Failed = $failed }
}

function Get-Median([double[]]$Values) {
    $v = @($Values | Sort-Object)
    if ($v.Count -eq 0) { return $null }
    $mid = [int][math]::Floor($v.Count / 2)
    if ($v.Count % 2 -eq 1) { return [double]$v[$mid] }
    return ([double]$v[$mid - 1] + [double]$v[$mid]) / 2
}

# One figure from the readings of several runs.
function New-Figure([string]$Name, [string]$Unit, [double[]]$Values) {
    $sorted = @($Values | Sort-Object)
    [pscustomobject]@{
        Name   = $Name
        Unit   = $Unit
        Median = Get-Median $Values
        Min    = [double]$sorted[0]
        Max    = [double]$sorted[$sorted.Count - 1]
    }
}

# The figures of several parsed runs: per rate name the median, minimum and maximum over the runs that have it
# (a rate the PC cannot measure is left out of a run, so it may be missing from some), sorted by name.
function Merge-Rates($ParsedRuns) {
    $by = @{}
    $units = @{}
    foreach ($run in $ParsedRuns) {
        foreach ($r in $run.Rates) {
            if (-not $by.ContainsKey($r.Name)) { $by[$r.Name] = @(); $units[$r.Name] = $r.Unit }
            $by[$r.Name] += $r.Value
        }
    }
    $names = [string[]]@($by.Keys)
    [Array]::Sort($names, [System.StringComparer]::Ordinal)
    @($names | ForEach-Object { New-Figure $_ $units[$_] ([double[]]$by[$_]) })
}

# The keys wanted out of a TOML text that we wrote ourselves: just `[section]` lines and `key = value` lines.
# $Wanted: section name -> key names. Returns "section.key" -> the value's text (quotes taken off). The first
# occurrence of a key in its section wins; `[[tables]]` and sub-sections such as [frame_time.steady] are not the section.
function Read-TomlKeys([string]$Text, [hashtable]$Wanted) {
    $found = @{}
    $section = ''
    foreach ($raw in ($Text -split "`r?`n")) {
        $line = $raw.Trim()
        if ($line -match '^\[\[') { $section = ''; continue }
        if ($line -match '^\[([^\[\]]+)\]$') { $section = $Matches[1].Trim(); continue }
        if ($line -match '^([A-Za-z0-9_]+)\s*=\s*(.*)$') {
            $key = $Matches[1]
            $value = $Matches[2].Trim()
            if ($Wanted.ContainsKey($section) -and ($Wanted[$section] -contains $key) -and -not $found.ContainsKey("$section.$key")) {
                if ($value -match '^"(.*)"$') { $value = $Matches[1] }
                elseif ($value -match '^([^#]*?)\s+#') { $value = $Matches[1] }
                $found["$section.$key"] = $value
            }
        }
    }
    $found
}

# The demo's figures from the text of its run report: figure name -> unit, and the run's ending.
$script:DemoKeys = [ordered]@{
    'frames.average_fps'           = @('demo.average_fps', 'fps')
    'frames.frame_ms_p50'          = @('demo.frame_ms_p50', 'ms')
    'frames.frame_ms_p99'          = @('demo.frame_ms_p99', 'ms')
    'frame_time.preparing_ms'      = @('demo.preparing_ms', 'ms')
    'frame_time.waiting_for_card_ms' = @('demo.waiting_for_card_ms', 'ms')
}

function ConvertFrom-DemoReport([string]$Text) {
    $wanted = @{
        'frames'     = @('average_fps', 'frame_ms_p50', 'frame_ms_p99')
        'frame_time' = @('preparing_ms', 'waiting_for_card_ms')
        'run'        = @('ended', 'exit_code')
    }
    $k = Read-TomlKeys $Text $wanted
    $figures = @()
    $missing = @()
    foreach ($path in $script:DemoKeys.Keys) {
        $pair = $script:DemoKeys[$path]
        $d = 0.0
        if ($k.ContainsKey($path) -and [double]::TryParse($k[$path], [System.Globalization.NumberStyles]::Float, $inv, [ref]$d)) {
            $figures += New-Figure $pair[0] $pair[1] @($d)
        } else {
            $missing += $path
        }
    }
    $code = -1
    $c = 0
    if ($k.ContainsKey('run.exit_code') -and [int]::TryParse($k['run.exit_code'], [ref]$c)) { $code = $c }
    $ended = ''
    if ($k.ContainsKey('run.ended')) { $ended = $k['run.ended'] }
    [pscustomobject]@{ Figures = $figures; Ended = $ended; ExitCode = $code; Missing = $missing }
}

# The comparison of two sets of figures (name -> figure hashtables), sorted by name: the old median, the new median and the
# change in percent; Flag is true when the change is beyond the threshold (either way: the script does not know which
# direction is better for a figure, and a person reads it).
function Compare-Figures([hashtable]$Old, [hashtable]$New, [double]$Threshold) {
    $names = [string[]]@(@($Old.Keys) + @($New.Keys) | Select-Object -Unique)
    [Array]::Sort($names, [System.StringComparer]::Ordinal)
    foreach ($n in $names) {
        $o = $null; $w = $null
        if ($Old.ContainsKey($n)) { $o = [double]$Old[$n].Median }
        if ($New.ContainsKey($n)) { $w = [double]$New[$n].Median }
        $unit = ''
        if ($New.ContainsKey($n)) { $unit = $New[$n].Unit } elseif ($Old.ContainsKey($n)) { $unit = $Old[$n].Unit }
        $pct = $null
        if (($null -ne $o) -and ($null -ne $w) -and ($o -ne 0)) { $pct = ($w - $o) / [math]::Abs($o) * 100 }
        $flag = $false
        if (($null -ne $pct) -and ([math]::Abs($pct) -gt $Threshold)) { $flag = $true }
        [pscustomobject]@{ Name = $n; Unit = $unit; Old = $o; New = $w; Percent = $pct; Flag = $flag }
    }
}

function Format-Number($Value) {
    if ($null -eq $Value) { return '-' }
    $v = [double]$Value
    if ([math]::Abs($v) -ge 1000) { return $v.ToString('0.#', $inv) }
    return $v.ToString('0.###', $inv)
}

function Format-Percent($Value) {
    if ($null -eq $Value) { return '-' }
    return ([double]$Value).ToString('+0.0;-0.0;0.0', $inv) + '%'
}

# The comparison as lines of text, plus the ::warning:: lines for GitHub Actions.
function Format-Comparison($Rows, [double]$Threshold) {
    $lines = @()
    $lines += ('{0,-30} {1,14} {2,14} {3,9}  {4}' -f 'figure', 'old median', 'new median', 'change', 'unit')
    foreach ($r in $Rows) {
        $mark = ''
        if ($r.Flag) { $mark = ' !' }
        $lines += ('{0,-30} {1,14} {2,14} {3,9}  {4}{5}' -f $r.Name, (Format-Number $r.Old), (Format-Number $r.New), (Format-Percent $r.Percent), $r.Unit, $mark)
    }
    foreach ($r in $Rows) {
        if ($r.Flag) {
            $lines += ('::warning::bench: {0} moved {1} (median {2} -> {3} {4}); beyond {5}%, on a shared runner this can be noise' -f $r.Name, (Format-Percent $r.Percent), (Format-Number $r.Old), (Format-Number $r.New), $r.Unit, (Format-Number $Threshold))
        }
    }
    $lines
}

function ConvertTo-JsonString([string]$S) {
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    foreach ($ch in $S.ToCharArray()) {
        $code = [int]$ch
        if ($ch -eq '"') { [void]$sb.Append('\"') }
        elseif ($ch -eq '\') { [void]$sb.Append('\\') }
        elseif ($code -lt 32 -or $code -gt 126) { [void]$sb.Append(('\u{0:x4}' -f $code)) }
        else { [void]$sb.Append($ch) }
    }
    [void]$sb.Append('"')
    $sb.ToString()
}

# bench.json: the same keys in the same order every time, figures sorted by name, one per line.
function ConvertTo-BenchJson($Bench) {
    $demoEnded = 'null'; $demoCode = 'null'; $demoFrames = 'null'
    if ($Bench.DemoEnded) {
        $demoEnded = ConvertTo-JsonString $Bench.DemoEnded
        $demoCode = [string]$Bench.DemoExitCode
        $demoFrames = [string]$Bench.DemoFrames
    }
    $rows = @()
    foreach ($f in $Bench.Figures) {
        $rows += ('    {{"name": {0}, "unit": {1}, "median": {2}, "min": {3}, "max": {4}}}' -f (ConvertTo-JsonString $f.Name), (ConvertTo-JsonString $f.Unit), (Format-Json-Number $f.Median), (Format-Json-Number $f.Min), (Format-Json-Number $f.Max))
    }
    $out = @()
    $out += '{'
    $out += '  "format": 1,'
    $out += ('  "rox_box_version": {0},' -f (ConvertTo-JsonString $Bench.Version))
    $out += ('  "adapter": {0},' -f (ConvertTo-JsonString $Bench.Adapter))
    $out += ('  "selftest_runs": {0},' -f $Bench.Runs)
    $out += ('  "demo_frames": {0},' -f $demoFrames)
    $out += ('  "demo_ended": {0},' -f $demoEnded)
    $out += ('  "demo_exit_code": {0},' -f $demoCode)
    $out += '  "figures": ['
    $out += ($rows -join (',' + "`n"))
    $out += '  ]'
    $out += '}'
    ($out -join "`n") + "`n"
}

function Format-Json-Number([double]$V) { [math]::Round($V, 4).ToString('0.####', $inv) }

# A bench.json read back: name -> figure (Name, Unit, Median, Min, Max).
function Read-BenchJson([string]$Text) {
    $j = $Text | ConvertFrom-Json
    $map = @{}
    foreach ($f in @($j.figures)) {
        $map[[string]$f.name] = [pscustomobject]@{ Name = [string]$f.name; Unit = [string]$f.unit; Median = [double]$f.median; Min = [double]$f.min; Max = [double]$f.max }
    }
    [pscustomobject]@{ Version = [string]$j.rox_box_version; Adapter = [string]$j.adapter; Figures = $map }
}

function ConvertTo-BenchMarkdown($Bench) {
    $md = @()
    $md += '### Self-test benchmark and demo title'
    $md += ''
    $md += ('Rox-box {0}; adapter: {1}; self-test runs: {2}' -f $Bench.Version, $Bench.Adapter, $Bench.Runs)
    if ($Bench.DemoEnded) {
        $md += ('Demo title: {0} frames, ended `{1}` with exit code {2}' -f $Bench.DemoFrames, $Bench.DemoEnded, $Bench.DemoExitCode)
    }
    $md += ''
    $md += '| Figure | Median | Min | Max | Unit |'
    $md += '|---|---:|---:|---:|---|'
    foreach ($f in $Bench.Figures) {
        $md += ('| {0} | {1} | {2} | {3} | {4} |' -f $f.Name, (Format-Number $f.Median), (Format-Number $f.Min), (Format-Number $f.Max), $f.Unit)
    }
    $md += ''
    $md += 'Shared runners are noisy: read these as a trend over many runs, not as a limit.'
    ($md -join "`n") + "`n"
}

# ---------------------------------------------------------------------------------------------------------------------
# -SelfTest
# ---------------------------------------------------------------------------------------------------------------------

function Invoke-ScriptSelfTest {
    $script:failures = 0
    $script:checks = 0
    function Check([string]$What, $Got, $Want) {
        $script:checks++
        if ("$Got" -ne "$Want") { $script:failures++; Write-Host "FAIL  $What : got [$Got], wanted [$Want]" }
        else { Write-Host "ok    $What" }
    }

    $sample = @'
victory 0.2.0 self-test on the software adapter (WARP) (a stand-in title of our own; no game files)

[  OK   ] Device            Microsoft Basic Render Driver (software); queue, list, fence and a 64x64 window
[  OK   ] Clear + present   120 frames cleared and presented; 3 of them read back and matched

Benchmark (name, value, unit; docs/selftest.md says how to read them):
present_interval                   0.46 ms
buffer_copy                    11695.48 MB/s
list_recording               4737652.49 calls/s

Self-test passed: 5 of 5 stages OK, 0 skipped.
'@
    $p = ConvertFrom-SelfTestOutput ($sample -split "`r?`n")
    Check 'parse: three rates' $p.Rates.Count 3
    Check 'parse: first name' $p.Rates[0].Name 'present_interval'
    Check 'parse: value' $p.Rates[1].Value 11695.48
    Check 'parse: unit with a slash' $p.Rates[1].Unit 'MB/s'
    Check 'parse: large value' $p.Rates[2].Value 4737652.49
    Check 'parse: adapter' $p.Adapter 'Microsoft Basic Render Driver (software)'
    Check 'parse: no failed stage' $p.Failed.Count 0
    $bad = ConvertFrom-SelfTestOutput @('[ FAIL ] Clear + present   a pixel differed', 'Benchmark (x):', 'a_rate 1.5 ms', '', 'not_a_rate 9 ms')
    Check 'parse: a FAIL line is found' $bad.Failed.Count 1
    Check 'parse: the benchmark ends at the empty line' $bad.Rates.Count 1

    Check 'median: odd' (Get-Median @(5, 1, 3)) 3
    Check 'median: even' (Get-Median @(4, 1, 3, 2)) 2.5
    Check 'median: one' (Get-Median @(7)) 7

    $r1 = ConvertFrom-SelfTestOutput @('Benchmark (x):', 'a 10 ms', 'b 1 MB/s', '')
    $r2 = ConvertFrom-SelfTestOutput @('Benchmark (x):', 'a 30 ms', 'b 3 MB/s', '')
    $r3 = ConvertFrom-SelfTestOutput @('Benchmark (x):', 'a 20 ms', '')
    $m = @(Merge-Rates @($r1, $r2, $r3))
    Check 'merge: two figures' $m.Count 2
    Check 'merge: a median' $m[0].Median 20
    Check 'merge: a min' $m[0].Min 10
    Check 'merge: a max' $m[0].Max 30
    Check 'merge: a rate missing from a run' $m[1].Median 2
    Check 'merge: unit' $m[1].Unit 'MB/s'

    $report = @'
format = 5
[run]
console = "series-x"
ended = "title_exited"
exit_code = 0

[frames]
presents = 600
average_fps = 58.5
frame_ms_p50 = 16.75
frame_ms_p99 = 33.25 # a comment is not expected, but a number must still parse

[frame_time]
preparing_ms = 1.25
waiting_for_card_ms = 2.5

[frame_time.steady]
preparing_ms = 9.99
'@
    $d = ConvertFrom-DemoReport $report
    Check 'report: ended' $d.Ended 'title_exited'
    Check 'report: exit code' $d.ExitCode 0
    Check 'report: five figures' $d.Figures.Count 5
    Check 'report: average_fps' ($d.Figures | Where-Object { $_.Name -eq 'demo.average_fps' }).Median 58.5
    Check 'report: the whole-run section, not the steady one' ($d.Figures | Where-Object { $_.Name -eq 'demo.preparing_ms' }).Median 1.25
    $dm = ConvertFrom-DemoReport "[run]`nended = `"crash`"`nexit_code = 3`n"
    Check 'report: missing figures are listed' $dm.Missing.Count 5
    Check 'report: another ending' $dm.Ended 'crash'

    $bench = [pscustomobject]@{ Version = '0.2.0'; Adapter = 'A "quoted" adapter'; Runs = 3; DemoFrames = 600; DemoEnded = 'title_exited'; DemoExitCode = 0; Figures = @($m + $d.Figures) }
    $json = ConvertTo-BenchJson $bench
    $back = Read-BenchJson $json
    Check 'json: version' $back.Version '0.2.0'
    Check 'json: adapter with quotes' $back.Adapter 'A "quoted" adapter'
    Check 'json: figure count' $back.Figures.Count 7
    Check 'json: a median survives' $back.Figures['a'].Median 20
    Check 'json: same text twice' (ConvertTo-BenchJson $bench) $json
    Check 'md: has a table row' ((ConvertTo-BenchMarkdown $bench) -match '\| a \| 20 \| 10 \| 30 \| ms \|') $true

    $old = @{
        'a' = [pscustomobject]@{ Name = 'a'; Unit = 'ms'; Median = 10.0 }
        'b' = [pscustomobject]@{ Name = 'b'; Unit = 'MB/s'; Median = 100.0 }
        'c' = [pscustomobject]@{ Name = 'c'; Unit = 'ms'; Median = 5.0 }
        'z' = [pscustomobject]@{ Name = 'z'; Unit = 'ms'; Median = 0.0 }
    }
    $new = @{
        'a' = [pscustomobject]@{ Name = 'a'; Unit = 'ms'; Median = 14.0 }
        'b' = [pscustomobject]@{ Name = 'b'; Unit = 'MB/s'; Median = 90.0 }
        'd' = [pscustomobject]@{ Name = 'd'; Unit = 'ms'; Median = 1.0 }
        'z' = [pscustomobject]@{ Name = 'z'; Unit = 'ms'; Median = 3.0 }
    }
    $rows = @(Compare-Figures $old $new 30)
    Check 'compare: every name once, sorted' (($rows | ForEach-Object { $_.Name }) -join ',') 'a,b,c,d,z'
    Check 'compare: +40 percent' $rows[0].Percent 40
    Check 'compare: beyond the threshold is flagged' $rows[0].Flag $true
    Check 'compare: -10 percent' $rows[1].Percent -10
    Check 'compare: within the threshold is not flagged' $rows[1].Flag $false
    Check 'compare: only in the old record' ($null -eq $rows[2].New) $true
    Check 'compare: only in the new record is not flagged' $rows[3].Flag $false
    Check 'compare: an old median of 0 has no percentage' ($null -eq $rows[4].Percent) $true
    Check 'compare: a stricter threshold flags more' (@(Compare-Figures $old $new 5 | Where-Object { $_.Flag }).Count) 2
    $text = @(Format-Comparison $rows 30)
    Check 'format: one warning line' (@($text | Where-Object { $_ -like '::warning::*' }).Count) 1
    Check 'format: the flagged row has a !' ($text[1] -match ' !$') $true
    Check 'format: the other rows have none' ($text[2] -match ' !$') $false

    Write-Host ''
    if ($script:failures -ne 0) {
        Write-Host "bench-record self-test FAILED: $($script:failures) of $($script:checks) checks"
        return 1
    }
    Write-Host "bench-record self-test passed: $($script:checks) checks"
    return 0
}

if ($SelfTest) {
    $code = Invoke-ScriptSelfTest
    exit $code
}

# ---------------------------------------------------------------------------------------------------------------------
# Running victory.exe
# ---------------------------------------------------------------------------------------------------------------------

function Stop-Recording([string]$Why) { Write-Host "BENCH RECORD FAILED: $Why"; exit 1 }

function ConvertTo-ArgumentText([string]$Argument) {
    if ($Argument -match '[\s"]') { return '"' + ($Argument -replace '"', '\"') + '"' }
    $Argument
}

# Runs a program with its output captured; $Env is set for the child only. Returns ExitCode, Out, Err, TimedOut.
function Invoke-Captured([string]$Exe, [string[]]$Arguments, [hashtable]$Env, [string]$Cwd, [int]$TimeoutSeconds) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = (($Arguments | ForEach-Object { ConvertTo-ArgumentText $_ }) -join ' ')
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    if ($Cwd) { $psi.WorkingDirectory = $Cwd }
    foreach ($k in $Env.Keys) { $psi.EnvironmentVariables[$k] = [string]$Env[$k] }
    $proc = [System.Diagnostics.Process]::Start($psi)
    $outTask = $proc.StandardOutput.ReadToEndAsync()
    $errTask = $proc.StandardError.ReadToEndAsync()
    $timedOut = $false
    if (-not $proc.WaitForExit($TimeoutSeconds * 1000)) {
        $timedOut = $true
        try { $proc.Kill() } catch { }
        [void]$proc.WaitForExit(10000)
    }
    $proc.WaitForExit()
    [pscustomobject]@{ ExitCode = $proc.ExitCode; Out = $outTask.Result; Err = $errTask.Result; TimedOut = $timedOut }
}

if (-not $Victory) { Stop-Recording '-Victory <path to victory.exe> is required' }
if (-not $Out) { Stop-Recording '-Out <folder> is required' }
if (-not (Test-Path -LiteralPath $Victory -PathType Leaf)) { Stop-Recording "no such file: $Victory" }
if ($Runs -lt 1) { Stop-Recording '-Runs must be at least 1' }
if ($DemoTitle -and -not (Test-Path -LiteralPath $DemoTitle -PathType Leaf)) { Stop-Recording "no such file: $DemoTitle" }
if ($Compare -and -not (Test-Path -LiteralPath $Compare -PathType Leaf)) { Stop-Recording "no such file: $Compare" }

$Victory = (Resolve-Path -LiteralPath $Victory).Path
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = (Resolve-Path -LiteralPath $Out).Path

# The version, as `victory.exe --version` says it ("victory 0.2.0").
$v = Invoke-Captured $Victory @('--version') @{} $Out 60
if ($v.ExitCode -ne 0) { Stop-Recording "victory --version exited with code $($v.ExitCode)" }
$version = ($v.Out.Trim())
if ($version -match '(\d+\.\d+\.\d+\S*)') { $version = $Matches[1] }

# The self-test runs.
$parsed = @()
$adapter = ''
for ($i = 1; $i -le $Runs; $i++) {
    Write-Host "self-test run $i of $Runs ..."
    $r = Invoke-Captured $Victory @('selftest', '--warp', '--bench') @{ VICTORY_REPORT = '0' } $Out 600
    $p = ConvertFrom-SelfTestOutput (($r.Out -split "`r?`n"))
    if ($r.TimedOut) { Write-Host $r.Out; Stop-Recording "self-test run $i did not end within 10 minutes" }
    if ($r.ExitCode -ne 0 -or $p.Failed.Count -gt 0) {
        Write-Host $r.Out
        Write-Host $r.Err
        Stop-Recording "self-test run $i failed (exit code $($r.ExitCode); failed stages: $($p.Failed.Count))"
    }
    if ($p.Rates.Count -eq 0) { Write-Host $r.Out; Stop-Recording "self-test run $i printed no benchmark rates" }
    if ($p.Adapter) { $adapter = $p.Adapter }
    $parsed += $p
}
$figures = @(Merge-Rates $parsed)

# The demo title, once, in folders under -Out only.
$demoEnded = ''
$demoCode = -1
if ($DemoTitle) {
    $DemoTitle = (Resolve-Path -LiteralPath $DemoTitle).Path
    $root = Join-Path $Out 'demo-run'
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
    $game = Join-Path $root 'game'
    $cache = Join-Path $root 'cache'
    $data = Join-Path $root 'data'
    New-Item -ItemType Directory -Force -Path $game, $cache, $data | Out-Null
    $exe = Join-Path $game 'demo-title.exe'
    Copy-Item -LiteralPath $DemoTitle -Destination $exe
    # The demo ships DLLs of its own (docs\demo-title.md): those beside the executable given are its
    # game folder's, and go with it. A build folder of the executable alone holds none.
    Get-ChildItem -LiteralPath (Split-Path -Parent $DemoTitle) -Filter '*.dll' -File | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $game $_.Name)
    }
    Write-Host "demo title run ($DemoFrames frames) ..."
    # The current folder is $root: it holds no victory.toml, so only the three variables are read.
    $env4 = @{ VICTORY_GAME_DIR = $game; VICTORY_CACHE_DIR = $cache; VICTORY_DATA_DIR = $data; VICTORY_D3D12_WARP = '1' }
    $r = Invoke-Captured $Victory @('run', $exe, '--execute', '--', '--frames', "$DemoFrames") $env4 $root 900
    if ($r.TimedOut) { Write-Host $r.Out; Write-Host $r.Err; Stop-Recording 'the demo title run did not end within 15 minutes' }
    $reports = @(Get-ChildItem -LiteralPath (Join-Path $data 'reports') -Filter 'run-*.txt' -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending)
    if ($reports.Count -eq 0) {
        Write-Host $r.Out
        Write-Host $r.Err
        Stop-Recording "the demo title run (exit code $($r.ExitCode)) wrote no run report"
    }
    $d = ConvertFrom-DemoReport ([System.IO.File]::ReadAllText($reports[0].FullName))
    $demoEnded = $d.Ended
    $demoCode = $d.ExitCode
    if ($r.ExitCode -ne 0 -or $d.Ended -ne 'title_exited' -or $d.ExitCode -ne 0) {
        Write-Host $r.Out
        Write-Host $r.Err
        Stop-Recording "the demo title run failed (process exit code $($r.ExitCode); report: ended = '$($d.Ended)', exit_code = $($d.ExitCode))"
    }
    if ($d.Missing.Count -gt 0) { Write-Host "note: the report lacks: $($d.Missing -join ', ')" }
    $figures = @($figures + $d.Figures)
    $sortNames = [string[]]@($figures | ForEach-Object { $_.Name })
    [Array]::Sort($sortNames, [System.StringComparer]::Ordinal)
    $figures = @($sortNames | ForEach-Object { $n = $_; $figures | Where-Object { $_.Name -eq $n } | Select-Object -First 1 })
}

$bench = [pscustomobject]@{
    Version = $version; Adapter = $adapter; Runs = $Runs
    DemoFrames = $DemoFrames; DemoEnded = $demoEnded; DemoExitCode = $demoCode
    Figures = $figures
}
$utf8 = New-Object System.Text.UTF8Encoding($false)
$json = ConvertTo-BenchJson $bench
$md = ConvertTo-BenchMarkdown $bench
[System.IO.File]::WriteAllText((Join-Path $Out 'bench.json'), $json, $utf8)
[System.IO.File]::WriteAllText((Join-Path $Out 'bench.md'), $md, $utf8)
Write-Host ''
Write-Host $md

# Against an older record: warnings only.
if ($Compare) {
    $old = Read-BenchJson ([System.IO.File]::ReadAllText((Resolve-Path -LiteralPath $Compare).Path))
    $new = Read-BenchJson $json
    Write-Host ("Compared with {0} (Rox-box {1}); '!' marks a change beyond {2}%:" -f $Compare, $old.Version, (Format-Number $Threshold))
    $rows = @(Compare-Figures $old.Figures $new.Figures $Threshold)
    foreach ($line in (Format-Comparison $rows $Threshold)) { Write-Host $line }
}
exit 0
