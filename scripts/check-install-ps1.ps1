# Static checks for install.ps1, which users run as `irm <url> | iex`.
#
#   powershell -NoProfile -File scripts/check-install-ps1.ps1   # Windows PowerShell 5.1
#   pwsh -NoProfile -File scripts/check-install-ps1.ps1         # PowerShell 7+
#
# Run it under both: each engine has its own parser, and 5.1 is the one that
# rejects PowerShell 7-only syntax (??, ?., ternaries, && / ||).
# Exits non-zero when any check fails. Must itself stay 5.1-compatible.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$scriptPath = Join-Path $PSScriptRoot '..\install.ps1'
$scriptPath = [IO.Path]::GetFullPath($scriptPath)
$failures = New-Object System.Collections.Generic.List[string]

Write-Host ("PowerShell {0} ({1})" -f $PSVersionTable.PSVersion, $PSVersionTable.PSEdition)
Write-Host "Checking $scriptPath"

# 1. Pure ASCII. irm decodes with the charset the server sends; a mis-decoded
#    em-dash or smart quote can turn into a quote character PowerShell parses
#    as a string delimiter, which is a parse error for every user.
$bytes = [IO.File]::ReadAllBytes($scriptPath)
$line = 1
$lastReportedLine = 0
for ($i = 0; $i -lt $bytes.Length; $i++) {
    if ($bytes[$i] -eq 10) { $line++ }
    if ($bytes[$i] -gt 127 -and $line -ne $lastReportedLine) {
        $failures.Add(("line {0}: non-ASCII byte 0x{1:X2} (replace smart punctuation with - ... ' or `")" -f $line, $bytes[$i]))
        $lastReportedLine = $line
    }
}

# 2. Parse with this engine's parser, from the same string iex would receive.
$text = [IO.File]::ReadAllText($scriptPath)
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseInput($text, [ref]$tokens, [ref]$parseErrors)
foreach ($e in $parseErrors) {
    $failures.Add(("parse error at line {0}, column {1}: {2}" -f $e.Extent.StartLineNumber, $e.Extent.StartColumnNumber, $e.Message))
}

# 3. Constructs that break or misbehave under iex, which runs the text in the
#    caller's own scope.
if ($ast.ParamBlock) {
    $failures.Add('top-level param() block: iex cannot pass parameters; read environment variables instead')
}
$exits = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.ExitStatementAst] }, $true)
foreach ($n in $exits) {
    $failures.Add(("line {0}: 'exit' closes the user's terminal under iex; use Write-ErrorAndExit (throws) or return" -f $n.Extent.StartLineNumber))
}
$vars = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.VariableExpressionAst] }, $true)
foreach ($n in $vars) {
    $name = $n.VariablePath.UserPath
    if ($name -eq 'PSScriptRoot' -or $name -eq 'PSCommandPath' -or $name -eq 'MyInvocation') {
        $failures.Add(("line {0}: `${1} is empty under iex" -f $n.Extent.StartLineNumber, $name))
    }
}

# Everything must run inside one scriptblock, so StrictMode, preferences, and
# helper functions (including the Write-Warning override) don't leak into the
# user's session. Allow comments and exactly one top-level statement: `& { ... }`.
$statements = @()
if ($ast.EndBlock) { $statements = @($ast.EndBlock.Statements) }
$wrapped = $false
if ($statements.Count -eq 1) {
    $pipeline = $statements[0]
    if ($pipeline -is [System.Management.Automation.Language.PipelineAst] -and $pipeline.PipelineElements.Count -eq 1) {
        $cmd = $pipeline.PipelineElements[0]
        if ($cmd -is [System.Management.Automation.Language.CommandAst] -and
            $cmd.InvocationOperator -eq [System.Management.Automation.Language.TokenKind]::Ampersand -and
            $cmd.CommandElements.Count -eq 1 -and
            $cmd.CommandElements[0] -is [System.Management.Automation.Language.ScriptBlockExpressionAst]) {
            $wrapped = $true
        }
    }
}
if (-not $wrapped) {
    $failures.Add(("expected the whole script to be a single '& {{ ... }}' scriptblock, found {0} top-level statement(s)" -f $statements.Count))
}

