#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateRange(1, [int]::MaxValue)]
    [int]$Issue,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$Message,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string[]]$Paths,
    [switch]$NoReuse
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repositoryRoot
$repository = ([string](& gh repo view --json nameWithOwner --jq '.nameWithOwner')).Trim()
if ($LASTEXITCODE -ne 0 -or -not $repository) { throw 'Unable to resolve the GitHub repository.' }

$issueState = ([string](& gh issue view $Issue --repo $repository --json state --jq '.state')).Trim()
if ($LASTEXITCODE -ne 0) { throw "Unable to read Issue #$Issue." }
if ($issueState -ne 'OPEN') { throw "Issue #$Issue is not open." }

& git diff --cached --quiet
if ($LASTEXITCODE -eq 1) { throw 'Staged changes already exist; finish or unstage them first.' }
if ($LASTEXITCODE -ne 0) { throw 'Unable to inspect staged changes.' }

& git add -- $Paths
if ($LASTEXITCODE -ne 0) { throw 'Unable to stage the requested paths.' }
& git diff --cached --quiet
if ($LASTEXITCODE -eq 0) { throw 'The requested paths contain no changes.' }
if ($LASTEXITCODE -ne 1) { throw 'Unable to inspect staged changes.' }
& git diff --quiet
if ($LASTEXITCODE -ne 0) { throw 'Unstaged tracked changes remain; include them explicitly or finish them separately.' }
$untracked = @(& git ls-files --others --exclude-standard)
if ($LASTEXITCODE -ne 0) { throw 'Unable to inspect untracked files.' }
if ($untracked.Count -gt 0) { throw "Untracked files remain: $($untracked -join ', ')" }

$verifyArguments = @('tools/verify.py', '--release')
if ($NoReuse) { $verifyArguments += '--no-reuse' }
& python @verifyArguments
if ($LASTEXITCODE -ne 0) { throw 'Release validation failed; finish was cancelled.' }

& git diff --cached --check
if ($LASTEXITCODE -ne 0) { throw 'Staged changes failed whitespace validation.' }
& git commit -m "$Message (#$Issue)"
if ($LASTEXITCODE -ne 0) { throw 'Git commit failed.' }
$commit = ([string](& git rev-parse --short HEAD)).Trim()

$summary = Join-Path $repositoryRoot 'artifacts/verify/summary.json'
$comment = @"
用户已确认验收完成。

- Release 收尾验证通过：格式、Clippy、测试、全部无界面场景与 Release 构建。
- 验证汇总：``artifacts/verify/summary.json``
- Release 程序：``target/release/asterfiles.exe``
- Commit：``$commit``
"@
& gh issue comment $Issue --repo $repository --body $comment
if ($LASTEXITCODE -ne 0) { throw 'Unable to write the Issue verification comment.' }

$project = & gh project list --owner LiuYangArt --format json | ConvertFrom-Json |
    Select-Object -ExpandProperty projects |
    Where-Object title -eq 'AsterFiles Development' |
    Select-Object -First 1
if (-not $project) { throw 'AsterFiles Development project was not found.' }
$itemList = & gh project item-list $project.number --owner LiuYangArt --format json --limit 100 | ConvertFrom-Json
$item = $itemList.items |
    Where-Object { $_.content.number -eq $Issue -and $_.content.repository -eq $repository } |
    Select-Object -First 1
if (-not $item) { throw "Issue #$Issue is not in AsterFiles Development." }
$fieldList = & gh project field-list $project.number --owner LiuYangArt --format json | ConvertFrom-Json
$statusField = $fieldList.fields | Where-Object name -eq 'Status' | Select-Object -First 1
$done = $statusField.options | Where-Object name -eq 'Done' | Select-Object -First 1
if (-not $statusField -or -not $done) { throw 'Project Done status was not found.' }
& gh project item-edit --id $item.id --project-id $project.id --field-id $statusField.id --single-select-option-id $done.id
if ($LASTEXITCODE -ne 0) { throw 'Unable to set the project item to Done.' }
& gh issue close $Issue --repo $repository --reason completed
if ($LASTEXITCODE -ne 0) { throw "Unable to close Issue #$Issue." }

[PSCustomObject]@{
    issue = $Issue
    commit = $commit
    verification = $summary
    projectStatus = 'Done'
    issueState = 'CLOSED'
} | ConvertTo-Json