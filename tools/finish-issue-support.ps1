Set-StrictMode -Version Latest

function Invoke-GhCapture {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,
        [scriptblock]$Runner
    )

    if ($Runner) {
        $result = & $Runner ([PSCustomObject]@{ Arguments = $Arguments })
        if ($null -eq $result -or
            $result.PSObject.Properties.Name -notcontains 'ExitCode' -or
            $result.PSObject.Properties.Name -notcontains 'Output') {
            throw 'The GitHub CLI runner returned an invalid result.'
        }
        $errorOutput = if ($result.PSObject.Properties.Name -contains 'ErrorOutput') {
            @($result.ErrorOutput | ForEach-Object { [string]$_ })
        }
        else {
            @()
        }
        return [PSCustomObject]@{
            ExitCode = [int]$result.ExitCode
            Output = @($result.Output | ForEach-Object { [string]$_ })
            ErrorOutput = $errorOutput
        }
    }

    $combined = @(& gh @Arguments 2>&1)
    $exitCode = $LASTEXITCODE
    return [PSCustomObject]@{
        ExitCode = $exitCode
        Output = @($combined | Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] } | ForEach-Object { [string]$_ })
        ErrorOutput = @($combined | Where-Object { $_ -is [System.Management.Automation.ErrorRecord] } | ForEach-Object { [string]$_ })
    }
}

function Invoke-GhText {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,
        [Parameter(Mandatory = $true)]
        [string]$FailureContext,
        [scriptblock]$Runner
    )

    $result = Invoke-GhCapture -Arguments $Arguments -Runner $Runner
    $text = ($result.Output -join [Environment]::NewLine).Trim()
    $errorText = ($result.ErrorOutput -join [Environment]::NewLine).Trim()
    if ($result.ExitCode -ne 0) {
        $diagnostic = @(@($text, $errorText) | Where-Object { $_ })
        $detail = if ($diagnostic.Count -gt 0) { " Output: $($diagnostic -join [Environment]::NewLine)" } else { '' }
        throw "$FailureContext GitHub CLI exited with code $($result.ExitCode).$detail"
    }
    if ($errorText) { Write-Warning $errorText }
    return $text
}

function Invoke-GhJson {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,
        [Parameter(Mandatory = $true)]
        [string]$FailureContext,
        [scriptblock]$Runner
    )

    $text = Invoke-GhText -Arguments $Arguments -FailureContext $FailureContext -Runner $Runner
    if (-not $text) {
        throw "$FailureContext GitHub CLI returned no JSON."
    }
    try {
        return $text | ConvertFrom-Json -Depth 100
    }
    catch {
        throw "$FailureContext GitHub CLI returned invalid JSON: $($_.Exception.Message)"
    }
}

function Get-FinishIssueProjectContext {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Owner,
        [Parameter(Mandatory = $true)]
        [string]$Repository,
        [Parameter(Mandatory = $true)]
        [int]$Issue,
        [scriptblock]$Runner
    )

    $project = Invoke-GhJson -Arguments @(
        'project', 'list', '--owner', $Owner, '--format', 'json',
        '--jq', '[.projects[] | select(.title == "AsterFiles Development") | {id, number}][0] // null'
    ) -FailureContext 'Unable to read AsterFiles Development project.' -Runner $Runner
    if (-not $project) { throw 'AsterFiles Development project was not found.' }

    $items = @(Invoke-GhJson -Arguments @(
        'project', 'item-list', [string]$project.number, '--owner', $Owner,
        '--format', 'json', '--limit', '1000',
        '--jq', '[.items[] | {id, number: .content.number, repository: .content.repository}]'
    ) -FailureContext "Unable to read AsterFiles Development items for Issue #$Issue." -Runner $Runner)
    $item = $items | Where-Object { $_.number -eq $Issue -and $_.repository -eq $Repository } | Select-Object -First 1
    if (-not $item) { throw "Issue #$Issue is not in AsterFiles Development." }

    $statusField = Invoke-GhJson -Arguments @(
        'project', 'field-list', [string]$project.number, '--owner', $Owner,
        '--format', 'json',
        '--jq', '[.fields[] | select(.name == "Status") | {id, options}][0] // null'
    ) -FailureContext 'Unable to read AsterFiles Development Status field.' -Runner $Runner
    if (-not $statusField) { throw 'Project Status field was not found.' }
    $done = $statusField.options | Where-Object name -eq 'Done' | Select-Object -First 1
    if (-not $done) { throw 'Project Done status was not found.' }

    return [PSCustomObject]@{
        ProjectId = [string]$project.id
        ItemId = [string]$item.id
        StatusFieldId = [string]$statusField.id
        DoneOptionId = [string]$done.id
    }
}

function Complete-GitHubIssue {
    param(
        [Parameter(Mandatory = $true)]
        [int]$Issue,
        [Parameter(Mandatory = $true)]
        [string]$Repository,
        [Parameter(Mandatory = $true)]
        [string]$Comment,
        [Parameter(Mandatory = $true)]
        [PSCustomObject]$ProjectContext,
        [Parameter(Mandatory = $true)]
        [string]$Commit,
        [scriptblock]$Runner
    )

    $completed = [System.Collections.Generic.List[string]]::new()
    $step = 'write the Issue verification comment'
    try {
        Invoke-GhText -Arguments @('issue', 'comment', [string]$Issue, '--repo', $Repository, '--body', $Comment) `
            -FailureContext "Unable to write the Issue #$Issue verification comment." -Runner $Runner | Out-Null
        $completed.Add('Issue comment')

        $step = 'set the Project item to Done'
        Invoke-GhText -Arguments @(
            'project', 'item-edit', '--id', $ProjectContext.ItemId,
            '--project-id', $ProjectContext.ProjectId,
            '--field-id', $ProjectContext.StatusFieldId,
            '--single-select-option-id', $ProjectContext.DoneOptionId
        ) -FailureContext 'Unable to set the Project item to Done.' -Runner $Runner | Out-Null
        $completed.Add('Project status Done')

        $step = 'close the Issue'
        Invoke-GhText -Arguments @('issue', 'close', [string]$Issue, '--repo', $Repository, '--reason', 'completed') `
            -FailureContext "Unable to close Issue #$Issue." -Runner $Runner | Out-Null
        $completed.Add('Issue closed')
    }
    catch {
        $completedText = if ($completed.Count -gt 0) { $completed -join ', ' } else { 'none' }
        throw "Commit $Commit was created and kept, but GitHub finalization failed while attempting to $step. Completed remote steps: $completedText. Do not rerun finish-issue.ps1 with the same changes; inspect Issue #$Issue and the Project, then complete the remaining GitHub steps manually. Cause: $($_.Exception.Message)"
    }
}
