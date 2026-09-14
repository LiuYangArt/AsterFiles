$supportPath = Join-Path $PSScriptRoot 'finish-issue-support.ps1'
$scriptPath = Join-Path $PSScriptRoot 'finish-issue.ps1'
. $supportPath

Describe 'finish-issue support' {
    It 'has valid PowerShell syntax' {
        foreach ($path in @($supportPath, $scriptPath)) {
            $tokens = $null
            $errors = $null
            [System.Management.Automation.Language.Parser]::ParseFile(
                $path,
                [ref]$tokens,
                [ref]$errors
            ) | Out-Null
            @($errors) | Should BeNullOrEmpty
        }
    }

    It 'parses multiline JSON containing Chinese, quotes, newlines, and a long readme' {
        $payload = [ordered]@{
            title = '中文“项目”'
            note = '第一行' + [Environment]::NewLine + '第二行，包含"引号"'
            readme = ('很长的说明。' * 12000)
        }
        $lines = @($payload | ConvertTo-Json -Depth 10) -split "\r?\n"
        $runner = {
            param($Invocation)
            [PSCustomObject]@{ ExitCode = 0; Output = $lines }
        }.GetNewClosure()

        $result = Invoke-GhJson -Arguments @('project', 'list') -FailureContext 'read failed.' -Runner $runner

        $result.title | Should Be $payload.title
        $result.note | Should Be $payload.note
        $result.readme.Length | Should Be $payload.readme.Length
    }

    It 'keeps successful JSON output separate from CLI warnings' {
        $runner = {
            param($Invocation)
            [PSCustomObject]@{
                ExitCode = 0
                Output = @('{"ok":true}')
                ErrorOutput = @('a harmless warning')
            }
        }
        $previousWarningPreference = $WarningPreference
        $WarningPreference = 'SilentlyContinue'
        try {
            $result = Invoke-GhJson -Arguments @('project', 'list') -FailureContext 'read failed.' -Runner $runner
        }
        finally {
            $WarningPreference = $previousWarningPreference
        }

        $result.ok | Should Be $true
    }
    It 'requests only the Project fields required for finalization' {
        $calls = [System.Collections.Generic.List[string]]::new()
        $runner = {
            param($Invocation)
            $command = [string[]]$Invocation.Arguments
            $calls.Add(($command -join ' '))
            $output = switch ($command[1]) {
                'list' { '{"id":"project-id","number":7}' }
                'item-list' { '[{"id":"other","number":108,"repository":"Other/Repo"},{"id":"item-id","number":108,"repository":"LiuYangArt/AsterFiles"}]' }
                'field-list' { '{"id":"status-field","options":[{"id":"done-option","name":"Done"}]}' }
                default { throw "Unexpected command: $($command -join ' ')" }
            }
            [PSCustomObject]@{ ExitCode = 0; Output = @($output) }
        }.GetNewClosure()

        $context = Get-FinishIssueProjectContext -Owner 'LiuYangArt' -Repository 'LiuYangArt/AsterFiles' -Issue 108 -Runner $runner

        $context.ProjectId | Should Be 'project-id'
        $context.ItemId | Should Be 'item-id'
        $context.StatusFieldId | Should Be 'status-field'
        $context.DoneOptionId | Should Be 'done-option'
        $calls.Count | Should Be 3
        ($calls -join [Environment]::NewLine) | Should Match '--jq'
        ($calls -join [Environment]::NewLine) | Should Not Match 'readme'
        $calls[0] | Should Match ([regex]::Escape('{id, number}'))

        $calls[0] | Should Match ([regex]::Escape('[0] // null'))
        $calls[1] | Should Match '\{id, number: \.content\.number, repository: \.content\.repository\}'
        $calls[1] | Should Match '--limit 1000'
        $calls[2] | Should Match ([regex]::Escape('{id, options}'))

        $calls[2] | Should Match ([regex]::Escape('[0] // null'))
    }

    It 'distinguishes a missing Project from a failed Project read' {
        $runner = {
            param($Invocation)
            [PSCustomObject]@{ ExitCode = 0; Output = @('null') }
        }
        $message = $null

        try {
            Get-FinishIssueProjectContext -Owner 'LiuYangArt' -Repository 'LiuYangArt/AsterFiles' -Issue 108 -Runner $runner
        }
        catch {
            $message = $_.Exception.Message
        }

        $message | Should Be 'AsterFiles Development project was not found.'
    }
    It 'reports a Project read failure instead of claiming the Project is missing' {
        $runner = {
            param($Invocation)
            [PSCustomObject]@{ ExitCode = 2; Output = @(); ErrorOutput = @('network unavailable', 'request id 42') }
        }
        $message = $null

        try {
            Get-FinishIssueProjectContext -Owner 'LiuYangArt' -Repository 'LiuYangArt/AsterFiles' -Issue 108 -Runner $runner
        }
        catch {
            $message = $_.Exception.Message
        }

        $message | Should Match 'Unable to read AsterFiles Development project'
        $message | Should Match 'exited with code 2'
        $message | Should Match 'network unavailable'
        $message | Should Not Match 'was not found'
    }

    It 'diagnoses a failure after commit and stops before later remote changes' {
        $calls = [System.Collections.Generic.List[string]]::new()
        $runner = {
            param($Invocation)
            $command = [string[]]$Invocation.Arguments
            $calls.Add(($command -join ' '))
            if ($command[0] -eq 'project' -and $command[1] -eq 'item-edit') {
                return [PSCustomObject]@{ ExitCode = 9; Output = @('project mutation failed') }
            }
            [PSCustomObject]@{ ExitCode = 0; Output = @('ok') }
        }.GetNewClosure()
        $context = [PSCustomObject]@{
            ProjectId = 'project-id'
            ItemId = 'item-id'
            StatusFieldId = 'status-field'
            DoneOptionId = 'done-option'
        }
        $message = $null

        try {
            Complete-GitHubIssue -Issue 108 -Repository 'LiuYangArt/AsterFiles' -Comment '验证通过' -ProjectContext $context -Commit 'abc1234' -Runner $runner
        }
        catch {
            $message = $_.Exception.Message
        }

        $message | Should Match 'Commit abc1234 was created and kept'
        $message | Should Match 'set the Project item to Done'
        $message | Should Match 'Completed remote steps: Issue comment'
        $message | Should Match 'Do not rerun finish-issue.ps1'
        $message | Should Match 'exited with code 9'
        $calls.Count | Should Be 2
        ($calls -join [Environment]::NewLine) | Should Not Match 'issue close'
    }
}