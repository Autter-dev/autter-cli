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

if ($failures.Count -gt 0) {
    Write-Host ''
    Write-Host ("install.ps1 failed {0} check(s):" -f $failures.Count) -ForegroundColor Red
    foreach ($f in $failures) { Write-Host "  $f" -ForegroundColor Red }
    exit 1
}
Write-Host 'install.ps1: ASCII-only, parses cleanly, iex-safe structure.' -ForegroundColor Green
