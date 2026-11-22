#requires -Version 5.1
<#
R9 dedicated product-EXE regression: a root with two synthetic subagents,
one active after a single task_started and one completed with a final answer.
Reproduces the *shape* of two real October 30 rollouts without copying their
private content. Runs entirely inside a freshly created synthetic CODEX_HOME.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [Parameter(Mandatory=$true)][string]$WorkDirectory,
    [Parameter(Mandatory=$true)][string]$LogDirectory
)
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$Utf8 = [Text.UTF8Encoding]::new($false)
$ProjectId = 'r9-synthetic-project'
$Cwd = 'C:\synthetic-codex-r9-workspace'
$RootId = '66666666-6666-7666-8666-666666666660'
$ActiveId = '66666666-6666-7666-8666-666666666661'
$DoneId = '66666666-6666-7666-8666-666666666662'
$HomeDir = Join-Path $WorkDirectory 'codex-home'
$SessionDir = Join-Path $HomeDir 'sessions\2026\10\30'
$OutputDir = Join-Path $WorkDirectory 'handoffs'
$ReceiptPath = Join-Path $LogDirectory 'R9-SUBAGENT-E2E-RESULT.json'
$OldCodexHome = [Environment]::GetEnvironmentVariable('CODEX_HOME','Process')
function Check([bool]$condition,[string]$reason) {
    if (-not $condition) { throw "R9 SUBAGENT E2E FAILED: $reason" }
}
function Record([object]$row) {
    return (($row | ConvertTo-Json -Depth 24 -Compress) + "`n")
}
function WriteUtf8([string]$path,[string]$content) {
    [IO.File]::WriteAllText($path,$content,$Utf8)
}
function HashFile([string]$path) {
    $stream=[IO.File]::OpenRead($path)
    try {
        $sha=[Security.Cryptography.SHA256]::Create()
        try { return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-','').ToLowerInvariant() }
        finally { $sha.Dispose() }
    } finally { $stream.Dispose() }
}
function Row([string]$time,[string]$type,[object]$payload) {
    return Record ([ordered]@{timestamp=$time;type=$type;payload=$payload})
}
function Meta([string]$id,[string]$time,[string]$role,[string]$name) {
    $payload = [ordered]@{
        id=$id;session_id=$id;timestamp=$time;cwd=$Cwd;
        originator='Codex Desktop';model_provider='openai';thread_source=$role;
        cli_version='0.160.0-synthetic'
    }
    if ($role -eq 'subagent') {
        $payload.parent_thread_id=$RootId
        $payload.agent_nickname=$name
        $payload.agent_path='/root/' + $name
        $payload.source=@{subagent=@{thread_spawn=@{
            parent_thread_id=$RootId;depth=1;agent_path=('/root/' + $name);
            agent_nickname=$name
        }}}
    } else { $payload.source='vscode' }
    return Row $time 'session_meta' $payload
}
function StartTask([string]$time,[string]$turnId) {
    return Row $time 'event_msg' ([ordered]@{type='task_started';turn_id=$turnId})
}
function Delegation([string]$time) {
    return Row $time 'response_item' ([ordered]@{
        type='message';role='developer';
        content=@(@{type='input_text';text='<multi_agent_role>You are an agent in a team of agents collaborating to complete a task.'})
    })
}
function AgentCall([string]$time,[string]$callId,[string]$name) {
    return Row $time 'response_item' ([ordered]@{
        type='function_call';name='shell_command';call_id=$callId;
        arguments=(('{"command":"echo ' + $name + '"}'))
    })
}
function AgentOutput([string]$time,[string]$callId,[string]$name) {
    return Row $time 'response_item' ([ordered]@{
        type='function_call_output';call_id=$callId;
        output=("Process exited with code 0`nFinal output:`n$name`n")
    })
}
try {
    Check (Test-Path -LiteralPath $Executable -PathType Leaf) 'newly built EXE exists'
    foreach ($dir in @($SessionDir,(Join-Path $HomeDir 'attachments'),$OutputDir,$LogDirectory)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }
    $state=[ordered]@{
        'project-order'=@($ProjectId);
        'local-projects'=@{};
        'projectless-thread-ids'=@();
        'thread-project-assignments'=@{}
    }
    $state['local-projects'][$ProjectId]=@{id=$ProjectId;name='Synthetic R9 project';rootPaths=@($Cwd)}
    $state['thread-project-assignments'][$RootId]=@{projectKind='local';projectId=$ProjectId}
    WriteUtf8 (Join-Path $HomeDir '.codex-global-state.json') (Record $state)
    WriteUtf8 (Join-Path $HomeDir 'session_index.jsonl') (Record ([ordered]@{id=$RootId;thread_name='R9 isolated acceptance'}))

    $root = Meta $RootId '2026-10-30T03:09:00Z' 'user' ''
    $root += StartTask '2026-10-30T03:09:01Z' 'r9-root-turn'
    $root += Row '2026-10-30T03:09:02Z' 'event_msg' ([ordered]@{type='user_message';message='R9_ROOT_MARKER'})
    $root += Row '2026-10-30T03:09:03Z' 'response_item' ([ordered]@{
        type='message';role='assistant';phase='commentary';
        content=@(@{type='output_text';text='R9_ROOT_ANSWER'})
    })
    $active=Meta $ActiveId '2026-10-30T03:10:15Z' 'subagent' 'active'
    $active+=StartTask '2026-10-30T03:10:16Z' 'r9-active-turn'
    $active+=Delegation '2026-10-30T03:10:17Z'
    $active+=Row '2026-10-30T03:10:18Z' 'response_item' ([ordered]@{
        type='message';role='user';content=@(@{type='input_text';text='R9_INHERITED_BOOTSTRAP_NOT_CHILD_SPEECH'})
    })
    $active+=Row '2026-10-30T03:10:19Z' 'turn_context' ([ordered]@{turn_id='r9-active-turn';model='gpt-synthetic'})
    $active+=AgentCall '2026-10-30T03:10:20Z' 'r9-active-call' 'R9_ACTIVE_TOOL'
    $active+=AgentOutput '2026-10-30T03:10:21Z' 'r9-active-call' 'R9_ACTIVE_TOOL'
    $done=Meta $DoneId '2026-10-30T03:09:57Z' 'subagent' 'done'
    $done+=StartTask '2026-10-30T03:09:58Z' 'r9-done-turn'
    $done+=Delegation '2026-10-30T03:09:59Z'
    $done+=AgentCall '2026-10-30T03:10:00Z' 'r9-done-call' 'R9_DONE_TOOL'
    $done+=AgentOutput '2026-10-30T03:10:01Z' 'r9-done-call' 'R9_DONE_TOOL'
    $done+=Row '2026-10-30T03:10:02Z' 'response_item' ([ordered]@{
        type='message';role='assistant';phase='final_answer';
        content=@(@{type='output_text';text='R9_CHILD_FINAL_ANSWER'})
    })
    $done+=Row '2026-10-30T03:10:03Z' 'event_msg' ([ordered]@{type='task_complete';turn_id='r9-done-turn'})
    $files = @(
        @{name="rollout-2026-10-30T03-09-00-$RootId.jsonl";data=$root},
        @{name="rollout-2026-10-30T03-09-57-$DoneId.jsonl";data=$done},
        @{name="rollout-2026-10-30T03-10-15-$ActiveId.jsonl";data=$active}
    )
    $sourceHashes=@{}
    foreach ($f in $files) {
        $path=Join-Path $SessionDir $f.name
        WriteUtf8 $path $f.data
        $sourceHashes[$f.name]=HashFile $path
    }
    [Environment]::SetEnvironmentVariable('CODEX_HOME',$HomeDir,'Process')
    $stdout=Join-Path $LogDirectory 'R9-subagent-exe.stdout.txt'
    $stderr=Join-Path $LogDirectory 'R9-subagent-exe.stderr.txt'
    $oldPreference=$ErrorActionPreference
    $ErrorActionPreference='Continue'
    try {
        & $Executable --project-id $ProjectId --output-directory $OutputDir --yes 1> $stdout 2> $stderr
        $exit=[int]$LASTEXITCODE
    } finally { $ErrorActionPreference=$oldPreference }
    Check ($exit -eq 0) "R9 synthetic project export exits 0; actual=$exit; logs=$stdout $stderr"
    $raw=[IO.File]::ReadAllText($stdout)
    $receipts=[regex]::Matches($raw,'(?m)^[ \t]*V2_RESULT[ \t]+(\{[^\r\n]+\})[ \t]*\r?$')
    Check ($receipts.Count -eq 1) 'one ASCII V2_RESULT receipt exists'
    $receipt=$receipts[0].Groups[1].Value | ConvertFrom-Json
    Check ([string]$receipt.status -eq 'PASS') 'EXE reports successful export'
    Check ([int]$receipt.metrics.scan_errors -eq 0 -and [int]$receipt.metrics.export_errors -eq 0) 'no scan/export errors'
    Check ([int]$receipt.metrics.root_conversations -eq 1) 'one selected root'
    $handoffs=@(Get-ChildItem -LiteralPath $OutputDir -File -Filter '*.handoff.json')
    Check ($handoffs.Count -eq 1) 'one resulting handoff'
    $handoff=[IO.File]::ReadAllText($handoffs[0].FullName) | ConvertFrom-Json
    Check ([string]$handoff.conversation_id -eq $RootId) 'handoff root identity matches'
    $children=@($handoff.subagents)
    Check ($children.Count -eq 2) 'two real synthetic subagent objects retained'
    $activeChild=@($children | Where-Object {$_.session_id -eq $ActiveId})
    $doneChild=@($children | Where-Object {$_.session_id -eq $DoneId})
    Check ($activeChild.Count -eq 1 -and $doneChild.Count -eq 1) 'both child session IDs are linked'
    Check (@($activeChild[0].messages).Count -eq 0) 'active child has no fabricated answer or inherited user message'
    Check (@($activeChild[0].activities).Count -gt 0) 'active child tool activity is retained'
    Check ([string]$activeChild[0].session_state -eq 'active_at_export') 'unfinished child is marked active'
    Check (@($doneChild[0].messages).Count -eq 1) 'completed child has one assistant reply'
    Check ([string]$doneChild[0].messages[0].content -match 'R9_CHILD_FINAL_ANSWER') 'completed child answer is retained'
    Check ([string]$doneChild[0].session_state -eq 'idle') 'completed child is marked idle'
    foreach ($f in $files) {
        $path=Join-Path $SessionDir $f.name
        Check ((HashFile $path) -eq $sourceHashes[$f.name]) "unmodified source: $($f.name)"
    }
    $result=[ordered]@{
        status='PASS';root_id=$RootId;child_count=$children.Count;
        active_child_id=$ActiveId;done_child_id=$DoneId;
        exe_sha256=(HashFile $Executable);source_unchanged=$true
    }
    WriteUtf8 $ReceiptPath (Record $result)
} finally {
    [Environment]::SetEnvironmentVariable('CODEX_HOME',$OldCodexHome,'Process')
}
