#requires -Version 5.1
<# Uses the exact built EXE with isolated synthetic input, then checks independent
   expected values, JSON types, negative conditions, source hashes and N/N rewrite.
   No real Codex home, Git checkout, network call or account write is involved. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [Parameter(Mandatory=$true)][string]$WorkDirectory,
    [Parameter(Mandatory=$true)][string]$LogDirectory
)
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Utf8 = [Text.UTF8Encoding]::new($false)
$Cases = New-Object System.Collections.ArrayList
function Check([bool]$condition,[string]$message) {
    if (-not $condition) { throw "SCHEMA OUTPUT CHECK FAILED: $message" }
    [void]$Cases.Add($message)
}
function WriteUtf8([string]$path,[string]$text) { [IO.File]::WriteAllText($path,$text,$Utf8) }
function HashFile([string]$path) {
    $stream = [IO.File]::OpenRead($path)
    try {
        $algorithm = [Security.Cryptography.SHA256]::Create()
        try { return [BitConverter]::ToString($algorithm.ComputeHash($stream)).Replace('-','').ToLowerInvariant() }
        finally { $algorithm.Dispose() }
    } finally { $stream.Dispose() }
}
function JsonType($value) {
    if ($null -eq $value) { return 'null' }
    if ($value -is [bool]) { return 'boolean' }
    if ($value -is [string]) { return 'string' }
    if ($value -is [Array]) { return 'array' }
    if ($value -is [ValueType]) { return 'number' }
    return 'object'
}
# Each hit is wrapped: PowerShell must not flatten JSON arrays or discard null.
function SelectValues($node,[string[]]$segments,[int]$position=0) {
    if ($position -ge $segments.Length) {
        Write-Output -NoEnumerate ([pscustomobject]@{ value=$node })
        return
    }
    $key=$segments[$position].Replace('~1','/').Replace('~0','~')
    if ($key -eq '*') {
        if ($node -is [Array]) {
            foreach ($child in $node) { SelectValues $child $segments ($position+1) }
        } elseif ($null -ne $node -and (JsonType $node) -eq 'object') {
            foreach ($property in $node.PSObject.Properties) { SelectValues $property.Value $segments ($position+1) }
        }
        return
    }
    if ($node -is [Array]) {
        $index=0
        if ([int]::TryParse($key,[ref]$index) -and $index -ge 0 -and $index -lt $node.Length) {
            SelectValues $node[$index] $segments ($position+1)
        }
    } elseif ($null -ne $node -and (JsonType $node) -eq 'object') {
        foreach ($property in $node.PSObject.Properties) {
            if ($property.Name -ceq $key) { SelectValues $property.Value $segments ($position+1); return }
        }
    }
}
function SameJson($left,$right) {
    $kind=JsonType $left
    if ($kind -cne (JsonType $right)) { return $false }
    switch ($kind) {
        'null' { return $true }
        'string' { return [string]::Equals($left,$right,[StringComparison]::Ordinal) }
        'number' { return ([decimal]$left -eq [decimal]$right) }
        'boolean' { return ([bool]$left -eq [bool]$right) }
        'array' {
            if ($left.Length -ne $right.Length) { return $false }
            for ($i=0;$i -lt $left.Length;$i++) { if (-not (SameJson $left[$i] $right[$i])) { return $false } }
            return $true
        }
        default {
            $lp=@($left.PSObject.Properties);$rp=@($right.PSObject.Properties)
            if ($lp.Count -ne $rp.Count) { return $false }
            foreach ($p in $lp) {
                $hit=@($rp | Where-Object { $_.Name -ceq $p.Name })
                if ($hit.Count -ne 1 -or -not (SameJson $p.Value $hit[0].Value)) { return $false }
            }
            return $true
        }
    }
}
function ObjectContains($value,$expected) {
    if ((JsonType $value) -ne 'object') { return $false }
    foreach ($p in $expected.PSObject.Properties) {
        $hit=@($value.PSObject.Properties | Where-Object { $_.Name -ceq $p.Name })
        if ($hit.Count -ne 1 -or -not (SameJson $hit[0].Value $p.Value)) { return $false }
    }
    return $true
}
function VerifyFacts($handoff,$contract) {
    foreach ($case in $contract.cases) {
        $path=[string]$case.path
        $segments=@()
        if ($path.Length -gt 0) { $segments=@($path.Substring(1).Split('/')) }
        $hits=@(SelectValues $handoff $segments)
        if ($case.mode -eq 'absent') { Check ($hits.Count -eq 0) $case.name; continue }
        if ($case.mode -eq 'none_contains_text') {
            foreach ($hit in $hits) {
                $text=if ($hit.value -is [string]) { $hit.value } else { ConvertTo-Json -InputObject $hit.value -Depth 100 -Compress }
                foreach ($forbidden in $case.value) { Check (-not $text.Contains([string]$forbidden)) ($case.name + ': ' + $forbidden) }
            }
            continue
        }
        $matched=$false
        foreach ($hit in $hits) {
            $typeProperty=@($case.PSObject.Properties | Where-Object { $_.Name -ceq 'json_type' })
            if ($typeProperty.Count -eq 1 -and (JsonType $hit.value) -cne [string]$case.json_type) { continue }
            if ($case.mode -eq 'text_contains') {
                if ($hit.value -is [string] -and $hit.value.Contains([string]$case.value)) { $matched=$true; break }
            } elseif ($case.mode -eq 'object_contains') {
                if (ObjectContains $hit.value $case.value) { $matched=$true; break }
            } elseif ($case.mode -eq 'contains') {
                if (SameJson $hit.value $case.value) { $matched=$true; break }
            } else { throw "Unsupported expected-facts mode: $($case.mode)" }
        }
        Check $matched ($case.name + ' at ' + $path)
    }
}
# Independent *executable* attachment fixture, using a new synthetic Codex
# home and one ordinary text file. This does not open any real user's Codex data.
function CheckAttachmentExactness([string]$base,[string]$exe) {
    $baseRoot = Join-Path $base 'attachment-r5'
    $syntheticHome = Join-Path $baseRoot 'codex-home'
    $sessionRoot = Join-Path $syntheticHome 'sessions\2026\10\29'
    $attachmentRoot = Join-Path $syntheticHome 'attachments'
    $target = Join-Path $baseRoot 'output'
    foreach ($folder in @($baseRoot,$syntheticHome,$sessionRoot,$attachmentRoot,$target)) {
        New-Item -ItemType Directory -Path $folder -Force | Out-Null
    }
    $thread = '77777777-7777-7777-8777-777777777777'
    $attachmentId = '00000000-0000-4000-8000-000000000031'
    $missingId = '00000000-0000-4000-8000-000000000032'
    $originalDir = Join-Path $attachmentRoot $attachmentId
    New-Item -ItemType Directory -Path $originalDir | Out-Null
    $originalPath = Join-Path $originalDir 'goal-objective.md'
    $missingPath = Join-Path (Join-Path $attachmentRoot $missingId) 'goal-objective.md'
    $originalText = "SYNTHETIC-R5-ORIGINAL`r`nsecond line`r`n"
    WriteUtf8 $originalPath $originalText
    $originalSha = HashFile $originalPath
    $userText = "Read the Codex goal file at $originalPath before continuing."
    $missingText = "Read the Codex goal file at $missingPath before continuing."
    $command1 = "Get-Content -LiteralPath '$originalPath' -Raw"
    $command2 = "Get-Content -LiteralPath '$missingPath' -Raw"
    $errorPrefix = "PowerShell FileSystem InitializDefaultDrives startup error`r`n"
    $records = @(
        @{ timestamp='2026-10-29T11:00:00Z';ordinal=0;type='session_meta';payload=@{
            id=$thread;session_id=$thread;timestamp='2026-10-29T11:00:00Z';
            cwd='C:\synthetic-attachment-qualification';source='vscode';originator='Codex Desktop';
            thread_source='user';cli_version='SYNTHETIC-NOT-A-CODEX-VERSION' } },
        @{ timestamp='2026-10-29T11:00:01Z';ordinal=1;type='event_msg';payload=@{type='task_started';turn_id='attachment-turn'} },
        @{ timestamp='2026-10-29T11:00:02Z';ordinal=2;type='event_msg';payload=@{type='user_message';message=$userText} },
        @{ timestamp='2026-10-29T11:00:03Z';ordinal=3;type='response_item';payload=@{
            type='function_call';name='shell_command';call_id='attachment-read-r5';
            arguments=(ConvertTo-Json -InputObject @{command=$command1} -Compress)} },
        @{ timestamp='2026-10-29T11:00:04Z';ordinal=4;type='response_item';payload=@{
            type='function_call_output';call_id='attachment-read-r5';
            output=("Exit code: 0`nOutput:`n"+$errorPrefix+$originalText+"`r`n")} },
        @{ timestamp='2026-10-29T11:00:05Z';ordinal=5;type='event_msg';payload=@{type='user_message';message=$missingText} },
        @{ timestamp='2026-10-29T11:00:06Z';ordinal=6;type='response_item';payload=@{
            type='function_call';name='shell_command';call_id='attachment-missing-r5';
            arguments=(ConvertTo-Json -InputObject @{command=$command2} -Compress)} },
        @{ timestamp='2026-10-29T11:00:07Z';ordinal=7;type='response_item';payload=@{
            type='function_call_output';call_id='attachment-missing-r5';
            output="Exit code: 0`nOutput:`nUNVERIFIED-TOOL-ONLY`r`n"} },
        @{ timestamp='2026-10-29T11:00:08Z';ordinal=8;type='event_msg';payload=@{type='task_complete';turn_id='attachment-turn'} }
    )
    $source = Join-Path $sessionRoot ('rollout-2026-10-29T11-00-00-'+$thread+'.jsonl')
    $sourceText = (@($records | ForEach-Object { ConvertTo-Json -InputObject $_ -Depth 20 -Compress }) -join "`n") + "`n"
    WriteUtf8 $source $sourceText
    $state=[ordered]@{'project-order'=@();'pinned-project-ids'=@();'local-projects'=@{};'projectless-thread-ids'=@($thread);'thread-project-assignments'=@{}}
    WriteUtf8 (Join-Path $syntheticHome '.codex-global-state.json') (ConvertTo-Json -InputObject $state -Depth 20 -Compress)
    WriteUtf8 (Join-Path $syntheticHome 'session_index.jsonl') ((ConvertTo-Json -InputObject @{id=$thread;thread_name='Synthetic R5 attachment case'} -Compress)+"`n")
    $beforeSourceSha=HashFile $source
    [Environment]::SetEnvironmentVariable('CODEX_HOME',$syntheticHome,'Process')
    $stdout=Join-Path $LogDirectory 'attachment-r5.stdout.txt'
    $stderr=Join-Path $LogDirectory 'attachment-r5.stderr.txt'
    $prior=$ErrorActionPreference;$ErrorActionPreference='Continue'
    try { & $exe --thread-id $thread --output-directory $target --yes 1> $stdout 2> $stderr; $exitCode=[int]$LASTEXITCODE }
    finally { $ErrorActionPreference=$prior }
    Check ($exitCode -eq 0) 'R5 synthetic attachment EXE exits successfully'
    $outputs=@(Get-ChildItem -LiteralPath $target -File -Filter '*.handoff.json')
    Check ($outputs.Count -eq 1) 'R5 synthetic attachment EXE wrote exactly one Handoff'
    $handoff=[IO.File]::ReadAllText($outputs[0].FullName) | ConvertFrom-Json
    $links=@($handoff.source_observations.attachment_links)
    $good=@($links | Where-Object { $_.metadata.attachment_id -ceq $attachmentId })
    $missing=@($links | Where-Object { $_.metadata.attachment_id -ceq $missingId })
    Check ($good.Count -eq 1 -and $missing.Count -eq 1) 'R5 attachment links resolve both selected references'
    Check ($good[0].metadata.restoration_status -ceq 'restored' -and $good[0].metadata.restoration_source -ceq 'attachment_file') 'R5 verified original source is selected'
    Check (@($good[0].metadata.PSObject.Properties | Where-Object { $_.Name -ceq 'verification_method' }).Count -eq 1 -and @($good[0].metadata.PSObject.Properties | Where-Object { $_.Name -ceq 'tool_output_comparison' }).Count -eq 1) 'R7 restored attachment link preserves verification fields'
    Check ($good[0].metadata.verification_method -ceq 'attachment_file_bytes' -and $good[0].metadata.tool_output_comparison -ceq 'mismatch') 'R5 detects polluted tool result'
    Check ($good[0].metadata.sha256 -ceq $originalSha -and [int]$good[0].metadata.byte_length -eq $Utf8.GetByteCount($originalText)) 'R5 original byte hash and length match disk'
    $goodMessage=@($handoff.messages | Where-Object { $_.id -ceq $good[0].handoff_message_ref })
    Check ($goodMessage.Count -eq 1 -and $goodMessage[0].content -ceq $originalText) 'R5 actual user message has exact original content without error prefix'
    Check (@($missing[0].metadata.PSObject.Properties | Where-Object { $_.Name -ceq 'verification_method' }).Count -eq 1 -and @($missing[0].metadata.PSObject.Properties | Where-Object { $_.Name -ceq 'unverified_tool_evidence' }).Count -eq 1) 'R7 missing attachment link preserves explicitly unverified evidence'
    Check ($missing[0].metadata.restoration_status -ceq 'tool_output_unverified' -and $missing[0].metadata.verification_method -ceq 'none') 'R5 missing original is not claimed restored'
    $missingMessage=@($handoff.messages | Where-Object { $_.id -ceq $missing[0].handoff_message_ref })
    Check ($missingMessage.Count -eq 1 -and $missingMessage[0].content -ceq $missingText) 'R5 missing original retains user placeholder, not tool text'
    $unverifiedHashProperties=@($missing[0].metadata.PSObject.Properties | Where-Object { $_.Name -ceq 'sha256' })
    Check ($unverifiedHashProperties.Count -eq 0 -and @($missing[0].metadata.unverified_tool_evidence).Count -eq 1) 'R5 unverified tool result is isolated as evidence'
    Check ((HashFile $source) -eq $beforeSourceSha -and (HashFile $originalPath) -eq $originalSha) 'R5 synthetic Session and attachment remain unchanged'
}

