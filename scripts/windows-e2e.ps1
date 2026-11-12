# End-to-end checks of autter on Windows, run by .github/workflows/windows.yml
# under Windows PowerShell 5.1. Pure ASCII; no PowerShell 7 syntax.
#
#   powershell -NoProfile -File scripts/windows-e2e.ps1 -Autter target\debug\autter.exe
#
# Everything runs in a throwaway profile directory: USERPROFILE, HOME,
# APPDATA and LOCALAPPDATA point under a temporary folder, so the global git
# config, ~/.autter and the background service's pipes are this run's own.
# It changes the current user's console settings (HKCU\Console\ForceV2) for
# one check and restores them, so only run it on a throwaway machine such as
# a CI runner.
#
# Sections (each reports PASS/FAIL lines; the script exits 1 on any FAIL):
#   service   bg start / bg status
#   capture   install --system, a checkpointed AI edit committed through
#             git's trace2 pipe, blame --json shows the line as AI
#   doctor    doctor --json and doctor --report
#   lifecycle bg shutdown, start, restart; then the service process killed
#             with Stop-Process -Force and started again over its stale lock
#   vscode    Code.exe ahead of bin\code.cmd on PATH: doctor and
#             install-hooks --dry-run run code.cmd and never Code.exe; a
#             Code.exe with no wrapper is asked --version once and no more
#   npm       the npm launcher against an empty autter.exe explains itself
#   ansi      redirected output, and a real console with and without VT
#             support, hold no ESC characters
#   elevated  a service started elevated, seen from a standard-user token

