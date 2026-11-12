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
#   0. A dry run ($env:AUTTER_INSTALL_DRY_RUN = '1') prints its plan and
#      creates nothing under a temporary profile directory.
#   1. A failing install throws instead of calling `exit`, so the user's shell
#      survives (issue #9 hardening). A failed download names the URL and the
#      npm route.
#   2. A successful install leaves `autter` resolvable in THIS session, from
#      the install dir, without opening a new terminal (issue #11). A second
#      install with the progress download forced on shows progress to 100%.
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

function Get-FilesUnder([string]$dir) {
    if (-not (Test-Path -LiteralPath $dir)) { return @() }
    return @(Get-ChildItem -LiteralPath $dir -Recurse -Force | ForEach-Object { $_.FullName })
}

# --- 0. Dry run: prints the plan, changes nothing ---------------------------
# Runs first, against an empty temporary profile directory: $HOME is pointed
# at it for the length of the run, so anything the installer wrote would show
# up there. The real profile is compared before and after as well.
Write-Host ''
Write-Host '=== install.ps1 | iex with AUTTER_INSTALL_DRY_RUN=1 (must print the plan and change nothing) ==='
$t_realHome = $HOME
$t_dryHome = Join-Path ([IO.Path]::GetTempPath()) ('autter-ci-dry-run-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $t_dryHome | Out-Null
$t_realFilesBefore = (Get-FilesUnder (Join-Path $t_realHome '.autter\bin')) -join '|'
$t_userPathBefore = [Environment]::GetEnvironmentVariable('Path', 'User')
$t_sessionPathBefore = $env:Path
$t_releaseTag = $env:AUTTER_RELEASE_TAG
$t_dryOutput = ''
$t_dryError = $null
# Pinned to a release so the plan names the exact download. No local binary:
# the dry run makes no request, so it can describe the normal download.
$env:AUTTER_INSTALL_DRY_RUN = '1'
$env:AUTTER_RELEASE_TAG = 'v3.5.1'
Remove-Item Env:\AUTTER_LOCAL_BINARY -ErrorAction SilentlyContinue
try {
    # $HOME is read-only, not constant: -Force replaces it for every scope.
    Set-Variable -Name HOME -Value $t_dryHome -Force -ErrorAction Stop
    # Write-Host writes to the information stream (6); merge it into the
    # output so the plan can be checked. Joined by hand, not Out-String,
    # which wraps long lines (the paths) at the console width.
    $t_dryRecords = @(Get-Content -Raw -LiteralPath $t_installerPath | Invoke-Expression 6>&1)
    $t_dryOutput = ($t_dryRecords | ForEach-Object { "$_" }) -join "`n"
} catch {
    $t_dryError = $_
} finally {
    Set-Variable -Name HOME -Value $t_realHome -Force
    Remove-Item Env:\AUTTER_INSTALL_DRY_RUN -ErrorAction SilentlyContinue
    if ($t_releaseTag) { $env:AUTTER_RELEASE_TAG = $t_releaseTag } else { Remove-Item Env:\AUTTER_RELEASE_TAG -ErrorAction SilentlyContinue }
    if ($t_localBinary) { $env:AUTTER_LOCAL_BINARY = $t_localBinary }
}
Write-Host $t_dryOutput
if ($HOME -ne $t_realHome) {
    $t_problems.Add("dry run: the test could not restore `$HOME (now $HOME)")
}
if ($t_dryError) {
    $t_problems.Add("dry run: the installer threw: $t_dryError")
}
# Each thing a real install does is named: the download and its check, the
# binary path, the PATH change, the ping, onboarding, and how to undo it.
$t_dryExpectations = @(
    'Dry run: nothing is downloaded, written or started.',
    '/releases/download/v3.5.1/autter-windows-',
    'SHA-256',
    (Join-Path $t_dryHome '.autter\bin\autter.exe'),
    'user PATH',
    'send no install ping',
    'autter onboard',
    'asks before',
    'Remove-Item Env:\AUTTER_INSTALL_DRY_RUN',
    'autter uninstall'
)
if ($env:AUTTER_NO_INSTALL_PING -ne '1') {
    $t_dryExpectations = @($t_dryExpectations | Where-Object { $_ -ne 'send no install ping' }) + 'anonymous install ping'
}
foreach ($t_expected in $t_dryExpectations) {
    if (-not "$t_dryOutput".Contains($t_expected)) {
        $t_problems.Add("dry run: the plan does not mention '$t_expected'")
    }
}
$t_dryCreated = @(Get-FilesUnder $t_dryHome)
if ($t_dryCreated.Count -ne 0) {
    $t_problems.Add("dry run: created under the temporary profile: $($t_dryCreated -join ', ')")
}
if (((Get-FilesUnder (Join-Path $t_realHome '.autter\bin')) -join '|') -ne $t_realFilesBefore) {
    $t_problems.Add('dry run: files under the real .autter\bin changed')
}
if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $t_userPathBefore) {
    $t_problems.Add('dry run: the persistent user PATH changed')
}
if ($env:Path -ne $t_sessionPathBefore) {
    $t_problems.Add('dry run: the session PATH changed')
}
if ($t_problems.Count -eq 0) {
    Write-Host 'OK: the dry run printed its plan and changed nothing.'
}
Remove-Item -LiteralPath $t_dryHome -Recurse -Force -ErrorAction SilentlyContinue

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