function RunExport([string]$label,[string]$SyntheticCodexHome,[string]$out,[string]$id) {
    [Environment]::SetEnvironmentVariable('CODEX_HOME',$SyntheticCodexHome,'Process')
    $stdout=Join-Path $LogDirectory ($label+'.stdout.txt')
    $stderr=Join-Path $LogDirectory ($label+'.stderr.txt')
    $prior=$ErrorActionPreference;$ErrorActionPreference='Continue'
    try { & $Executable --thread-id $id --output-directory $out --yes 1> $stdout 2> $stderr; $rc=[int]$LASTEXITCODE }
    finally { $ErrorActionPreference=$prior }
    Check ($rc -eq 0) "$label executable exit=0"
    $text=[IO.File]::ReadAllText($stdout)
    $receipts=[regex]::Matches($text,'(?m)^\s*V2_RESULT\s+(\{[^\r\n]+\})\s*\r?$')
    Check ($receipts.Count -eq 1) "$label exactly one machine receipt"
    $receipt=$receipts[0].Groups[1].Value | ConvertFrom-Json
    Check ($receipt.status -eq 'PASS') "$label reports export PASS"
    Check ([int]$receipt.metrics.outputs_rewritten -eq 1) "$label N/N rewrite=1"
    Check ([int]$receipt.metrics.root_conversations -eq 1) "$label one root"
    Check ([int]$receipt.metrics.schema_unknown_occurrences -gt 0) "$label reports synthetic unknown data"
    Check ([int]$receipt.metrics.schema_limited_observations -gt 0) "$label reports source/retention limits"
    $files=@(Get-ChildItem -LiteralPath $out -File -Filter '*.handoff.json')
    Check ($files.Count -eq 1) "$label exactly one handoff file"
    return [pscustomobject]@{path=$files[0].FullName; data=([IO.File]::ReadAllText($files[0].FullName) | ConvertFrom-Json)}
}
New-Item -ItemType Directory -Path $WorkDirectory,$LogDirectory -Force | Out-Null
$OldHome=[Environment]::GetEnvironmentVariable('CODEX_HOME','Process')
try {
    $contractFile=Join-Path $Root 'tests\schema\expected-facts.json'
    $sourceFile=Join-Path $Root 'tests\schema\source.jsonl'
    $contract=[IO.File]::ReadAllText($contractFile) | ConvertFrom-Json
    Check ((HashFile $sourceFile) -eq [string]$contract.source_sha256) 'fixture source bound to expected facts'
    # Checker self-tests: null is not missing; arrays remain arrays; case is exact.
    $probe='{"explicit":null,"array":[],"nested":{"N":1}}' | ConvertFrom-Json
    $nullHits=@(SelectValues $probe @('explicit'));$missingHits=@(SelectValues $probe @('missing'))
    Check ($nullHits.Count -eq 1 -and $null -eq $nullHits[0].value -and $missingHits.Count -eq 0) 'checker distinguishes missing and null'
    $arrayHits=@(SelectValues $probe @('array'))
    Check ($arrayHits.Count -eq 1 -and (JsonType $arrayHits[0].value) -eq 'array') 'checker preserves an empty array'
    Check (@(SelectValues $probe @('nested','n')).Count -eq 0) 'checker uses case-sensitive JSON names'
    Check (-not (SameJson 1 '1')) 'checker rejects number-to-string flattening'
    $SyntheticCodexHome=Join-Path $WorkDirectory 'codex-home';$sessions=Join-Path $SyntheticCodexHome 'sessions\2026\10\29';$out=Join-Path $WorkDirectory 'handoffs'
    foreach ($p in @($SyntheticCodexHome,$sessions,$out,(Join-Path $SyntheticCodexHome 'attachments'))) { New-Item -ItemType Directory -Path $p -Force | Out-Null }
    $id='66666666-6666-7666-8666-666666666666'
    $destination=Join-Path $sessions ('rollout-2026-10-29T10-00-00-'+$id+'.jsonl')
    Copy-Item -LiteralPath $sourceFile -Destination $destination
    $state=[ordered]@{'project-order'=@();'pinned-project-ids'=@();'local-projects'=@{};'projectless-thread-ids'=@($id);'thread-project-assignments'=@{}}
    WriteUtf8 (Join-Path $SyntheticCodexHome '.codex-global-state.json') (ConvertTo-Json -InputObject $state -Depth 20 -Compress)
    WriteUtf8 (Join-Path $SyntheticCodexHome 'session_index.jsonl') ((ConvertTo-Json -InputObject @{id=$id;thread_name='Schema Output Qualification'} -Compress)+"`n")
    $inputHashes=@{}
    foreach ($f in @(Get-ChildItem -LiteralPath $SyntheticCodexHome -File -Recurse)) { $inputHashes[$f.FullName]=HashFile $f.FullName }
    $first=RunExport 'schema-first' $SyntheticCodexHome $out $id
    VerifyFacts $first.data $contract
    # Independent R8 output-level checks: do not accept source snapshot data
    # that exists only in an internal struct but disappeared from the Handoff.
    $receipt=$first.data.source_snapshot
    Check ($null -ne $receipt -and $receipt.mode -ceq 'selected_physical_files_with_fixed_byte_limits') 'R8 final Handoff preserves fixed-prefix source snapshot mode'
    Check ($receipt.later_appends_and_new_files -ceq 'excluded_until_next_export') 'R8 snapshot states post-capture additions are excluded'
    Check (@($receipt.physical_files).Count -eq 1 -and [long]$receipt.physical_files[0].captured_bytes -eq (Get-Item -LiteralPath $destination).Length) 'R8 snapshot records original physical byte cutoff'
    Check ($receipt.physical_files[0].source_sha256 -ceq (HashFile $destination) -and $receipt.physical_files[0].byte_scope -ceq 'physical_jsonl_prefix') 'R8 snapshot SHA matches selected physical source bytes'
    $firstHash=HashFile $first.path
    $repeat=RunExport 'schema-repeat' $SyntheticCodexHome $out $id
    VerifyFacts $repeat.data $contract
    Check ((HashFile $repeat.path) -eq $firstHash) 'same frozen input produces identical rewritten bytes'
    Check (@(Get-ChildItem -LiteralPath $SyntheticCodexHome -File -Recurse).Count -eq $inputHashes.Count) 'synthetic input file set remains unchanged'
    foreach ($path in $inputHashes.Keys) { Check ((HashFile $path) -eq $inputHashes[$path]) 'synthetic input remains unchanged' }
    # A validator which always passes is unacceptable: force an impossible value.
    $negative=[pscustomobject]@{cases=@([pscustomobject]@{name='negative detector';path='/handoff_schema_version';mode='contains';value='impossible-version'})}
    $rejected=$false
    try { VerifyFacts $first.data $negative } catch { $rejected=$true }
    Check $rejected 'expected-facts checker rejects a wrong output value'
    $beforeAttachmentChecks=$Cases.Count
    CheckAttachmentExactness $WorkDirectory $Executable
    $attachmentChecks=$Cases.Count-$beforeAttachmentChecks
    Check ($attachmentChecks -ge 10) 'R5 executable attachment regression completed all independent assertions'
    $record=[ordered]@{status='PASS';scope='synthetic executable output and attachment exact-byte verification; not private live-data acceptance';expected_fact_count=$contract.cases.Count;attachment_exactness_status='PASS';attachment_exactness_checks=$attachmentChecks;successful_checks=$Cases.Count;checks=@($Cases.ToArray());exe_sha256=(HashFile $Executable);output_sha256=$firstHash;source_sha256=(HashFile $sourceFile)}
    WriteUtf8 (Join-Path $LogDirectory 'SCHEMA-E2E-RESULT.json') (ConvertTo-Json -InputObject $record -Depth 30)
} finally { [Environment]::SetEnvironmentVariable('CODEX_HOME',$OldHome,'Process') }