param(
    [Parameter(Mandatory = $true)][string]$Autter,
    [string]$RepoRoot = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

if (-not $RepoRoot) { $RepoRoot = Split-Path -Parent $PSScriptRoot }
$Autter = (Resolve-Path -LiteralPath $Autter).Path

Write-Host ("PowerShell {0} ({1}) on {2}" -f $PSVersionTable.PSVersion, $PSVersionTable.PSEdition, [Environment]::OSVersion.VersionString)

# --- harness ----------------------------------------------------------------

$script:Failures = New-Object System.Collections.Generic.List[string]
$script:Passes = 0
$script:Notes = New-Object System.Collections.Generic.List[string]
$script:Section = ''
$script:LogIndex = 0

function Pass([string]$msg) {
    $script:Passes++
    Write-Host ("PASS [{0}] {1}" -f $script:Section, $msg)
}
function Fail([string]$msg) {
    $line = "[{0}] {1}" -f $script:Section, $msg
    $script:Failures.Add($line)
    Write-Host ("FAIL " + $line) -ForegroundColor Red
}
function Note([string]$msg) {
    $line = "[{0}] {1}" -f $script:Section, $msg
    $script:Notes.Add($line)
    Write-Host ("NOTE " + $line) -ForegroundColor Yellow
}
function Check([bool]$cond, [string]$msg, [string]$detail = '') {
    if ($cond) { Pass $msg } else {
        if ($detail) { Fail ($msg + "`n      " + ($detail -replace "`n", "`n      ")) } else { Fail $msg }
    }
}
function Begin-Section([string]$name) {
    $script:Section = $name
    Write-Host ''
    Write-Host ("=== {0} ===" -f $name)
}

# A process runner that reads stdout/stderr on their own threads and never
# waits for EOF after the process exits: a detached background service that
# inherited the pipes would otherwise hang the caller. StreamsHeld reports
# exactly that case.
Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;

public class E2eResult {
    public int ExitCode;
    public string Stdout;
    public string Stderr;
    public bool TimedOut;
    public bool StreamsHeld;
    public double Seconds;
}

public static class E2eRunner {
    static void Pump(StreamReader reader, StringBuilder sb) {
        char[] buf = new char[4096];
        try {
            int n;
            while ((n = reader.Read(buf, 0, buf.Length)) > 0) {
                lock (sb) { sb.Append(buf, 0, n); }
            }
        } catch (Exception) { }
    }

    public static string Quote(string arg) {
        if (arg.Length > 0 && arg.IndexOfAny(new char[] { ' ', '\t', '"' }) < 0) { return arg; }
        StringBuilder sb = new StringBuilder();
        sb.Append('"');
        int backslashes = 0;
        foreach (char c in arg) {
            if (c == '\\') { backslashes++; continue; }
            if (c == '"') { sb.Append('\\', backslashes * 2 + 1); sb.Append('"'); backslashes = 0; continue; }
            sb.Append('\\', backslashes); backslashes = 0; sb.Append(c);
        }
        sb.Append('\\', backslashes * 2);
        sb.Append('"');
        return sb.ToString();
    }

    public static E2eResult Run(string file, string[] args, string cwd, int timeoutMs) {
        ProcessStartInfo psi = new ProcessStartInfo(file);
        string[] quoted = new string[args.Length];
        for (int i = 0; i < args.Length; i++) { quoted[i] = Quote(args[i]); }
        psi.Arguments = String.Join(" ", quoted);
        psi.WorkingDirectory = cwd;
        psi.UseShellExecute = false;
        psi.CreateNoWindow = true;
        psi.RedirectStandardInput = true;
        psi.RedirectStandardOutput = true;
        psi.RedirectStandardError = true;
        psi.StandardOutputEncoding = new UTF8Encoding(false);
        psi.StandardErrorEncoding = new UTF8Encoding(false);
        Stopwatch sw = Stopwatch.StartNew();
        Process p = Process.Start(psi);
        p.StandardInput.Close();
        StringBuilder outSb = new StringBuilder();
        StringBuilder errSb = new StringBuilder();
        Thread tOut = new Thread(delegate () { Pump(p.StandardOutput, outSb); });
        Thread tErr = new Thread(delegate () { Pump(p.StandardError, errSb); });
        tOut.IsBackground = true; tErr.IsBackground = true;
        tOut.Start(); tErr.Start();
        E2eResult r = new E2eResult();
        if (!p.WaitForExit(timeoutMs)) {
            r.TimedOut = true;
            try { p.Kill(); } catch (Exception) { }
            p.WaitForExit(5000);
        }
        r.Seconds = sw.Elapsed.TotalSeconds;
        bool joined = tOut.Join(3000) & tErr.Join(3000);
        r.StreamsHeld = !joined;
        r.ExitCode = r.TimedOut ? -1 : p.ExitCode;
        lock (outSb) { r.Stdout = outSb.ToString(); }
        lock (errSb) { r.Stderr = errSb.ToString(); }
        return r;
    }
}
'@

function Invoke-Exe {
    param(
        [string]$File,
        [string[]]$Arguments = @(),
        [string]$Cwd = '',
        [int]$TimeoutSec = 120,
        [string]$Label = ''
    )
    if (-not $Cwd) { $Cwd = $script:WorkDir }
    $r = [E2eRunner]::Run($File, [string[]]$Arguments, $Cwd, $TimeoutSec * 1000)
    $script:LogIndex++
    if (-not $Label) { $Label = ((@([IO.Path]::GetFileNameWithoutExtension($File)) + $Arguments) -join ' ') }
    $safe = ($Label -replace '[^A-Za-z0-9._-]+', '_')
    if ($safe.Length -gt 60) { $safe = $safe.Substring(0, 60) }
    $log = Join-Path $script:LogDir ("{0:D3}-{1}.txt" -f $script:LogIndex, $safe)
    $text = "cmd: {0} {1}`r`ncwd: {2}`r`nexit: {3} timed_out: {4} streams_held: {5} seconds: {6:N1}`r`n--- stdout`r`n{7}`r`n--- stderr`r`n{8}`r`n" -f `
        $File, ($Arguments -join ' '), $Cwd, $r.ExitCode, $r.TimedOut, $r.StreamsHeld, $r.Seconds, $r.Stdout, $r.Stderr
    [IO.File]::WriteAllText($log, $text, (New-Object Text.UTF8Encoding($false)))
    Write-Host ("  > {0} -> exit {1} in {2:N1}s{3}{4}" -f $Label, $r.ExitCode, $r.Seconds, $(if ($r.TimedOut) { ' TIMED OUT' } else { '' }), $(if ($r.StreamsHeld) { ' (output pipes held open after exit)' } else { '' }))
    return $r
}

function Invoke-Autter {
    param([string[]]$Arguments, [string]$Cwd = '', [int]$TimeoutSec = 120)
    return Invoke-Exe -File $script:AutterExe -Arguments $Arguments -Cwd $Cwd -TimeoutSec $TimeoutSec -Label ('autter ' + ($Arguments -join ' '))
}

function Invoke-Git {
    param([string[]]$Arguments, [string]$Cwd = '')
    $r = Invoke-Exe -File $script:GitExe -Arguments $Arguments -Cwd $Cwd -TimeoutSec 120 -Label ('git ' + ($Arguments -join ' '))
    if ($r.ExitCode -ne 0) { throw ("git {0} failed ({1}): {2}" -f ($Arguments -join ' '), $r.ExitCode, $r.Stderr) }
    return $r
}

function Show-Output($r) {
    $all = ($r.Stdout + $r.Stderr).Trim()
    if ($all.Length -gt 3000) { $all = $all.Substring(0, 3000) + ' ...' }
    return $all
}

function ConvertFrom-JsonText([string]$text) {
    # doctor --json prints one line; bg status prints pretty JSON. Take the
    # first {...} document in the text.
    $start = $text.IndexOf('{')
    if ($start -lt 0) { throw "no JSON object in output" }
    return ($text.Substring($start) | ConvertFrom-Json)
}

function Get-ServicePid {
    $r = Invoke-Autter -Arguments @('bg', 'status')
    try {
        $j = ConvertFrom-JsonText $r.Stdout
        return $j.health.pid
    } catch { return $null }
}

function Wait-Until([scriptblock]$Condition, [int]$TimeoutSec = 30) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (& $Condition) { return $true }
        Start-Sleep -Milliseconds 500
    }
    return (& $Condition)
}

# --- isolated profile -------------------------------------------------------

$base = $env:RUNNER_TEMP
if (-not $base) { $base = [IO.Path]::GetTempPath() }
$script:Root = Join-Path $base 'autter-e2e'
if (Test-Path -LiteralPath $script:Root) { Remove-Item -LiteralPath $script:Root -Recurse -Force }
$script:LogDir = Join-Path $script:Root 'logs'
$ProfileDir = Join-Path $script:Root 'profile'
$script:WorkDir = Join-Path $script:Root 'work'
foreach ($d in @($script:LogDir, $ProfileDir, $script:WorkDir,
        (Join-Path $ProfileDir 'AppData\Roaming'), (Join-Path $ProfileDir 'AppData\Local'))) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}

$OriginalUserProfile = $env:USERPROFILE
$env:USERPROFILE = $ProfileDir
$env:HOME = $ProfileDir
$env:HOMEDRIVE = ''
$env:HOMEPATH = ''
$env:APPDATA = Join-Path $ProfileDir 'AppData\Roaming'
$env:LOCALAPPDATA = Join-Path $ProfileDir 'AppData\Local'
$env:AUTTER_NO_INSTALL_PING = '1'
$env:AUTTER_DISABLE_AUTO_UPDATES = '1'
$env:AUTTER_DEBUG = '0'
$env:GIT_TERMINAL_PROMPT = '0'

# The binary where a real install puts it, first on PATH.
$BinDir = Join-Path $ProfileDir '.autter\bin'
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$script:AutterExe = Join-Path $BinDir 'autter.exe'
Copy-Item -LiteralPath $Autter -Destination $script:AutterExe

# PATH without any editor command line (a VS Code, Cursor or Windsurf on the
# runner would be asked to install extensions).
$editorNames = @('code.cmd', 'code.exe', 'cursor.cmd', 'cursor.exe', 'windsurf.cmd', 'windsurf.exe', 'code-insiders.cmd')
$kept = @()
foreach ($dir in @($env:Path -split ';')) {
    if (-not $dir) { continue }
    $hasEditor = $false
    foreach ($n in $editorNames) {
        if (Test-Path -LiteralPath (Join-Path $dir $n)) { $hasEditor = $true }
    }
    if ($hasEditor) { Write-Host ("Dropping editor folder from PATH: {0}" -f $dir); continue }
    $kept += $dir
}
$script:BasePath = (@($BinDir) + $kept) -join ';'
$env:Path = $script:BasePath

$script:GitExe = (Get-Command git -CommandType Application | Select-Object -First 1).Source
Write-Host ("git: {0}" -f $script:GitExe)
Invoke-Git -Arguments @('--version') | Out-Null
Invoke-Git -Arguments @('config', '--global', 'user.name', 'autter e2e') | Out-Null
Invoke-Git -Arguments @('config', '--global', 'user.email', 'e2e@example.invalid') | Out-Null
Invoke-Git -Arguments @('config', '--global', 'init.defaultBranch', 'main') | Out-Null

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$IsElevated = (New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host ("user: {0} elevated: {1}" -f $identity.Name, $IsElevated)

$ver = Invoke-Autter -Arguments @('--version')
Write-Host ("autter --version: {0}" -f $ver.Stdout.Trim())

$RepoDir = Join-Path $script:Root 'repo'

# --- service ----------------------------------------------------------------

Begin-Section 'service'
try {
    $r = Invoke-Autter -Arguments @('bg', 'start') -TimeoutSec 90
    Check ($r.ExitCode -eq 0) ("bg start exits 0 ({0:N1}s)" -f $r.Seconds) (Show-Output $r)
    Check (-not $r.StreamsHeld) 'bg start does not leave its caller''s output pipes open' 'the detached service inherited the stdout/stderr pipes of `autter bg start`, so a caller reading them to the end waits for the service to exit'
    $r = Invoke-Autter -Arguments @('bg', 'status')
    $status = $null
    try { $status = ConvertFrom-JsonText $r.Stdout } catch { }
    Check ($r.ExitCode -eq 0 -and $null -ne $status -and $status.health.state -eq 'healthy') 'bg status answers and reports healthy' (Show-Output $r)
    if ($status) { Write-Host ("  service pid {0}" -f $status.health.pid) }
} catch { Fail ("section error: " + $_.Exception.Message) }

# --- capture ----------------------------------------------------------------

Begin-Section 'capture'
try {
    # What onboarding does once the user agrees: hooks, trace2, service.
    $r = Invoke-Autter -Arguments @('install', '--system', '--yes') -TimeoutSec 180
    Check ($r.ExitCode -eq 0) 'install --system --yes succeeds (trace2 written, capture verified)' (Show-Output $r)
    $target = (Invoke-Git -Arguments @('config', '--global', '--get', 'trace2.eventTarget')).Stdout.Trim()
    Write-Host ("  trace2.eventTarget = {0}" -f $target)
    Check ($target -like '\\.\pipe\autter-*-trace2') 'global trace2.eventTarget is the service''s named pipe' $target

    New-Item -ItemType Directory -Force -Path $RepoDir | Out-Null
    Invoke-Git -Arguments @('init') -Cwd $RepoDir | Out-Null
    $file = Join-Path $RepoDir 'app.txt'
    [IO.File]::WriteAllText($file, "human line`n")
    Invoke-Git -Arguments @('add', '.') -Cwd $RepoDir | Out-Null
    Invoke-Git -Arguments @('commit', '-m', 'initial') -Cwd $RepoDir | Out-Null

    [IO.File]::WriteAllText($file, "human line`nai line`n")
    $r = Invoke-Autter -Arguments @('checkpoint', 'mock_ai', 'app.txt') -Cwd $RepoDir
    Check ($r.ExitCode -eq 0) 'checkpoint mock_ai app.txt exits 0' (Show-Output $r)
    Invoke-Git -Arguments @('add', 'app.txt') -Cwd $RepoDir | Out-Null
    Invoke-Git -Arguments @('commit', '-m', 'ai edit') -Cwd $RepoDir | Out-Null

    $noted = Wait-Until -TimeoutSec 60 -Condition {
        $n = [E2eRunner]::Run($script:GitExe, [string[]]@('notes', '--ref=ai', 'list', 'HEAD'), $RepoDir, 30000)
        return ($n.ExitCode -eq 0 -and $n.Stdout.Trim().Length -gt 0)
    }
    Check $noted 'the commit gets an authorship note in refs/notes/ai (via the trace2 pipe)'

    $r = Invoke-Autter -Arguments @('blame', '--json', 'app.txt') -Cwd $RepoDir
    $blame = $null
    try { $blame = ConvertFrom-JsonText $r.Stdout } catch { }
    $cats = $null
    if ($blame -and ($blame.PSObject.Properties.Name -contains 'line_categories')) { $cats = $blame.line_categories }
    $line2 = $null
    if ($cats) {
        foreach ($p in $cats.PSObject.Properties) {
            $range = $p.Name -split '-'
            $lo = [int]$range[0]; $hi = [int]$range[-1]
            if (2 -ge $lo -and 2 -le $hi) { $line2 = $p.Value }
        }
    }
    Check ($line2 -eq 'ai') 'blame --json shows line 2 as ai' (Show-Output $r)
} catch { Fail ("section error: " + $_.Exception.Message) }

# --- doctor -----------------------------------------------------------------

Begin-Section 'doctor'
try {
    $r = Invoke-Autter -Arguments @('doctor', '--json') -Cwd $RepoDir -TimeoutSec 240
    $doc = $null
    try { $doc = ConvertFrom-JsonText $r.Stdout } catch { }
    Check ($null -ne $doc) 'doctor --json prints a JSON document' (Show-Output $r)
    if ($doc) {
        foreach ($c in $doc.checks) {
            $extra = ''
            if ($c.status -ne 'passed') {
                $d = @()
                if ($c.PSObject.Properties.Name -contains 'details') { $d = @($c.details) }
                $fix = ''
                if ($c.PSObject.Properties.Name -contains 'remediation') { $fix = $c.remediation }
                $extra = (" | " + ($d -join ' / ') + $(if ($fix) { " | fix: $fix" } else { '' }))
            }
            Write-Host ("  {0,-8} {1} / {2}: {3}{4}" -f $c.status, $c.section, $c.name, $c.summary, $extra)
        }
        $capture = @($doc.checks | Where-Object { @('Background service', 'Git capture (trace2)', 'End-to-end checkpoint') -contains $_.section })
        Check ($capture.Count -gt 0) 'doctor ran the service, trace2 and end-to-end checks'
        $bad = @($capture | Where-Object { $_.status -ne 'passed' -and $_.status -ne 'skipped' })
        Check ($bad.Count -eq 0) 'service, trace2 ingestion and end-to-end checkpoint checks pass' (($bad | ForEach-Object { "{0}: {1} {2}" -f $_.name, $_.status, $_.summary }) -join "`n")
        Check ([bool]$doc.capture_ok) 'doctor reports capture_ok'
        Check (-not ($r.Stdout -match 'predates native agent hooks')) 'doctor does not call VS Code too old'
    }

    $report = Join-Path $script:Root 'autter-report.txt'
    $r = Invoke-Autter -Arguments @('doctor', '--report', $report) -Cwd $RepoDir -TimeoutSec 240
    Check (Test-Path -LiteralPath $report) 'doctor --report writes the report file' (Show-Output $r)
    if (Test-Path -LiteralPath $report) {
        $text = [IO.File]::ReadAllText($report)
        Copy-Item -LiteralPath $report -Destination (Join-Path $script:LogDir 'autter-report.txt')
        foreach ($h in @('autter doctor report', '== system', '== doctor results', '== background service', '== lock file')) {
            Check ($text.Contains($h)) ("report has '{0}'" -f $h)
        }
        Check ($text -match 'state: Healthy') 'report says the service is Healthy' ($text.Substring(0, [Math]::Min(2000, $text.Length)))
    }
} catch { Fail ("section error: " + $_.Exception.Message) }

# --- lifecycle --------------------------------------------------------------

Begin-Section 'lifecycle'
try {
    $pid1 = Get-ServicePid
    $r = Invoke-Autter -Arguments @('bg', 'shutdown')
    Check ($r.ExitCode -eq 0) 'bg shutdown exits 0' (Show-Output $r)
    $down = Wait-Until -TimeoutSec 20 -Condition {
        $s = [E2eRunner]::Run($script:AutterExe, [string[]]@('bg', 'status'), $script:WorkDir, 30000)
        return ($s.ExitCode -ne 0)
    }
    Check $down 'bg status reports the service down after shutdown'
    if ($pid1) {
        $gone = Wait-Until -TimeoutSec 20 -Condition { $null -eq (Get-Process -Id $pid1 -ErrorAction SilentlyContinue) }
        Check $gone ("service process {0} exits after shutdown" -f $pid1)
    }

    $r = Invoke-Autter -Arguments @('bg', 'start') -TimeoutSec 90
    Check ($r.ExitCode -eq 0) ("bg start after shutdown exits 0 ({0:N1}s)" -f $r.Seconds) (Show-Output $r)
    $r = Invoke-Autter -Arguments @('bg', 'restart') -TimeoutSec 120
    Check ($r.ExitCode -eq 0) ("bg restart exits 0 ({0:N1}s)" -f $r.Seconds) (Show-Output $r)
    Check (-not ($r.Stderr -match 'ERROR: The process')) 'bg restart prints no raw taskkill error' $r.Stderr
    $r = Invoke-Autter -Arguments @('bg', 'status')
    Check ($r.ExitCode -eq 0) 'bg status healthy after restart' (Show-Output $r)

    # Stale lock: the service dies without cleaning up.
    $pid2 = Get-ServicePid
    Check ($null -ne $pid2) 'bg status names the service pid'
    if ($pid2) {
        Stop-Process -Id $pid2 -Force
        $gone = Wait-Until -TimeoutSec 20 -Condition { $null -eq (Get-Process -Id $pid2 -ErrorAction SilentlyContinue) }
        Check $gone ("killed service process {0} is gone" -f $pid2)
        $lockPath = Join-Path $ProfileDir '.autter\internal\daemon\daemon.lock'
        Write-Host ("  lock file left behind: {0}" -f (Test-Path -LiteralPath $lockPath))
        $r = Invoke-Autter -Arguments @('bg', 'status')
        Check ($r.ExitCode -ne 0) 'bg status reports a killed service as not healthy' (Show-Output $r)
        $r = Invoke-Autter -Arguments @('bg', 'start') -TimeoutSec 90
        Check ($r.ExitCode -eq 0) ("bg start takes over the stale lock ({0:N1}s)" -f $r.Seconds) (Show-Output $r)
        $pid3 = Get-ServicePid
        Check ($null -ne $pid3 -and $pid3 -ne $pid2) ("a new service process runs (pid {0})" -f $pid3)

        # Capture still works after the takeover.
        $file = Join-Path $RepoDir 'app.txt'
        [IO.File]::WriteAllText($file, "human line`nai line`nsecond ai line`n")
        $r = Invoke-Autter -Arguments @('checkpoint', 'mock_ai', 'app.txt') -Cwd $RepoDir
        Check ($r.ExitCode -eq 0) 'checkpoint after takeover exits 0' (Show-Output $r)
        Invoke-Git -Arguments @('commit', '-am', 'second ai edit') -Cwd $RepoDir | Out-Null
        $noted = Wait-Until -TimeoutSec 60 -Condition {
            $n = [E2eRunner]::Run($script:GitExe, [string[]]@('notes', '--ref=ai', 'list', 'HEAD'), $RepoDir, 30000)
            return ($n.ExitCode -eq 0 -and $n.Stdout.Trim().Length -gt 0)
        }
        Check $noted 'a commit after the takeover gets an authorship note'
    }
} catch { Fail ("section error: " + $_.Exception.Message) }

# --- vscode -----------------------------------------------------------------

Begin-Section 'vscode'
try {
    # A console program that logs its arguments next to itself and prints
    # nothing, like the editor's window executable (Code.exe) does.
    $stubSrc = @'
using System;
using System.IO;
public static class CodeExeStub {
    public static int Main(string[] args) {
        string log = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "Code.exe.log");
        File.AppendAllText(log, String.Join(" ", args) + Environment.NewLine);
        return 0;
    }
}
'@
    $stubBuild = Join-Path $script:Root 'stub'
    New-Item -ItemType Directory -Force -Path $stubBuild | Out-Null
    $stubExe = Join-Path $stubBuild 'Code.exe'
    Add-Type -TypeDefinition $stubSrc -OutputAssembly $stubExe -OutputType ConsoleApplication

    # 1. The reported PATH order: the install folder (Code.exe) before its
    #    bin folder (code.cmd). The folder name has spaces, like the real one.
    $vs = Join-Path $script:Root 'editors\Microsoft VS Code'
    New-Item -ItemType Directory -Force -Path (Join-Path $vs 'bin') | Out-Null
    Copy-Item -LiteralPath $stubExe -Destination (Join-Path $vs 'Code.exe')
    $cmdLog = Join-Path $vs 'code.cmd.log'
    $cmdStub = @(
        '@echo off',
        ('>>"{0}" echo %*' -f $cmdLog),
        'if "%~1"=="--version" (',
        '  echo 1.110.0',
        '  echo 0000000000000000000000000000000000000000',
        '  echo x64',
        ')',
        'exit /b 0'
    ) -join "`r`n"
    [IO.File]::WriteAllText((Join-Path $vs 'bin\code.cmd'), $cmdStub + "`r`n", [Text.Encoding]::ASCII)

    $env:Path = ($vs + ';' + (Join-Path $vs 'bin') + ';' + $script:BasePath)
    $exeLog = Join-Path $vs 'Code.exe.log'
    $r1 = Invoke-Autter -Arguments @('doctor', '--json') -Cwd $RepoDir -TimeoutSec 240
    $r2 = Invoke-Autter -Arguments @('install-hooks', '--dry-run') -Cwd $RepoDir -TimeoutSec 120
    $exeCalls = @()
    if (Test-Path -LiteralPath $exeLog) { $exeCalls = @(Get-Content -LiteralPath $exeLog) }
    $cmdCalls = @()
    if (Test-Path -LiteralPath $cmdLog) { $cmdCalls = @(Get-Content -LiteralPath $cmdLog) }
    Write-Host ("  Code.exe calls: {0}; code.cmd calls: {1}" -f $exeCalls.Count, $cmdCalls.Count)
    foreach ($c in $cmdCalls) { Write-Host ("    code.cmd {0}" -f $c) }
    Check ($exeCalls.Count -eq 0) 'Code.exe (the window) was never run' ($exeCalls -join "`n")
    Check (@($cmdCalls | Where-Object { $_ -match '--version' }).Count -gt 0) 'bin\code.cmd was asked --version'
    Check (@($cmdCalls | Where-Object { $_ -match '--list-extensions' }).Count -gt 0) 'bin\code.cmd was asked --list-extensions'
    Check (-not (($r1.Stdout + $r1.Stderr) -match 'predates native agent hooks')) 'doctor does not call VS Code 1.110 too old' (Show-Output $r1)
    Check ($r2.ExitCode -eq 0) 'install-hooks --dry-run exits 0' (Show-Output $r2)
    Check (($r2.Stdout + $r2.Stderr) -match 'VS Code') 'install-hooks --dry-run reports on VS Code' (Show-Output $r2)

    # 2. Only the window executable on PATH, no wrapper beside it: asked
    #    --version, prints nothing, and is not run again.
    $lone = Join-Path $script:Root 'editors\lone'
    New-Item -ItemType Directory -Force -Path $lone | Out-Null
    Copy-Item -LiteralPath $stubExe -Destination (Join-Path $lone 'Code.exe')
    $loneLog = Join-Path $lone 'Code.exe.log'
    $env:Path = ($lone + ';' + $script:BasePath)
    $r3 = Invoke-Autter -Arguments @('install-hooks', '--dry-run') -Cwd $RepoDir -TimeoutSec 120
    $loneCalls = @()
    if (Test-Path -LiteralPath $loneLog) { $loneCalls = @(Get-Content -LiteralPath $loneLog) }
    Write-Host ("  lone Code.exe calls: {0}" -f (($loneCalls | ForEach-Object { "[$_]" }) -join ' '))
    Check (@($loneCalls | Where-Object { $_ -notmatch '^--version' }).Count -eq 0) 'a lone Code.exe is only ever asked --version' ($loneCalls -join "`n")
    Check ($loneCalls.Count -le 1) ("a lone Code.exe is run at most once per autter command ({0})" -f $loneCalls.Count) ($loneCalls -join "`n")
    Check (($r3.Stdout + $r3.Stderr) -match 'printed nothing') 'install-hooks says the silent --version is not the command line' (Show-Output $r3)
} catch { Fail ("section error: " + $_.Exception.Message) }
$env:Path = $script:BasePath