# --- 1b. A failed download names the URL and the npm route ------------------
# A release tag that does not exist: its checksums.txt is a 404, the first
# download a real install makes after resolving the release. Skipped with a
# local binary, which makes no request.
if (-not $t_localBinary) {
    Write-Host ''
    Write-Host '=== install.ps1 | iex with a release that does not exist (must name the URL and the npm route) ==='
    $t_missingTag = 'v0.0.0-autter-ci-no-such-release'
    $env:AUTTER_RELEASE_TAG = $t_missingTag
    $t_downloadFailure = $null
    try {
        Get-Content -Raw -LiteralPath $t_installerPath | Invoke-Expression
    } catch {
        $t_downloadFailure = $_
    }
    if ($t_releaseTag) { $env:AUTTER_RELEASE_TAG = $t_releaseTag } else { Remove-Item Env:\AUTTER_RELEASE_TAG -ErrorAction SilentlyContinue }
    if (-not $t_downloadFailure) {
        $t_problems.Add('download failure: the installer did not throw for a release that does not exist')
    } else {
        foreach ($t_expected in @("/releases/download/$t_missingTag/checksums.txt", 'npm install -g @autter/cli', 'autter onboard')) {
            if (-not "$t_downloadFailure".Contains($t_expected)) {
                $t_problems.Add("download failure: the error does not mention '$t_expected': $t_downloadFailure")
            }
        }
        if ($t_problems.Count -eq 0) {
            Write-Host 'OK: the failed download named the URL and the npm route, and the session survived.'
        }
    }
}

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

# --- 2b. Download progress ---------------------------------------------------
# CI output is redirected, so the install above took the quiet download.
# AUTTER_INSTALL_PROGRESS=1 forces the progress download a person at a
# terminal gets; reinstall with it and check the status line reached 100%.
# Skipped with a local binary, which is copied, not downloaded.
if (-not $t_localBinary) {
    Write-Host ''
    Write-Host '=== install.ps1 | iex with AUTTER_INSTALL_PROGRESS=1 (must show download progress and install) ==='
    $t_progressOutput = ''
    $t_progressError = $null
    $env:AUTTER_INSTALL_PROGRESS = '1'
    try {
        $t_progressRecords = @(Get-Content -Raw -LiteralPath $t_installerPath | Invoke-Expression 6>&1)
        $t_progressOutput = ($t_progressRecords | ForEach-Object { "$_" }) -join "`n"
    } catch {
        $t_progressError = $_
    } finally {
        Remove-Item Env:\AUTTER_INSTALL_PROGRESS -ErrorAction SilentlyContinue
    }
    Write-Host ($t_progressOutput -replace "`r", "`n")
    if ($t_progressError) {
        $t_problems.Add("progress install failed: $t_progressError")
    } elseif ($t_progressOutput -notmatch 'MB of \S+ MB \(100%\)') {
        $t_problems.Add('progress install: no "<n> MB of <n> MB (100%)" status line; the progress download was not used or did not finish')
    } elseif (-not (Test-Path -LiteralPath $t_expectedExe)) {
        $t_problems.Add("progress install: autter.exe is missing at $t_expectedExe")
    } else {
        Write-Host 'OK: the progress download reached 100% and autter was installed.'
    }
    if ($LASTEXITCODE -ne 0) {
        $t_problems.Add("progress install left `$LASTEXITCODE = $LASTEXITCODE (expected 0)")
    }
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
foreach ($t_name in @('Write-ErrorAndExit', 'Write-Success', 'Verify-Checksum', 'Try-Download', 'Set-PathEnsureContains', 'Start-DaemonIfRequested', 'Save-UrlWithProgress', 'Get-GitBashPlan', 'Test-UserPathContains')) {
    if (Get-Command $t_name -CommandType Function -ErrorAction SilentlyContinue) {
        $t_problems.Add("installer function '$t_name' leaked into the session")
    }
}
if ((Get-Command Write-Warning).CommandType -ne 'Cmdlet') {
    $t_problems.Add("the installer's Write-Warning override leaked into the session")
}
foreach ($t_name in @('finalExe', 'installDir', 'releaseTag', 'EmbeddedChecksums', 'pathUpdate', 'dryRun', 'drySteps', 'NpmFallbackHint', 'showDownloadProgress')) {
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
