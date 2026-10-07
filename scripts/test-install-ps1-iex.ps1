# End-to-end test of install.ps1 run the way users run it: `irm <url> | iex`.
# Windows only. CI dot-sources it so that, like a user's shell, the installer
# runs in the session's global scope:
#
#   powershell -NoProfile -Command ". ./scripts/test-install-ps1-iex.ps1"
#
# It really installs autter for the current user (%USERPROFILE%\.autter\bin,
# user PATH), so only run it on a throwaway machine such as a CI runner.
#
# By default the installer takes its normal path: resolve the latest GitHub
# release, download checksums.txt, download the binary, verify its SHA256.
# Set AUTTER_LOCAL_BINARY to install a locally built autter.exe instead.
#
# Checks:
#   1. A failing install throws instead of calling `exit`, so the user's shell
#      survives (issue #9 hardening).
#   2. A successful install leaves `autter` resolvable in THIS session, from
#      the install dir, without opening a new terminal (issue #11).
#   3. Nothing leaks into the caller's session: $ErrorActionPreference,
#      StrictMode, helper functions (incl. the Write-Warning override), and
#      installer variables; $LASTEXITCODE is left at 0.

# The defaults of an interactive session, so that leaks are detectable.
$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

$t_problems = New-Object System.Collections.Generic.List[string]
$t_repoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
if (-not $t_repoRoot) { $t_repoRoot = (Get-Location).Path }
$t_installerPath = Join-Path $t_repoRoot 'install.ps1'
$t_installDir = Join-Path $HOME '.autter\bin'
$t_expectedExe = Join-Path $t_installDir 'autter.exe'
$t_localBinary = $env:AUTTER_LOCAL_BINARY

Write-Host ("PowerShell {0} ({1})" -f $PSVersionTable.PSVersion, $PSVersionTable.PSEdition)

function Test-NormalizedPathEqual([string]$a, [string]$b) {
    if (-not $a -or -not $b) { return $false }
    return $a.Trim().TrimEnd('\').ToLowerInvariant() -eq $b.Trim().TrimEnd('\').ToLowerInvariant()
}

# Start from a session PATH without the install dir: finding autter after the
# install then proves the installer refreshed this session's PATH.
$env:Path = (@($env:Path -split ';') | Where-Object { $_ -and -not (Test-NormalizedPathEqual $_ $t_installDir) }) -join ';'
$t_preexisting = Get-Command autter -CommandType Application -ErrorAction SilentlyContinue
if ($t_preexisting) {
    Write-Host ("Note: an autter is already on PATH at {0}; the installer must prepend its own." -f $t_preexisting[0].Source)
}

# --- 1. Failure path: must throw, not exit ----------------------------------
Write-Host ''
Write-Host '=== install.ps1 | iex with a missing AUTTER_LOCAL_BINARY (must fail without closing the shell) ==='
$env:AUTTER_LOCAL_BINARY = Join-Path ([IO.Path]::GetTempPath()) 'autter-ci-does-not-exist.exe'
$t_failure = $null
try {
    Get-Content -Raw -LiteralPath $t_installerPath | Invoke-Expression
} catch {
    $t_failure = $_
}
# Reaching this line at all proves the installer did not `exit` the session.
if (-not $t_failure) {
    $t_problems.Add('failure path: the installer did not throw for a missing AUTTER_LOCAL_BINARY')
} elseif ("$t_failure" -notlike '*Local binary not found*') {
    $t_problems.Add("failure path: unexpected error: $t_failure")
} else {
    Write-Host "OK: installer threw '$t_failure' and the session survived."
}
if ($t_localBinary) { $env:AUTTER_LOCAL_BINARY = $t_localBinary } else { Remove-Item Env:\AUTTER_LOCAL_BINARY -ErrorAction SilentlyContinue }

# --- 2. Success path --------------------------------------------------------
Write-Host ''
Write-Host '=== install.ps1 | iex (must install and make autter resolvable in this session) ==='
$t_installError = $null
try {
    Get-Content -Raw -LiteralPath $t_installerPath | Invoke-Expression
} catch {
    $t_installError = $_
}
Write-Host ''
Write-Host '=== checks ==='
if ($t_installError) {
    $t_problems.Add("install failed: $t_installError")
}
if ($LASTEXITCODE -ne 0) {
    $t_problems.Add("installer left `$LASTEXITCODE = $LASTEXITCODE (expected 0)")
}
if (-not (Test-Path -LiteralPath $t_expectedExe)) {
    $t_problems.Add("autter.exe was not installed at $t_expectedExe")
}

$t_cmd = Get-Command autter -CommandType Application -ErrorAction SilentlyContinue
if (-not $t_cmd) {
    $t_problems.Add('autter is not resolvable in this session after install (session PATH not refreshed)')
} elseif (-not (Test-NormalizedPathEqual $t_cmd[0].Source $t_expectedExe)) {
    $t_problems.Add(("autter resolves to {0}, expected {1} (install dir not prepended to the session PATH)" -f $t_cmd[0].Source, $t_expectedExe))
} else {
    $t_version = (& autter --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $t_version) {
        $t_problems.Add("autter --version failed (exit $LASTEXITCODE): $t_version")
    } else {
        Write-Host "OK: 'autter --version' in the same session -> $t_version"
    }
}

$t_userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (@($t_userPath -split ';') | Where-Object { Test-NormalizedPathEqual $_ $t_installDir })) {
    $t_problems.Add("install dir missing from the persistent user PATH: $t_userPath")
}

# --- 3. Nothing leaked into the caller's session ----------------------------
if ($ErrorActionPreference -ne 'Continue') {
    $t_problems.Add("`$ErrorActionPreference leaked into the session: $ErrorActionPreference")
}
try {
    $null = $t_thisVariableIsNeverSet
} catch {
    $t_problems.Add('Set-StrictMode leaked into the session')
}
foreach ($t_name in @('Write-ErrorAndExit', 'Write-Success', 'Verify-Checksum', 'Try-Download', 'Set-PathEnsureContains', 'Start-DaemonIfRequested')) {
    if (Get-Command $t_name -CommandType Function -ErrorAction SilentlyContinue) {
        $t_problems.Add("installer function '$t_name' leaked into the session")
    }
}
if ((Get-Command Write-Warning).CommandType -ne 'Cmdlet') {
    $t_problems.Add("the installer's Write-Warning override leaked into the session")
}
foreach ($t_name in @('finalExe', 'installDir', 'releaseTag', 'EmbeddedChecksums', 'pathUpdate')) {
    if (Get-Variable -Name $t_name -ErrorAction SilentlyContinue) {
        $t_problems.Add("installer variable `$$t_name leaked into the session")
    }
}

if ($t_problems.Count -gt 0) {
    Write-Host ''
    Write-Host ("FAILED: {0} problem(s):" -f $t_problems.Count) -ForegroundColor Red
    foreach ($t_p in $t_problems) { Write-Host "  $t_p" -ForegroundColor Red }
    exit 1
}
Write-Host 'All install.ps1 | iex checks passed.' -ForegroundColor Green
exit 0