# --- npm --------------------------------------------------------------------

Begin-Section 'npm'
try {
    $node = Get-Command node -CommandType Application -ErrorAction SilentlyContinue
    if (-not $node) { Fail 'node is not on PATH' } else {
        $npmHome = Join-Path $script:Root 'npm-profile'
        $npmBin = Join-Path $npmHome '.autter\bin'
        New-Item -ItemType Directory -Force -Path $npmBin | Out-Null
        [IO.File]::WriteAllBytes((Join-Path $npmBin 'autter.exe'), [byte[]]@())
        $saved = $env:USERPROFILE
        $env:USERPROFILE = $npmHome
        try {
            $launcher = Join-Path $RepoRoot 'npm\bin\autter.js'
            $r = Invoke-Exe -File $node.Source -Arguments @($launcher, '--version') -Label 'node npm/bin/autter.js --version (empty autter.exe)'
        } finally { $env:USERPROFILE = $saved }
        $err = $r.Stderr
        Write-Host ("  launcher said:`n    " + ($err.Trim() -replace "`n", "`n    "))
        Check ($r.ExitCode -ne 0) 'launcher exits non-zero'
        Check ($err -match 'failed to run') 'launcher names the failed run'
        Check ($err -match 'Windows Security' -and $err -match 'install\.ps1') 'launcher adds spawnFailureHelp (cause, antivirus step, reinstall)' $err
    }
} catch { Fail ("section error: " + $_.Exception.Message) }

