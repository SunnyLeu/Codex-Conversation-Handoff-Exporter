#requires -Version 5.1
<#
Deterministic, private-data-free Windows end-to-end qualification.
Uses an isolated CODEX_HOME and runs the newly built EXE in product mode.
Never reads/writes the real user's Codex data or real Handoff directory.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [Parameter(Mandatory=$true)][string]$WorkDirectory,
    [Parameter(Mandatory=$true)][string]$LogDirectory
)
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$Utf8 = [System.Text.UTF8Encoding]::new($false)
$RootA = '11111111-1111-7111-8111-111111111111'
$PhysA2 = '11111111-1111-7111-8111-222222222222'
$PhysA3 = '11111111-1111-7111-8111-333333333333'
$RootB = '22222222-2222-7222-8222-222222222222'
$PhysB2 = '22222222-2222-7222-8222-333333333333'
$ProjectC = '33333333-3333-7333-8333-333333333333'
$ProjectD = '44444444-4444-7444-8444-444444444444'
$Other = '55555555-5555-7555-8555-555555555555'
$RootReply = '66666666-6666-7666-8666-666666666666'
$Cwd = 'C:\synthetic-codex-qualification'
$runLog = Join-Path $LogDirectory 'end-to-end-cases.txt'
New-Item -ItemType Directory -Path $WorkDirectory,$LogDirectory -Force | Out-Null
function Record([object]$item) {
    return (($item | ConvertTo-Json -Depth 24 -Compress) + "`n")
}
function WriteUtf8([string]$path,[string]$value) {
    [System.IO.File]::WriteAllText($path, $value, $Utf8)
}
function Check([bool]$cond,[string]$message) {
    if (-not $cond) { throw "SYNTHETIC QUALIFICATION FAILED: $message" }
    Add-Content -LiteralPath $runLog -Encoding UTF8 -Value "PASS: $message"
}
function Meta([string]$id,[int]$ordinal,[string]$timestamp,[object]$historyBase) {
    $payload = [ordered]@{
        id = $id; session_id = $id; timestamp = $timestamp; cwd = $Cwd;
        originator = 'Codex Desktop'; source = 'vscode';
        thread_source = 'user'; model_provider = 'openai';
        history_mode = 'paginated'; cli_version = '0.163.0-alpha.11.2'
    }
    if ($null -ne $historyBase) { $payload.history_base = $historyBase }
    return Record ([ordered]@{ timestamp=$timestamp; ordinal=$ordinal; type='session_meta'; payload=$payload })
}
function UserRow([int]$ordinal,[string]$text,[string]$timestamp) {
    return Record ([ordered]@{ timestamp=$timestamp; ordinal=$ordinal; type='event_msg'; payload=[ordered]@{type='user_message';message=$text} })
}
function WriteRollout([string]$directory,[string]$basename,[string]$text) {
    WriteUtf8 (Join-Path $directory $basename) $text
}
# Product mode's stable machine-readable receipt is V2_RESULT JSON.
# Informational lines such as the developer-mode `Verified history_base ...`
# are intentionally absent from the normal UI. Inspect the structured
# receipt for control-flow assertions. The UTF-8 capture below separately
# verifies that localized errors remain human-readable on stderr.
function ReadExportResult([string]$stdoutFile) {
    $raw = [System.IO.File]::ReadAllText($stdoutFile)
    $receipts = [regex]::Matches($raw, '(?m)^[ \t]*V2_RESULT[ \t]+(\{[^\r\n]+\})[ \t]*\r?$')
    Check ($receipts.Count -eq 1) "exactly one V2_RESULT JSON receipt in $(Split-Path -Leaf $stdoutFile)"
    try {
        return ($receipts[0].Groups[1].Value | ConvertFrom-Json -ErrorAction Stop)
    } catch {
        throw "Invalid V2_RESULT JSON in $(Split-Path -Leaf $stdoutFile): $($_.Exception.Message)"
    }
}
# Windows PowerShell 5.1's native-command '2>' redirection converts UTF-8
# stderr through its legacy encoding and can corrupt Traditional Chinese text.
# Capture the real product EXE's UTF-8 pipes directly, retaining accurate
# machine receipts and full human-readable failure diagnostics in log files.
# Command-line quoting follows Windows backslash/quote argument rules.
function QuoteWindowsArgument([string]$value) {
    if ($value.Length -gt 0 -and $value -notmatch '[\s\"]') {
        return $value
    }
    $buffer = [System.Text.StringBuilder]::new()
    [void]$buffer.Append('"')
    $slashes = 0
    foreach ($character in $value.ToCharArray()) {
        if ($character -eq '\') {
            $slashes++
            continue
        }
        if ($character -eq '"') {
            [void]$buffer.Append(('\' * ($slashes * 2 + 1)))
            [void]$buffer.Append('"')
            $slashes = 0
            continue
        }
        if ($slashes -gt 0) {
            [void]$buffer.Append(('\' * $slashes))
            $slashes = 0
        }
        [void]$buffer.Append($character)
    }
    if ($slashes -gt 0) {
        [void]$buffer.Append(('\' * ($slashes * 2)))
    }
    [void]$buffer.Append('"')
    return $buffer.ToString()
}
function RunExport([string]$case,[string[]]$parameters,[bool]$expectSuccess,[int]$expectedRootCount=1) {
    $stdout = Join-Path $LogDirectory ("$case-stdout.txt")
    $stderr = Join-Path $LogDirectory ("$case-stderr.txt")
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.Arguments = [string]::Join(' ', @($parameters | ForEach-Object { QuoteWindowsArgument ([string]$_) }))
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = $Utf8
    $startInfo.StandardErrorEncoding = $Utf8
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        Check ($process.Start()) "$case started the actual Windows EXE"
        # Read both streams asynchronously to avoid pipe-buffer deadlocks.
        $outTask = $process.StandardOutput.ReadToEndAsync()
        $errTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $exit = [int]$process.ExitCode
        [IO.File]::WriteAllText($stdout, $outTask.GetAwaiter().GetResult(), $Utf8)
        [IO.File]::WriteAllText($stderr, $errTask.GetAwaiter().GetResult(), $Utf8)
    } finally {
        $process.Dispose()
    }
    $receipt = ReadExportResult $stdout
    if ($expectSuccess) {
        Check ($exit -eq 0) "$case exits 0 (actual $exit)"
        Check ([string]$receipt.status -eq 'PASS') "$case reports structured V2_RESULT PASS"
        Check ([int]$receipt.metrics.root_conversations -eq $expectedRootCount) "$case selected expected root conversation count"
        Check ([int]$receipt.metrics.outputs_rewritten -eq $expectedRootCount) "$case wrote expected output count"
        Check ([int]$receipt.metrics.scan_errors -eq 0 -and [int]$receipt.metrics.export_errors -eq 0) "$case has no scan/export errors"
        if ($case -ne '05-damaged-foreign-coexists') {
            Check (-not ([IO.File]::ReadAllText($stderr)).Contains('[DEBUG]')) "$case does not save a DEBUG log on error-free success"
        }
    } else {
        Check ($exit -ne 0) "$case safely fails with non-zero exit"
        Check ([string]$receipt.status -eq 'FAIL') "$case reports structured V2_RESULT FAIL"
        Check ([int]$receipt.completed_writes -eq 0) "$case fails before writing Handoffs"
        Check (-not [string]::IsNullOrWhiteSpace([string]$receipt.error)) "$case reports a detailed failure reason"
        Check (-not [string]::IsNullOrWhiteSpace([string]$receipt.error_code)) "$case emits an actionable error code"
        $debugFile = [string]$receipt.diagnostic_log
        Check (-not [string]::IsNullOrWhiteSpace($debugFile) -and (Test-Path -LiteralPath $debugFile -PathType Leaf)) "$case creates a DEBUG diagnostic file"
        Copy-Item -LiteralPath $debugFile -Destination (Join-Path $LogDirectory ("$case-debug.jsonl")) -Force
        $debugEvents = @(Get-Content -LiteralPath $debugFile -Encoding UTF8 | ForEach-Object { $_ | ConvertFrom-Json -ErrorAction Stop })
        Check ($debugEvents.Count -ge 3) "$case DEBUG diagnostic file contains parseable JSONL entries"
        Check (@($debugEvents | Where-Object { $_.stage -eq 'process.finish' -and $_.details.status -eq 'FAIL' }).Count -eq 1) "$case DEBUG file records FAIL termination"
        $stderrText = [IO.File]::ReadAllText($stderr, $Utf8)
        Check ($stderrText.Contains('[匯出失敗]') -and
            $stderrText.Contains('[DEBUG]') -and
            $stderrText.Contains([string]$receipt.error_code) -and
            $stderrText.Contains($debugFile)) "$case UTF-8 stderr visibly reports the full localized error, error code and exact DEBUG path"
    }
    return ($stdout, $stderr)
}
# Portable .NET SHA-256: available in Windows PowerShell 5.1 without the
# optional Get-FileHash command/module. Keep the stream lifetime explicit.
function HashFile([string]$path) {
    $stream = [System.IO.File]::OpenRead($path)
    try {
        $algorithm = [System.Security.Cryptography.SHA256]::Create()
        try {
            $digest = $algorithm.ComputeHash($stream)
            return [System.BitConverter]::ToString($digest).Replace('-','').ToLowerInvariant()
        } finally {
            $algorithm.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}
function Hash([string]$path) { return (HashFile $path) }
function ListHandoffs([string]$path) { return @(Get-ChildItem -LiteralPath $path -File | Where-Object { $_.Name.EndsWith('.handoff.json', [StringComparison]::OrdinalIgnoreCase) }) }
function FindHandoff([string]$directory,[string]$id) {
    $foundHandoffs = @(ListHandoffs $directory | Where-Object {
        $parsed = Get-Content -LiteralPath $_.FullName -Raw -Encoding UTF8 | ConvertFrom-Json
        $parsed.conversation_id -eq $id
    })
    Check ($foundHandoffs.Count -eq 1) "Exactly one output for $id"
    return $foundHandoffs[0].FullName
}
$originalCodeHome = [Environment]::GetEnvironmentVariable('CODEX_HOME','Process')
try {
    Check (Test-Path -LiteralPath $Executable -PathType Leaf) 'EXE exists'
    $SyntheticCodexHome = Join-Path $WorkDirectory 'codex-home'
    $sessions = Join-Path $SyntheticCodexHome 'sessions\2026\10\23'
    $attachments = Join-Path $SyntheticCodexHome 'attachments'
    $out = Join-Path $WorkDirectory 'handoffs'
    foreach ($path in @($SyntheticCodexHome,$sessions,$attachments,$out)) { New-Item -ItemType Directory -Force -Path $path | Out-Null }
    $state = [ordered]@{
        'project-order'=@('qualified-project'); 'pinned-project-ids'=@();
        'local-projects'=@{ 'qualified-project' = @{id='qualified-project';name='Qualification Project';rootPaths=@($Cwd)} };
        'projectless-thread-ids'=@($RootA,$RootB,$RootReply);
        'thread-project-assignments'=@{}
    }
    $state['thread-project-assignments'][$ProjectC] = @{projectKind='local';projectId='qualified-project'}
    $state['thread-project-assignments'][$ProjectD] = @{projectKind='local';projectId='qualified-project'}
    WriteUtf8 (Join-Path $SyntheticCodexHome '.codex-global-state.json') (Record $state)
    $indexText = ''
    $indexItems = @(
        @{id=$RootA;title='Three page fixture'},
        @{id=$RootB;title='Two page fixture'},
        @{id=$ProjectC;title='Project C'},
        @{id=$ProjectD;title='Project D'},
        @{id=$RootReply;title='Async question reply fixture'}
    )
    foreach ($item in $indexItems) {
        $indexText += (Record ([ordered]@{id=$item.id;thread_name=$item.title}))
    }
    WriteUtf8 (Join-Path $SyntheticCodexHome 'session_index.jsonl') $indexText
    # Three physical pages. The second history_base offset is into page 2,
    # never the already assembled history. Both superseded tails are discarded.
    $r0 = Meta $RootA 0 '2026-10-23T10:05:55Z' $null
    $r0 += UserRow 1 'QUAL_ROOT_KEEP' '2026-10-23T10:05:56Z'
    $r0 += UserRow 2 'QUAL_ROOT_KEEP2' '2026-10-23T10:05:57Z'
    $offsetA = $Utf8.GetByteCount($r0)
    $r0 += UserRow 3 'QUAL_ROOT_DISCARD' '2026-10-23T10:05:58Z'
    $r1 = Meta $RootA 3 '2026-10-23T10:31:40Z' ([ordered]@{thread_id=$RootA;end_ordinal_exclusive=3;end_byte_offset=$offsetA})
    $r1 += UserRow 4 'QUAL_MIDDLE_KEEP' '2026-10-23T10:31:41Z'
    $offsetMiddle = $Utf8.GetByteCount($r1)
    $r1 += UserRow 5 'QUAL_MIDDLE_DISCARD' '2026-10-23T10:31:42Z'
    $r2 = Meta $RootA 5 '2026-10-23T10:31:55Z' ([ordered]@{thread_id=$PhysA2;end_ordinal_exclusive=5;end_byte_offset=$offsetMiddle})
    $r2 += UserRow 6 'QUAL_LATEST_KEEP' '2026-10-23T10:31:56Z'
    WriteRollout $sessions "rollout-2026-10-23T10-05-55-$RootA.jsonl" $r0
    WriteRollout $sessions "rollout-2026-10-23T10-31-40-${RootA}_${PhysA2}.jsonl" $r1
    WriteRollout $sessions "rollout-2026-10-23T10-31-55-${RootA}_${PhysA3}.jsonl" $r2
    # Independent two-page lineage provides the previously accepted regression.
    $b0 = Meta $RootB 0 '2026-10-23T09:05:55Z' $null
    $b0 += UserRow 1 'QUAL_TWO_ROOT' '2026-10-23T09:05:56Z'
    $bCut = $Utf8.GetByteCount($b0)
    $b0 += UserRow 2 'QUAL_TWO_DISCARD' '2026-10-23T09:05:57Z'
    $b1 = Meta $RootB 2 '2026-10-23T09:07:55Z' ([ordered]@{thread_id=$RootB;end_ordinal_exclusive=2;end_byte_offset=$bCut})
    $b1 += UserRow 3 'QUAL_TWO_LATEST' '2026-10-23T09:07:56Z'
    WriteRollout $sessions "rollout-2026-10-23T09-05-55-$RootB.jsonl" $b0
    WriteRollout $sessions "rollout-2026-10-23T09-07-55-${RootB}_${PhysB2}.jsonl" $b1
    foreach ($item in @(@{id=$ProjectC;marker='QUAL_PROJECT_C'}, @{id=$ProjectD;marker='QUAL_PROJECT_D'})) {
        $id = $item.id
        $body = Meta $id 0 '2026-10-23T08:00:00Z' $null
        $body += UserRow 1 $item.marker '2026-10-23T08:00:01Z'
        WriteRollout $sessions "rollout-2026-10-23T08-00-00-$id.jsonl" $body
    }
    # Codex Desktop emits BOTH a response_item user message and a verified
    # item_completed.UserMessage. A delivery acknowledgment is not the answer.
    $replyTurn = '66666666-6666-7666-8666-777777777777'
    $replyCall = 'call-async-qualification'
    $pendingCall = 'call-async-pending'
    $questions = @([ordered]@{title='第一題？';options=@('核准','拒絕')},
                   [ordered]@{title='第二題？';options=@('版本二','版本三')})
    $replyEntries = @(
        [ordered]@{questionItemId=(ConvertTo-Json -InputObject @('request_user_input_async',$replyCall,0) -Compress);question='第一題？';answer='核准'},
        [ordered]@{questionItemId=(ConvertTo-Json -InputObject @('request_user_input_async',$replyCall,1) -Compress);question='第二題？';answer='版本二'}
    )
    $wrappedReply = "<send_user_message_question_reply>`n" +
        (ConvertTo-Json -InputObject $replyEntries -Depth 24 -Compress) +
        "`n</send_user_message_question_reply>"
    $replyRollout = Meta $RootReply 0 '2026-10-23T12:00:00Z' $null
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:01Z';ordinal=1;type='event_msg';payload=[ordered]@{type='task_started';turn_id=$replyTurn}})
    $replyRollout += UserRow 2 '提出兩個問題' '2026-10-23T12:00:02Z'
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:03Z';ordinal=3;type='response_item';payload=[ordered]@{type='function_call';name='request_user_input_async';call_id=$replyCall;arguments=(ConvertTo-Json -InputObject @{questions=$questions} -Depth 24 -Compress);internal_chat_message_metadata_passthrough=@{turn_id=$replyTurn}}})
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:04Z';ordinal=4;type='response_item';payload=[ordered]@{type='function_call_output';call_id=$replyCall;output='{"accepted":true}'}})
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:05Z';ordinal=5;type='response_item';payload=[ordered]@{type='function_call';name='request_user_input_async';call_id=$pendingCall;arguments=(ConvertTo-Json -InputObject @{questions=@(@{title='尚未回答？';options=@('是','否')})} -Depth 24 -Compress);internal_chat_message_metadata_passthrough=@{turn_id=$replyTurn}}})
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:06Z';ordinal=6;type='response_item';payload=[ordered]@{type='function_call_output';call_id=$pendingCall;output='{"accepted":true}'}})
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:07Z';ordinal=7;type='response_item';payload=[ordered]@{type='message';role='user';content=@(@{type='input_text';text=$wrappedReply});internal_chat_message_metadata_passthrough=@{turn_id=$replyTurn}}})
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:07.010Z';ordinal=8;type='event_msg';payload=[ordered]@{type='item_completed';thread_id=$RootReply;turn_id=$replyTurn;item=[ordered]@{type='UserMessage';id='verified-async-user-message';client_id='qualified-async-client';content=@(@{type='text';text=$wrappedReply;text_elements=@()})}}})
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:08Z';ordinal=9;type='response_item';payload=[ordered]@{type='message';role='assistant';phase='final_answer';content=@(@{type='output_text';text='已收到'})}})
    $replyRollout += Record ([ordered]@{timestamp='2026-10-23T12:00:09Z';ordinal=10;type='event_msg';payload=[ordered]@{type='task_complete';turn_id=$replyTurn}})
    WriteRollout $sessions "rollout-2026-10-23T12-00-00-$RootReply.jsonl" $replyRollout
    $sourceHashes = @{}
    foreach ($f in @(Get-ChildItem -LiteralPath $sessions -File -Filter '*.jsonl')) { $sourceHashes[$f.Name] = Hash $f.FullName }
    $foreign = Join-Path $out 'unrelated.handoff.json'
    WriteUtf8 $foreign (Record @{conversation_id=$Other;title='existing valid foreign handoff'})
    $originalForeign = Hash $foreign
    [Environment]::SetEnvironmentVariable('CODEX_HOME',$SyntheticCodexHome,'Process')
    $argsA = @('--thread-id',$RootA,'--output-directory',$out,'--yes')
    $logs = RunExport '01-three-page' $argsA $true
    $chainResult = ReadExportResult $logs[0]
    Check ([string]$chainResult.kind -eq 'independent-thread' -and [string]$chainResult.target_id -eq $RootA) 'three-page targets the intended independent thread'
    Check ([int]$chainResult.metrics.continuations_merged -eq 1) 'three-page lineage reconstructed by actual Rust exporter'
    Check ((ListHandoffs $out).Count -eq 2) 'single export coexists with another target'
    Check ((Hash $foreign) -eq $originalForeign) 'foreign Handoff bytes unchanged by three-page export'
    $aPath = FindHandoff $out $RootA
    $aText = [IO.File]::ReadAllText($aPath)
    foreach ($token in @('QUAL_ROOT_KEEP','QUAL_ROOT_KEEP2','QUAL_MIDDLE_KEEP','QUAL_LATEST_KEEP')) {
        Check ($aText.Contains($token)) "retained three-page marker $token"
    }
    foreach ($token in @('QUAL_ROOT_DISCARD','QUAL_MIDDLE_DISCARD')) {
        Check (-not $aText.Contains($token)) "discarded abandoned tail $token"
    }
    $ownedBytes = [IO.File]::ReadAllBytes($aPath)
    $null = RunExport '02-repeat-one-handoff' $argsA $true
    Check ((ListHandoffs $out).Count -eq 2) 'repeat export rewrites one Handoff without deleting foreign'
    Check ((Hash $foreign) -eq $originalForeign) 'foreign Handoff bytes unchanged on repeat export'
    $argsB = @('--thread-id',$RootB,'--output-directory',$out,'--yes')
    $logB = RunExport '03-two-page' $argsB $true
    $twoPageResult = ReadExportResult $logB[0]
    Check ([string]$twoPageResult.kind -eq 'independent-thread' -and [string]$twoPageResult.target_id -eq $RootB) 'two-page targets the intended independent thread'
    Check ([int]$twoPageResult.metrics.continuations_merged -eq 1) 'two-page lineage reconstructed by actual Rust exporter'
    Check ((ListHandoffs $out).Count -eq 3) 'second independent Handoff can coexist'
    $bPath = FindHandoff $out $RootB
    $bText = [IO.File]::ReadAllText($bPath)
    Check ($bText.Contains('QUAL_TWO_ROOT') -and $bText.Contains('QUAL_TWO_LATEST')) 'two-page retained markers'
    Check (-not $bText.Contains('QUAL_TWO_DISCARD')) 'two-page abandoned tail excluded'
    # Same prospective output filename is occupied by another conversation.
    WriteUtf8 $aPath (Record @{conversation_id=$Other;title='foreign collision'})
    $collisionHash = Hash $aPath
    $failedCollision = RunExport '04-name-collision-fails' $argsA $false
    $collisionResult = ReadExportResult $failedCollision[0]
    Check ([string]$collisionResult.error -like (('*' + $Other + '*'))) 'collision error identifies the foreign conversation occupying the planned filename'
    Check ($collisionResult.error_code -eq 'DESTINATION_NAME_COLLISION') 'same filename with different ID has precise failure code'
    $collisionTrace = @((Get-Content -LiteralPath (Join-Path $LogDirectory '04-name-collision-fails-debug.jsonl') -Encoding UTF8) | ForEach-Object { $_ | ConvertFrom-Json })
    Check (@($collisionTrace | Where-Object { $_.stage -eq 'destination.planned.ownership' -and $_.details.accepted -eq $false }).Count -eq 1) 'filename conflict DEBUG trace includes existing and incoming IDs'
    Check ((Hash $aPath) -eq $collisionHash) 'name collision does not modify existing bytes'
    Check ((Hash $foreign) -eq $originalForeign) 'foreign Handoff unaffected by collision'
    [IO.File]::WriteAllBytes($aPath,$ownedBytes)
    $bad = Join-Path $out 'damaged.handoff.json'
    WriteUtf8 $bad '{not-valid-json'
    $badHash = Hash $bad
    # A malformed Handoff at an unrelated path is never a planned write or
    # cleanup target, so it must not block otherwise safe exports. It does
    # cause a handled index error, which MUST leave a full DEBUG trace.
    $damagedLog = RunExport '05-damaged-foreign-coexists' $argsA $true
    Check ((Hash $bad) -eq $badHash) 'unrelated malformed Handoff preserved byte-for-byte'
    Check ((Hash $foreign) -eq $originalForeign) 'unrelated valid Handoff preserved by malformed-foreign export'
    Check ((ListHandoffs $out).Count -eq 4) 'existing owned and unrelated Handoffs coexist with malformed file'
    # The exporter rewrites its OWN Handoff on each run. Its source_observations
    # contains the continuation merge TEMP path with the current process ID.
    # Thus byte-for-byte identity of the OWN file across separate processes is
    # not a valid correctness test; verify identity and logical content instead.
    $reExportText = [IO.File]::ReadAllText($aPath, $Utf8)
    $reExport = $reExportText | ConvertFrom-Json -ErrorAction Stop
    Check ([string]$reExport.conversation_id -eq $RootA -and [string]$reExport.exporter_version -eq '2.3.1') 'repeat export retains correct owned conversation ID and candidate version'
    Check (@($reExport.source_snapshot.physical_files).Count -eq 3) 'repeat export retains all three physical continuation source snapshots'
    foreach ($token in @('QUAL_ROOT_KEEP','QUAL_ROOT_KEEP2','QUAL_MIDDLE_KEEP','QUAL_LATEST_KEEP')) {
        Check ($reExportText.Contains($token)) "repeat export retains expected message $token"
    }
    foreach ($token in @('QUAL_ROOT_DISCARD','QUAL_MIDDLE_DISCARD')) {
        Check (-not $reExportText.Contains($token)) "repeat export excludes abandoned message $token"
    }
    # R2 did not copy the DEBUG log for this recovered error into DIAGNOSTICS;
    # make it independently reviewable, including the cause and PASS finish.
    $handledStderr = [IO.File]::ReadAllText($damagedLog[1], $Utf8)
    $handledPathMatch = [regex]::Match($handledStderr, '[A-Za-z]:\\[^\r\n]*?\.debug\.jsonl')
    Check ($handledStderr.Contains('[DEBUG]') -and $handledPathMatch.Success) 'recovered malformed-foreign error clearly reports its DEBUG path'
    $handledPath = $handledPathMatch.Value
    Check (Test-Path -LiteralPath $handledPath -PathType Leaf) 'recovered malformed-foreign DEBUG log exists on disk'
    Copy-Item -LiteralPath $handledPath -Destination (Join-Path $LogDirectory '05-damaged-foreign-coexists-debug.jsonl') -Force
    $handledEvents = @((Get-Content -LiteralPath $handledPath -Encoding UTF8) | ForEach-Object { $_ | ConvertFrom-Json -ErrorAction Stop })
    Check (@($handledEvents | Where-Object { $_.stage -eq 'destination.existing.index_error' -and $_.level -eq 'ERROR' -and $_.details.error_code -eq 'DESTINATION_EXISTING_INDEX_ERROR' }).Count -ge 1) 'recovered malformed-foreign index error retains its full structured DEBUG event'
    Check (@($handledEvents | Where-Object { $_.stage -eq 'process.finish' -and $_.details.status -eq 'PASS' }).Count -eq 1) 'recovered error trace records the final successful export'
    Remove-Item -LiteralPath $bad -Force
    # A multi-root project may share a folder with unrelated independent
    # Handoffs as long as planned paths do not collide.
    $beforeBatchA = Hash $aPath
    $beforeBatchB = Hash $bPath
    $batchLog = RunExport '06-multi-root-coexists' @('--project-id','qualified-project','--output-directory',$out,'--yes') $true 2
    Check ((ListHandoffs $out).Count -eq 5) 'two project outputs coexist with independent and foreign Handoffs'
    Check ((Hash $foreign) -eq $originalForeign) 'multi-root export preserves unrelated Handoff'
    Check ((Hash $aPath) -eq $beforeBatchA) 'multi-root export preserves unrelated first independent Handoff'
    Check ((Hash $bPath) -eq $beforeBatchB) 'multi-root export preserves unrelated second independent Handoff'
    # Tamper with the last page's physical offset and ensure no pre-existing
    # output is changed by the rejected history_base chain.
    $lastRollout = Join-Path $sessions "rollout-2026-10-23T10-31-55-${RootA}_${PhysA3}.jsonl"
    $lastOriginal = [IO.File]::ReadAllBytes($lastRollout)
    $badR2 = Meta $RootA 5 '2026-10-23T10:31:55Z' ([ordered]@{thread_id=$PhysA2;end_ordinal_exclusive=5;end_byte_offset=($offsetMiddle+1)})
    $badR2 += UserRow 6 'QUAL_LATEST_KEEP' '2026-10-23T10:31:56Z'
    WriteUtf8 $lastRollout $badR2
    $beforeBadLineage = Hash $aPath
    $badLineage = RunExport '07-invalid-physical-offset' $argsA $false
    $badLineageResult = ReadExportResult $badLineage[0]
    # A valid baseline export previously passed; corrupting only its offset
    # must reject before touching existing output. Error prose is not an API.
    Check ((Hash $aPath) -eq $beforeBadLineage) 'bad lineage causes no Handoff overwrite'
    [IO.File]::WriteAllBytes($lastRollout,$lastOriginal)
    # E8: the actual freshly built product EXE must link a verified async
    # answer to the correct call ID/question index, not merely retain a tag.
    $replyLogs = RunExport '08-async-question-replies' @('--thread-id',$RootReply,'--output-directory',$out,'--yes') $true
    $replyPath = FindHandoff $out $RootReply
    $replyJson = Get-Content -LiteralPath $replyPath -Raw -Encoding UTF8 | ConvertFrom-Json
    Check ($replyJson.exporter_version -eq '2.3.1') 'async reply EXE reports the candidate version'
    $answered = @($replyJson.user_input_requests | Where-Object { $_.call_id -eq $replyCall })
    $pending = @($replyJson.user_input_requests | Where-Object { $_.call_id -eq $pendingCall })
    Check ($answered.Count -eq 1 -and $pending.Count -eq 1) 'both async requests retained'
    Check ($answered[0].status -eq 'answered' -and @($answered[0].responses).Count -eq 2) 'complete async reply recognized'
    Check ($answered[0].responses[0].answer -eq '核准' -and $answered[0].responses[1].answer -eq '版本二') 'question-index answer association is exact'
    Check ($answered[0].responses[0].source_item_id -eq 'verified-async-user-message') 'response retains verified source item identity'
    # Optional `responses` is absent for an unanswered request. PowerShell
    # StrictMode rejects direct access to a non-existent PSObject property.
    $pendingResponseProperties = @($pending[0].PSObject.Properties | Where-Object { $_.Name -eq 'responses' })
    $noPendingResponses = $pendingResponseProperties.Count -eq 0
    if (-not $noPendingResponses) {
        $noPendingResponses = $null -eq $pendingResponseProperties[0].Value
    }
    Check ($pending[0].status -eq 'awaiting_response' -and $noPendingResponses) 'unanswered request remains awaiting response'
    $replyMessages = @($replyJson.messages | Where-Object { $_.role -eq 'user' -and $_.content -like '*send_user_message_question_reply*' })
    Check ($replyMessages.Count -eq 1 -and $replyMessages[0].content -eq $wrappedReply) 'original tagged user message preserved exactly'
    foreach ($f in @(Get-ChildItem -LiteralPath $sessions -File -Filter '*.jsonl')) {
        Check ((Hash $f.FullName) -eq $sourceHashes[$f.Name]) "source snapshot remained untouched: $($f.Name)"
    }
    # Deleted original thread, recreated new thread with the same title and
    # different last-message timestamp: old and new Handoffs must coexist.
    $staleOut = Join-Path $WorkDirectory '09-deleted-old-thread-reused-title'
    New-Item -ItemType Directory -Path $staleOut -Force | Out-Null
    $ghost = Join-Path $staleOut '01. Project C-20260101000000.handoff.json'
    WriteUtf8 $ghost (Record @{conversation_id=$Other;title='Project C';messages=@('OLD-HANDOFF-MUST-STAY')})
    $ghostHash = Hash $ghost
    $staleArgs = @('--project-id','qualified-project','--output-directory',$staleOut,'--yes')
    $staleLog = RunExport '09-deleted-recreated-same-title-coexists' $staleArgs $true 2
    Check ((Hash $ghost) -eq $ghostHash) 'deleted old conversation export remains untouched after new project export'
    Check ((ListHandoffs $staleOut).Count -eq 3) 'two new project Handoffs coexist with the deleted old conversation Handoff'
    $oldTraceError = @([IO.File]::ReadAllText($staleLog[1]) -match '\[DEBUG\]')
    Check (-not $oldTraceError[0]) 'error-free same-title coexistence run does not save DEBUG log'
    # An actual planned final filename collision is a distinct fatal case.
    $newProjectHandoffs = @(ListHandoffs $staleOut | Where-Object { $_.FullName -ne $ghost })
    $overwritten = $newProjectHandoffs[0].FullName
    WriteUtf8 $overwritten (Record @{conversation_id=$Other;title='fake collision';messages=@('DO-NOT-TOUCH')})
    $collisionHash = Hash $overwritten
    $collisionLogs = RunExport '10-same-filename-different-id-fails' $staleArgs $false
    $collisionReceipt = ReadExportResult $collisionLogs[0]
    Check ($collisionReceipt.error_code -eq 'DESTINATION_NAME_COLLISION') 'same final filename and different ID produces exact collision error'
    Check ((Hash $overwritten) -eq $collisionHash) 'real collision does not modify or delete occupied bytes'
    Check ((Hash $ghost) -eq $ghostHash) 'unrelated deleted thread still intact after true collision'
    $collisionTrace = @((Get-Content -LiteralPath (Join-Path $LogDirectory '10-same-filename-different-id-fails-debug.jsonl') -Encoding UTF8) | ForEach-Object { $_ | ConvertFrom-Json })
    Check (@($collisionTrace | Where-Object { $_.stage -eq 'destination.planned.ownership' -and $_.details.accepted -eq $false }).Count -eq 1) 'real collision is traceable with both IDs and actual output filename'
    $safeOut = Join-Path $WorkDirectory '11-safe-alternate-destination'
    $safeArgs = @('--project-id','qualified-project','--output-directory',$safeOut,'--yes')
    $null = RunExport '11-new-empty-destination' $safeArgs $true 2
    Check ((ListHandoffs $safeOut).Count -eq 2) 'new conversations export to fresh destination independently of old Handoffs'
    # A separately isolated SQLite registry whose current root differs from legacy JSON.
    $registryHome = Join-Path $WorkDirectory 'sqlite-registry-home'
    $registrySessions = Join-Path $registryHome 'sessions\2026\10\23'
    $registryOut = Join-Path $WorkDirectory '12-sqlite-project-root-change'
    foreach ($path in @($registrySessions,(Join-Path $registryHome 'attachments'),$registryOut)) { New-Item -ItemType Directory -Force -Path $path | Out-Null }
    $registryState = [ordered]@{
        'project-order'=@('legacy-moved'); 'pinned-project-ids'=@();
        'local-projects'=@{'legacy-moved'=@{id='legacy-moved';name='Old Project';rootPaths=@('C:\synthetic-old')}};
        'app-server-project-id-by-legacy-project-id-by-host'=@{'local:fixture'=@{'legacy-moved'='app-moved'}};
        'projectless-thread-ids'=@(); 'thread-project-assignments'=@{}
    }
    $registryState['thread-project-assignments'][$RootA]=@{projectKind='local';projectId='legacy-moved'}
    WriteUtf8 (Join-Path $registryHome '.codex-global-state.json') (Record $registryState)
    $index = (Record @{id=$RootA;thread_name='Assigned legacy root'}) + (Record @{id=$RootB;thread_name='Unassigned new root'})
    WriteUtf8 (Join-Path $registryHome 'session_index.jsonl') $index
    $fixturePath = Join-Path (Split-Path -Parent $PSScriptRoot) 'fixtures\project-registry.sqlite.b64'
    [IO.File]::WriteAllBytes((Join-Path $registryHome 'state_5.sqlite'),[Convert]::FromBase64String([IO.File]::ReadAllText($fixturePath)))
    $Cwd='C:\synthetic-old'
    WriteRollout $registrySessions "rollout-2026-10-23T10-05-55-$RootA.jsonl" ((Meta $RootA 0 '2026-10-23T10:05:55Z' $null)+(UserRow 1 'QUAL_LEGACY_ASSIGNED' '2026-10-23T10:05:56Z'))
    $Cwd='C:\synthetic-new'
    WriteRollout $registrySessions "rollout-2026-10-23T10-06-55-$RootB.jsonl" ((Meta $RootB 0 '2026-10-23T10:06:55Z' $null)+(UserRow 1 'QUAL_SQLITE_NEW_ROOT' '2026-10-23T10:06:56Z'))
    $registryHashes=@{}
    foreach ($f in @(Get-ChildItem -LiteralPath $registryHome -Recurse -Force -File)) { $registryHashes[$f.FullName]=Hash $f.FullName }
    [Environment]::SetEnvironmentVariable('CODEX_HOME',$registryHome,'Process')
    $listText=(& $Executable --developer-mode --list-projects --no-pause | Out-String)
    $listExit=$LASTEXITCODE
    WriteUtf8 (Join-Path $LogDirectory '12-list-projects-stdout.txt') $listText
    Check ($listExit -eq 0 -and $listText.Contains('Project ID: legacy-moved') -and $listText.Contains('Root: "C:\\synthetic-new"') -and -not $listText.Contains('Root: "C:\\synthetic-old"')) 'developer project list displays the current SQLite root, not removed JSON root'
    $null=RunExport '12-sqlite-project-root-change' @('--project-id','legacy-moved','--output-directory',$registryOut,'--yes') $true 2
    Check ((ListHandoffs $registryOut).Count -eq 2) 'SQLite root change exports exactly two roots'
    Check ([IO.File]::ReadAllText((FindHandoff $registryOut $RootA),$Utf8).Contains('QUAL_LEGACY_ASSIGNED')) 'legacy explicit assignment remains valid when SQLite project_id is NULL'
    Check ([IO.File]::ReadAllText((FindHandoff $registryOut $RootB),$Utf8).Contains('QUAL_SQLITE_NEW_ROOT')) 'new SQLite root recovers unassigned root'
    $afterFiles=@(Get-ChildItem -LiteralPath $registryHome -Recurse -Force -File)
    Check ($afterFiles.Count -eq $registryHashes.Count) 'SQLite project reads do not create source sidecar files'
    foreach ($f in $afterFiles) { Check ($registryHashes.ContainsKey($f.FullName) -and (Hash $f.FullName) -eq $registryHashes[$f.FullName]) "registry source preserved: $($f.Name)" }
    WriteUtf8 (Join-Path $LogDirectory 'SYNTHETIC-E2E-PASS.txt') 'PASS: twelve executable export scenarios plus visible project list, including SQLite root change, legacy assignment, and unchanged sources.'
} finally {
    [Environment]::SetEnvironmentVariable('CODEX_HOME',$originalCodeHome,'Process')
}
