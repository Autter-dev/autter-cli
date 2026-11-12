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
# The test repository, with one commit made before any checkpoint.
New-Item -ItemType Directory -Force -Path $RepoDir | Out-Null
Invoke-Git -Arguments @('init') -Cwd $RepoDir | Out-Null
$file = Join-Path $RepoDir 'app.txt'
[IO.File]::WriteAllText($file, "human line`n")
Invoke-Git -Arguments @('add', '.') -Cwd $RepoDir | Out-Null
Invoke-Git -Arguments @('commit', '-m', 'initial') -Cwd $RepoDir | Out-Null

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
    $r = Invoke-Autter -Arguments @('install', '--system') -TimeoutSec 180
    Check ($r.ExitCode -eq 0) 'install --system succeeds (trace2 written, capture verified)' (Show-Output $r)
    $t = Invoke-Exe -File $script:GitExe -Arguments @('config', '--global', '--get', 'trace2.eventTarget') -Label 'git config --get trace2.eventTarget'
    $target = $t.Stdout.Trim()
    Write-Host ("  trace2.eventTarget = {0}" -f $target)
    Check ($target -like '\\.\pipe\autter-*-trace2') 'global trace2.eventTarget is the service''s named pipe' $target

    $file = Join-Path $RepoDir 'app.txt'
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
    $node = Get-Command node -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
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
    # Start a program in a console of its own (hidden window), the way a
    # user's terminal runs autter, and wait for it.
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class NewConsole {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct SI {
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX; public int dwY; public int dwXSize; public int dwYSize;
        public int dwXCountChars; public int dwYCountChars; public int dwFillAttribute; public int dwFlags;
        public short wShowWindow; public short cbReserved2; public IntPtr lpReserved2;
        public IntPtr hStdInput; public IntPtr hStdOutput; public IntPtr hStdError;
    }
    [StructLayout(LayoutKind.Sequential)]
    public struct PI { public IntPtr hProcess; public IntPtr hThread; public int dwProcessId; public int dwThreadId; }
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcess(string app, string cmd, IntPtr pa, IntPtr ta, bool inherit, uint flags,
        IntPtr env, string cwd, ref SI si, out PI pi);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle, uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr handle, out uint code);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr handle, uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);

    public static int Run(string app, string commandLine, string cwd, int timeoutMs) {
        SI si = new SI();
        si.cb = Marshal.SizeOf(typeof(SI));
        si.dwFlags = 0x1;      // STARTF_USESHOWWINDOW
        si.wShowWindow = 0;    // SW_HIDE
        PI pi;
        // CREATE_NEW_CONSOLE; no handles inherited, so the console is its stdio.
        if (!CreateProcess(app, commandLine, IntPtr.Zero, IntPtr.Zero, false, 0x10, IntPtr.Zero, cwd, ref si, out pi)) {
            throw new Win32Exception();
        }
        uint code = 0;
        if (WaitForSingleObject(pi.hProcess, (uint)timeoutMs) != 0) {
            TerminateProcess(pi.hProcess, 1);
            code = unchecked((uint)-1);
        } else {
            GetExitCodeProcess(pi.hProcess, out code);
        }
        CloseHandle(pi.hThread); CloseHandle(pi.hProcess);
        return unchecked((int)code);
    }
}
'@
    $probe = Join-Path $script:Root 'console-probe.ps1'
    $probeSrc = @'