# --- ansi -------------------------------------------------------------------

Begin-Section 'ansi'
$savedCI = $env:CI
try {
    # CI=true turns color off by itself; take it away so the console
    # detection is what decides.
    Remove-Item Env:CI -ErrorAction SilentlyContinue
    $esc = [string][char]27
    foreach ($args1 in @(@('doctor'), @('onboard'), @('bg', 'status'), @('install-hooks', '--dry-run'), @('--help'))) {
        $r = Invoke-Autter -Arguments $args1 -Cwd $RepoDir -TimeoutSec 240
        $all = $r.Stdout + $r.Stderr
        Check (-not $all.Contains($esc)) ("redirected 'autter {0}' has no ESC characters" -f ($args1 -join ' ')) (Show-Output $r)
    }

    # A real console window: the probe runs autter with the console as its
    # output and copies the screen buffer to a file. Legacy console
    # (ForceV2=0) has no VT support; the default one has.
    $probe = Join-Path $script:Root 'console-probe.ps1'
    $probeSrc = @'
param([string]$Autter, [string]$Repo, [string]$OutDir)
$ErrorActionPreference = 'Continue'
Remove-Item Env:CI -ErrorAction SilentlyContinue
Remove-Item Env:TERM -ErrorAction SilentlyContinue
Remove-Item Env:WT_SESSION -ErrorAction SilentlyContinue
Remove-Item Env:TERM_PROGRAM -ErrorAction SilentlyContinue
Remove-Item Env:COLORTERM -ErrorAction SilentlyContinue
$raw = $Host.UI.RawUI
try { $raw.BufferSize = New-Object System.Management.Automation.Host.Size(160, 3000) } catch { }
function Save-Screen([string]$name) {
    $w = $raw.BufferSize.Width
    $h = [Math]::Min($raw.BufferSize.Height - 1, $raw.CursorPosition.Y + 2)
    $rect = New-Object System.Management.Automation.Host.Rectangle(0, 0, ($w - 1), $h)
    $cells = $raw.GetBufferContents($rect)
    $sb = New-Object System.Text.StringBuilder
    for ($y = 0; $y -le $h; $y++) {
        $line = New-Object System.Text.StringBuilder
        for ($x = 0; $x -lt $w; $x++) { [void]$line.Append($cells[$y, $x].Character) }
        [void]$sb.AppendLine($line.ToString().TrimEnd())
    }
    [IO.File]::WriteAllText((Join-Path $OutDir $name), $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
}
Set-Location -LiteralPath $Repo
Clear-Host
& $Autter doctor
Save-Screen 'doctor.txt'
Clear-Host
$p = Start-Process -FilePath $Autter -ArgumentList 'onboard' -NoNewWindow -PassThru
Start-Sleep -Seconds 12
Save-Screen 'onboard.txt'
try { $p.Kill() } catch { }
Start-Sleep -Seconds 1
Clear-Host
[Console]::Out.Write([string][char]27 + '[31mVTCHECK' + [string][char]27 + '[0m')
[Console]::Out.WriteLine()
Save-Screen 'vtcheck.txt'
[IO.File]::WriteAllText((Join-Path $OutDir 'done'), 'done')
'@
    [IO.File]::WriteAllText($probe, $probeSrc, [Text.Encoding]::ASCII)

    $consoleKey = 'HKCU:\Console'
    $hadForceV2 = $false
    $oldForceV2 = $null
    try {
        $prop = Get-ItemProperty -Path $consoleKey -Name ForceV2 -ErrorAction Stop
        $hadForceV2 = $true
        $oldForceV2 = $prop.ForceV2
    } catch { }

    foreach ($mode in @('legacy', 'default')) {
        if ($mode -eq 'legacy') {
            if (-not (Test-Path $consoleKey)) { New-Item -Path $consoleKey | Out-Null }
            New-ItemProperty -Path $consoleKey -Name ForceV2 -Value 0 -PropertyType DWord -Force | Out-Null
        } else {
            if ($hadForceV2) { New-ItemProperty -Path $consoleKey -Name ForceV2 -Value $oldForceV2 -PropertyType DWord -Force | Out-Null }
            else { Remove-ItemProperty -Path $consoleKey -Name ForceV2 -ErrorAction SilentlyContinue }
        }
        $out = Join-Path $script:LogDir ("console-" + $mode)
        New-Item -ItemType Directory -Force -Path $out | Out-Null
        $proc = Start-Process -FilePath 'powershell.exe' -PassThru -WindowStyle Hidden -ArgumentList @(
            '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', ('"' + $probe + '"'),
            '-Autter', ('"' + $script:AutterExe + '"'), '-Repo', ('"' + $RepoDir + '"'), '-OutDir', ('"' + $out + '"'))
        if (-not $proc.WaitForExit(300000)) { try { $proc.Kill() } catch { }; Fail ("{0} console probe timed out" -f $mode); continue }
        if (-not (Test-Path -LiteralPath (Join-Path $out 'done'))) { Fail ("{0} console probe did not finish (exit {1})" -f $mode, $proc.ExitCode); continue }
        $vt = [IO.File]::ReadAllText((Join-Path $out 'vtcheck.txt'))
        $vtOn = -not $vt.Contains($esc)
        Write-Host ("  {0} console: VT processing {1}" -f $mode, $(if ($vtOn) { 'on' } else { 'off (raw ESC shows as text)' }))
        if ($mode -eq 'legacy' -and $vtOn) { Note 'the legacy console (ForceV2=0) is not available on this runner; the no-VT console case ran in the default console only' }
        foreach ($name in @('doctor.txt', 'onboard.txt')) {
            $screen = [IO.File]::ReadAllText((Join-Path $out $name))
            Write-Host ("  --- {0} console, {1}:" -f $mode, $name)
            foreach ($l in @($screen -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -First 40)) { Write-Host ("    |" + $l) }
            Check ($screen.Trim().Length -gt 0) ("{0} console: {1} shows output" -f $mode, $name)
            Check (-not $screen.Contains($esc)) ("{0} console (VT {1}): {2} has no raw ESC characters" -f $mode, $(if ($vtOn) { 'on' } else { 'off' }), $name)
            Check (-not $screen.Contains([string][char]0xFFFD)) ("{0} console: {1} has no replacement characters" -f $mode, $name)
        }
        $onb = [IO.File]::ReadAllText((Join-Path $out 'onboard.txt'))
        Check ($onb -match '(?i)local') ("{0} console: the onboarding selection is on screen" -f $mode)
    }
    if ($hadForceV2) { New-ItemProperty -Path $consoleKey -Name ForceV2 -Value $oldForceV2 -PropertyType DWord -Force | Out-Null }
    else { Remove-ItemProperty -Path $consoleKey -Name ForceV2 -ErrorAction SilentlyContinue }
} catch { Fail ("section error: " + $_.Exception.Message) }
finally { if ($null -ne $savedCI) { $env:CI = $savedCI } }

# --- elevated ---------------------------------------------------------------

Begin-Section 'elevated'
try {
    if (-not $IsElevated) {
        Note 'this shell is not elevated, so the elevated-service case cannot be set up here'
    } else {
        # The service is running from this (elevated) shell. Ask it things
        # from a Basic User token (runas /trustlevel:0x20000), the way a
        # normal window would after an install from an Administrator one.
        $r = Invoke-Autter -Arguments @('bg', 'status')
        Check ($r.ExitCode -eq 0) 'elevated service is running'
        $elevDir = Join-Path $script:Root 'elevated'
        New-Item -ItemType Directory -Force -Path $elevDir | Out-Null
        $wrapper = Join-Path $elevDir 'as-standard-user.cmd'
        $lines = @(
            '@echo off',
            ('set "USERPROFILE={0}"' -f $ProfileDir),
            ('set "HOME={0}"' -f $ProfileDir),
            ('set "APPDATA={0}"' -f $env:APPDATA),
            ('set "LOCALAPPDATA={0}"' -f $env:LOCALAPPDATA),
            'set "HOMEDRIVE="',
            'set "HOMEPATH="',
            'set "CI="',
            'set "AUTTER_NO_INSTALL_PING=1"',
            'set "AUTTER_DISABLE_AUTO_UPDATES=1"',
            'set "AUTTER_DEBUG=0"',
            ('set "PATH={0}"' -f $script:BasePath),
            ('cd /d "{0}"' -f $RepoDir),
            ('whoami /groups > "{0}\whoami.txt" 2>&1' -f $elevDir),
            ('"{0}" bg status > "{1}\status.out" 2> "{1}\status.err"' -f $script:AutterExe, $elevDir),
            ('echo %ERRORLEVEL% > "{0}\status.code"' -f $elevDir),
            ('"{0}" bg start > "{1}\start.out" 2> "{1}\start.err"' -f $script:AutterExe, $elevDir),
            ('echo %ERRORLEVEL% > "{0}\start.code"' -f $elevDir),
            ('"{0}" bg restart > "{1}\restart.out" 2> "{1}\restart.err"' -f $script:AutterExe, $elevDir),
            ('echo %ERRORLEVEL% > "{0}\restart.code"' -f $elevDir),
            ('"{0}" bg shutdown > "{1}\shutdown.out" 2> "{1}\shutdown.err"' -f $script:AutterExe, $elevDir),
            ('echo %ERRORLEVEL% > "{0}\shutdown.code"' -f $elevDir),
            ('"{0}" doctor --json > "{1}\doctor.out" 2> "{1}\doctor.err"' -f $script:AutterExe, $elevDir),
            ('echo %ERRORLEVEL% > "{0}\doctor.code"' -f $elevDir),
            ('echo done > "{0}\done"' -f $elevDir)
        )
        [IO.File]::WriteAllText($wrapper, (($lines -join "`r`n") + "`r`n"), [Text.Encoding]::ASCII)
        $runas = Invoke-Exe -File (Join-Path $env:SystemRoot 'System32\runas.exe') -Arguments @('/trustlevel:0x20000', ('cmd.exe /c "' + $wrapper + '"')) -TimeoutSec 60 -Label 'runas /trustlevel:0x20000'
        $finished = Wait-Until -TimeoutSec 300 -Condition { Test-Path -LiteralPath (Join-Path $elevDir 'done') }
        if (-not $finished) {
            Note ("runas /trustlevel:0x20000 did not run the standard-user commands on this runner (exit {0}): {1}" -f $runas.ExitCode, (Show-Output $runas))
        } else {
            function Read-Part([string]$name) {
                $p = Join-Path $elevDir $name
                if (Test-Path -LiteralPath $p) { return [IO.File]::ReadAllText($p) } else { return '' }
            }
            $groups = Read-Part 'whoami.txt'
            $restricted = ($groups -match 'Medium Mandatory Level') -or ($groups -match 'Group used for deny only')
            Write-Host ("  standard-user token: {0}" -f $(if ($restricted) { 'yes (Administrators deny-only / medium integrity)' } else { 'NOT restricted' }))
            Check $restricted 'runas /trustlevel gives a non-elevated token' $groups
            foreach ($step in @('status', 'start', 'restart', 'shutdown', 'doctor')) {
                $code = (Read-Part ($step + '.code')).Trim()
                $text = (Read-Part ($step + '.out')) + (Read-Part ($step + '.err'))
                [IO.File]::WriteAllText((Join-Path $script:LogDir ("elevated-" + $step + ".txt")), ("exit " + $code + "`r`n" + $text))
                Write-Host ("  standard user: autter {0} -> exit {1}" -f $step, $code)
                foreach ($l in @($text -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -First 12)) { Write-Host ("    |" + $l) }
            }
            $startText = (Read-Part 'start.err') + (Read-Part 'start.out')
            $restartText = (Read-Part 'restart.err') + (Read-Part 'restart.out')
            $shutdownText = (Read-Part 'shutdown.err') + (Read-Part 'shutdown.out')
            $doctorText = (Read-Part 'doctor.out') + (Read-Part 'doctor.err')
            $hint = 'started from an Administrator window'
            Check ($startText.Contains($hint)) 'bg start from a standard user names the elevated service' $startText
            Check ($restartText.Contains($hint)) 'bg restart from a standard user names the elevated service' $restartText
            Check ($shutdownText.Contains($hint)) 'bg shutdown from a standard user names the elevated service' $shutdownText
            Check ($doctorText.Contains($hint)) 'doctor from a standard user names the elevated service' $doctorText
            $all = $startText + $restartText + $shutdownText + $doctorText
            Check (-not ($all -match 'ERROR: The process')) 'no raw taskkill error reaches a standard user' $all
        }
        $r = Invoke-Autter -Arguments @('bg', 'status')
        Check ($r.ExitCode -eq 0) 'the elevated service is still running after the standard-user attempts' (Show-Output $r)
    }
} catch { Fail ("section error: " + $_.Exception.Message) }

# --- teardown ---------------------------------------------------------------

Begin-Section 'teardown'
try {
    $r = Invoke-Autter -Arguments @('bg', 'shutdown')
    $logs = Join-Path $ProfileDir '.autter\internal\daemon\logs'
    if (Test-Path -LiteralPath $logs) { Copy-Item -LiteralPath $logs -Destination (Join-Path $script:LogDir 'service-logs') -Recurse -Force }
} catch { Write-Host ("teardown: " + $_.Exception.Message) }
$env:USERPROFILE = $OriginalUserProfile

Write-Host ''
Write-Host ("{0} passed, {1} failed, {2} notes" -f $script:Passes, $script:Failures.Count, $script:Notes.Count)
foreach ($n in $script:Notes) { Write-Host ("NOTE " + $n) }
foreach ($f in $script:Failures) { Write-Host ("FAIL " + $f) }
if ($script:Failures.Count -gt 0) { exit 1 }
exit 0