# 4. Behaviour pinned by the text. The end-to-end test runs the dry run, a
#    failed checksum download and the progress download for real; these checks
#    cover what it cannot reach (the other failure messages) and keep the
#    pieces from being removed by accident. Single-quoted strings throughout:
#    they hold installer variable names that must not expand here.
$lines = @($text -split "`n")

# The dry run is an environment variable (iex cannot pass arguments) and its
# block comes before anything that makes a request.
if (-not $text.Contains('$env:AUTTER_INSTALL_DRY_RUN -eq ''1''')) {
    $failures.Add('no dry run switch: expected $env:AUTTER_INSTALL_DRY_RUN -eq ''1''')
}
$dryRunAt = $text.IndexOf('Dry run: nothing is downloaded, written or started.')
$dryRunEndAt = -1
if ($dryRunAt -ge 0) { $dryRunEndAt = $text.IndexOf('return', $dryRunAt) }
if ($dryRunAt -lt 0 -or $dryRunEndAt -lt 0) {
    $failures.Add('no dry run block: expected the plan to be printed, followed by return')
} else {
    foreach ($needle in @('Invoke-WebRequest -Uri', 'Invoke-RestMethod -Uri', '[System.Net.WebRequest]::Create')) {
        $requestAt = $text.IndexOf($needle)
        if ($requestAt -ge 0 -and $requestAt -lt $dryRunEndAt) {
            $failures.Add(('the dry run must return before the first ''{0}'', so a dry run makes no request' -f $needle))
        }
    }
}

# Every failed download names the URL and the npm route.
$hintLines = @($lines | Where-Object { $_.StartsWith('$NpmFallbackHint = ') })
if ($hintLines.Count -ne 1) {
    $failures.Add('expected exactly one line that starts with $NpmFallbackHint = ')
} else {
    foreach ($needle in @('npm install -g @autter/cli', 'autter onboard')) {
        if (-not $hintLines[0].Contains($needle)) {
            $failures.Add(('$NpmFallbackHint does not mention ''{0}''' -f $needle))
        }
    }
}
foreach ($needle in @('Failed to resolve the latest release from ${latestReleaseUrl}', 'Failed to download release checksums from ${checksumsUrl}')) {
    $failureLines = @($lines | Where-Object { $_.Contains($needle) })
    if ($failureLines.Count -ne 1) {
        $failures.Add(('expected exactly one error line containing ''{0}''' -f $needle))
    } elseif (-not $failureLines[0].Contains('$NpmFallbackHint')) {
        $failures.Add(('the error ''{0}'' does not include $NpmFallbackHint' -f $needle))
    }
}
$binaryHintLines = @($lines | Where-Object { $_.Contains('$message +=') -and $_.Contains('$NpmFallbackHint') })
if ($binaryHintLines.Count -lt 1) {
    $failures.Add('the failed binary download message does not include $NpmFallbackHint')
}
# Windows PowerShell 5.1 cannot run `a && b`, so never print it as a command.
if ($text.Contains('@autter/cli &&')) {
    $failures.Add('prints npm install -g @autter/cli followed by &&: Windows PowerShell 5.1 has no &&; print the two commands on two lines')
}

# The download stays fast: Invoke-WebRequest's own progress bar is never
# turned on (5.1 redraws it per chunk), and progress comes from
# Save-UrlWithProgress instead.
if ($text.Contains('$ProgressPreference = ''Continue''')) {
    $failures.Add('$ProgressPreference is set to Continue: Invoke-WebRequest progress makes Windows PowerShell 5.1 downloads many times slower')
}
if (-not $text.Contains('function Save-UrlWithProgress')) {
    $failures.Add('no Save-UrlWithProgress function: the binary download shows no progress')
}

if ($failures.Count -gt 0) {
    Write-Host ''
    Write-Host ("install.ps1 failed {0} check(s):" -f $failures.Count) -ForegroundColor Red
    foreach ($f in $failures) { Write-Host "  $f" -ForegroundColor Red }
    exit 1
}
Write-Host 'install.ps1: ASCII-only, parses cleanly, iex-safe structure, dry run and npm route in place.' -ForegroundColor Green