param([string]$Autter, [string]$Repo, [string]$OutDir)
$ErrorActionPreference = 'Continue'
[IO.File]::WriteAllText((Join-Path $OutDir 'started'), 'started')
trap {
    [IO.File]::AppendAllText((Join-Path $OutDir 'error.txt'), (($_ | Out-String) + "`r`n"))
    continue
}
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
        $psExe = Join-Path $PSHOME 'powershell.exe'
        $probeArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $probe, '-Autter', $script:AutterExe, '-Repo', $RepoDir, '-OutDir', $out)
        $cmdLine = (@($psExe) + $probeArgs | ForEach-Object { [E2eRunner]::Quote($_) }) -join ' '
        $exit = [NewConsole]::Run($psExe, $cmdLine, $RepoDir, 300000)
        $probeErrors = Join-Path $out 'error.txt'
        if (Test-Path -LiteralPath $probeErrors) { Write-Host ("  {0} console probe errors:`n{1}" -f $mode, [IO.File]::ReadAllText($probeErrors)) }
        if (-not (Test-Path -LiteralPath (Join-Path $out 'done'))) {
            Fail ("{0} console probe did not finish (exit {1}, started: {2})" -f $mode, $exit, (Test-Path -LiteralPath (Join-Path $out 'started')))
            # The same probe with redirected output, to see why.
            $diag = Invoke-Exe -File $psExe -Arguments $probeArgs -Cwd $RepoDir -TimeoutSec 120 -Label ('console probe redirected ' + $mode)
            Write-Host ("  redirected probe: exit {0}`n{1}" -f $diag.ExitCode, (Show-Output $diag))
            continue
        }
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
            # UTF-8 read as the OEM (437) or ANSI (1252) code page: an em dash
            # shows as "G-C" or "a-euro" pairs.
            $mojibake = $screen.Contains([string][char]0x393 + [char]0xC7) -or $screen.Contains([string][char]0xE2 + [char]0x20AC)
            Check (-not $mojibake) ("{0} console: {1} has no UTF-8 mojibake" -f $mode, $name)
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
        # The service runs from this elevated shell. A normal window's token
        # is the same user with Administrators turned into a deny-only group,
        # no privileges and medium integrity (what UAC gives). Build that token
        # from this process's own and run autter with it. (runas /trustlevel
        # does not filter the token on a machine with UAC off, such as a CI
        # runner.)
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class FilteredToken {
    [StructLayout(LayoutKind.Sequential)]
    public struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct STARTUPINFO {
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX; public int dwY; public int dwXSize; public int dwYSize;
        public int dwXCountChars; public int dwYCountChars; public int dwFillAttribute; public int dwFlags;
        public short wShowWindow; public short cbReserved2; public IntPtr lpReserved2;
        public IntPtr hStdInput; public IntPtr hStdOutput; public IntPtr hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct PROCESS_INFORMATION { public IntPtr hProcess; public IntPtr hThread; public int dwProcessId; public int dwThreadId; }

    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool CreateRestrictedToken(IntPtr existing, uint flags, uint disableCount, SID_AND_ATTRIBUTES[] disable,
        uint deleteCount, IntPtr delete, uint restrictCount, IntPtr restrict, out IntPtr newToken);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertStringSidToSid(string sid, out IntPtr psid);
    [DllImport("advapi32.dll")] static extern uint GetLengthSid(IntPtr sid);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool SetTokenInformation(IntPtr token, int infoClass, IntPtr info, uint length);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcessAsUser(IntPtr token, string app, string cmd, IntPtr pa, IntPtr ta, bool inherit,
        uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle, uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr handle, out uint code);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr handle, uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr GetProcessWindowStation();
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr GetThreadDesktop(uint threadId);
    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool GetUserObjectInformation(IntPtr obj, int index, System.Text.StringBuilder info, int length, out int needed);
    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr OpenWindowStation(string name, bool inherit, uint access);
    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr OpenDesktop(string name, uint flags, bool inherit, uint access);
    [DllImport("advapi32.dll")]
    static extern uint SetSecurityInfo(IntPtr handle, int objectType, uint securityInfo, IntPtr owner, IntPtr group, IntPtr dacl, IntPtr sacl);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertStringSecurityDescriptorToSecurityDescriptor(string sddl, uint revision, out IntPtr sd, IntPtr size);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetSecurityDescriptorDacl(IntPtr sd, out bool present, out IntPtr dacl, out bool defaulted);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetSecurityDescriptorSacl(IntPtr sd, out bool present, out IntPtr sacl, out bool defaulted);

    // Everyone may use the object, from any integrity level.
    static void OpenUp(IntPtr handle) {
        IntPtr sd;
        if (!ConvertStringSecurityDescriptorToSecurityDescriptor("D:(A;;GA;;;WD)S:(ML;;NW;;;LW)", 1, out sd, IntPtr.Zero)) { throw new Win32Exception(); }
        bool present, defaulted;
        IntPtr dacl, sacl;
        GetSecurityDescriptorDacl(sd, out present, out dacl, out defaulted);
        GetSecurityDescriptorSacl(sd, out present, out sacl, out defaulted);
        // DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION
        uint rc = SetSecurityInfo(handle, 7, 0x4 | 0x10, IntPtr.Zero, IntPtr.Zero, dacl, sacl);
        if (rc != 0) {
            rc = SetSecurityInfo(handle, 7, 0x4, IntPtr.Zero, IntPtr.Zero, dacl, IntPtr.Zero);
            if (rc != 0) { throw new Win32Exception((int)rc); }
        }
    }

    static string ObjectName(IntPtr obj) {
        System.Text.StringBuilder sb = new System.Text.StringBuilder(256);
        int needed;
        if (!GetUserObjectInformation(obj, 2, sb, sb.Capacity * 2, out needed)) { throw new Win32Exception(); }
        return sb.ToString();
    }

    // A process started with the filtered token fails to initialize
    // (STATUS_DLL_INIT_FAILED) unless it may use this window station and
    // desktop, which on a service-run CI agent only Administrators may. Give
    // them a null DACL (everyone) on this throwaway machine and return the
    // "winsta\desktop" name for STARTUPINFO.
    static string OpenUpDesktop() {
        string winsta = ObjectName(GetProcessWindowStation());
        string desktop = ObjectName(GetThreadDesktop(GetCurrentThreadId()));
        // READ_CONTROL | WRITE_DAC | WRITE_OWNER
        IntPtr ws = OpenWindowStation(winsta, false, 0x00020000 | 0x00040000 | 0x00080000);
        if (ws == IntPtr.Zero) { throw new Win32Exception(); }
        OpenUp(ws);
        IntPtr dt = OpenDesktop(desktop, 0, false, 0x00020000 | 0x00040000 | 0x00080000);
        if (dt == IntPtr.Zero) { throw new Win32Exception(); }
        OpenUp(dt);
        return winsta + "\\" + desktop;
    }

    static IntPtr Sid(string text) {
        IntPtr sid;
        if (!ConvertStringSidToSid(text, out sid)) { throw new Win32Exception(); }
        return sid;
    }

    // Runs commandLine with a UAC-style filtered copy of this process's
    // token and returns its exit code (-1 on timeout). mediumIntegrity
    // also lowers the token to medium integrity, as UAC does.
    public static int Run(string commandLine, string cwd, int timeoutMs, bool mediumIntegrity) {
        IntPtr token;
        // TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID
        if (!OpenProcessToken(GetCurrentProcess(), 0x0001 | 0x0002 | 0x0008 | 0x0080 | 0x0100, out token)) { throw new Win32Exception(); }
        SID_AND_ATTRIBUTES[] disable = new SID_AND_ATTRIBUTES[2];
        disable[0].Sid = Sid("S-1-5-32-544");   // BUILTIN\Administrators
        disable[1].Sid = Sid("S-1-5-114");      // Local account and member of Administrators group
        IntPtr filtered;
        // DISABLE_MAX_PRIVILEGE
        if (!CreateRestrictedToken(token, 0x1, 2, disable, 0, IntPtr.Zero, 0, IntPtr.Zero, out filtered)) { throw new Win32Exception(); }

        IntPtr medium = Sid("S-1-16-8192");
        int labelSize = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
        IntPtr label = Marshal.AllocHGlobal(labelSize);
        SID_AND_ATTRIBUTES tml = new SID_AND_ATTRIBUTES();
        tml.Sid = medium;
        tml.Attributes = 0x20; // SE_GROUP_INTEGRITY
        Marshal.StructureToPtr(tml, label, false);
        // TokenIntegrityLevel
        if (mediumIntegrity && !SetTokenInformation(filtered, 25, label, (uint)labelSize + GetLengthSid(medium))) { throw new Win32Exception(); }

        STARTUPINFO si = new STARTUPINFO();
        si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
        si.lpDesktop = OpenUpDesktop();
        PROCESS_INFORMATION pi;
        // CREATE_NO_WINDOW; environment block null: this process's environment.
        if (!CreateProcessAsUser(filtered, null, commandLine, IntPtr.Zero, IntPtr.Zero, false, 0x08000000, IntPtr.Zero, cwd, ref si, out pi)) {
            throw new Win32Exception();
        }
        uint code = 0;
        if (WaitForSingleObject(pi.hProcess, (uint)timeoutMs) != 0) {
            TerminateProcess(pi.hProcess, 1);
            code = unchecked((uint)-1);
        } else {
            GetExitCodeProcess(pi.hProcess, out code);
        }
        CloseHandle(pi.hThread); CloseHandle(pi.hProcess);
        CloseHandle(filtered); CloseHandle(token);
        return unchecked((int)code);
    }
}
'@
        $r = Invoke-Autter -Arguments @('bg', 'status')
        Check ($r.ExitCode -eq 0) 'elevated service is running'
        $elevPid = Get-ServicePid

        # The profile folder lives under the runner's work folder, which only
        # Administrators may write; give the user itself access, as a real
        # profile folder does.
        $sid = $identity.User.Value
        & icacls.exe $script:Root /grant ("*{0}:(OI)(CI)F" -f $sid) /T /Q | Out-Null

        $elevDir = Join-Path $script:Root 'elevated'
        New-Item -ItemType Directory -Force -Path $elevDir | Out-Null
        $wrapper = Join-Path $elevDir 'as-standard-user.cmd'
        $steps = @(
            @('status', 'bg status'),
            @('start', 'bg start'),
            @('restart', 'bg restart'),
            @('shutdown', 'bg shutdown'),
            @('doctor', 'doctor --json')
        )
        $lines = @('@echo off', 'set "CI="', ('cd /d "{0}"' -f $RepoDir),
            ('whoami /groups > "{0}\whoami.txt" 2>&1' -f $elevDir))
        foreach ($s in $steps) {
            $lines += ('"{0}" {1} > "{2}\{3}.out" 2> "{2}\{3}.err"' -f $script:AutterExe, $s[1], $elevDir, $s[0])
            $lines += ('echo %ERRORLEVEL% > "{0}\{1}.code"' -f $elevDir, $s[0])
        }
        $lines += ('echo done > "{0}\done"' -f $elevDir)
        [IO.File]::WriteAllText($wrapper, (($lines -join "`r`n") + "`r`n"), [Text.Encoding]::ASCII)

        $mediumIl = $true
        $code = [FilteredToken]::Run(('cmd.exe /c "' + $wrapper + '"'), $RepoDir, 600000, $true)
        if ($code -eq -1073741502) {
            # STATUS_DLL_INIT_FAILED: a medium-integrity process cannot use the
            # service session's desktop on this runner. Keep the integrity
            # level; Administrators stay deny-only, which is what decides
            # access to the service's pipes and files.
            $mediumIl = $false
            Note 'a medium-integrity process cannot start on this runner''s service desktop (STATUS_DLL_INIT_FAILED); the standard-user token keeps high integrity, with Administrators deny-only and no privileges'
            $code = [FilteredToken]::Run(('cmd.exe /c "' + $wrapper + '"'), $RepoDir, 600000, $false)
        }
        Write-Host ("  standard-user commands finished with exit {0}" -f $code)
        function Read-Part([string]$name) {
            $p = Join-Path $elevDir $name
            if (Test-Path -LiteralPath $p) { return [IO.File]::ReadAllText($p) } else { return '' }
        }
        $groups = Read-Part 'whoami.txt'
        $restricted = ($groups -match 'S-1-5-32-544\s+Group used for deny only') -and (-not $mediumIl -or $groups -match 'Medium Mandatory Level')
        Check $restricted 'the standard-user token has Administrators deny-only' $groups
        foreach ($s in $steps) {
            $c = (Read-Part ($s[0] + '.code')).Trim()
            $text = (Read-Part ($s[0] + '.out')) + (Read-Part ($s[0] + '.err'))
            [IO.File]::WriteAllText((Join-Path $script:LogDir ("elevated-" + $s[0] + ".txt")), ("exit " + $c + "`r`n" + $text))
            Write-Host ("  standard user: autter {0} -> exit {1}" -f $s[1], $c)
            foreach ($l in @($text -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -First 12)) { Write-Host ("    |" + $l) }
        }
        $hint = 'started from an Administrator window'
        foreach ($s in @(@('status', 'bg status'), @('start', 'bg start'), @('restart', 'bg restart'), @('shutdown', 'bg shutdown'), @('doctor', 'doctor'))) {
            $text = (Read-Part ($s[0] + '.out')) + (Read-Part ($s[0] + '.err'))
            Check ($text.Contains($hint)) ("{0} from a standard user names the elevated service as the cause" -f $s[1]) $text
            Check (-not ($text -match 'ERROR: The process|ERROR: Access')) ("{0} from a standard user shows no raw taskkill error" -f $s[1]) $text
        }
        $r = Invoke-Autter -Arguments @('bg', 'status')
        Check ($r.ExitCode -eq 0 -and (Get-ServicePid) -eq $elevPid) 'the elevated service is untouched by the standard-user attempts' (Show-Output $r)
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
