// E01–E06 contract regressions. Each lifecycle fixture uses the real observed
// record shapes; integration fixtures in tests/real are the immutable raw evidence.
use super::*;
use serde_json::json;

fn fixture_handoff(records: &[Value]) -> Value {
    let unique = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "codex-e01-e06-{}-{unique}.jsonl",
        std::process::id()
    ));
    let mut bytes = Vec::new();
    for record in records {
        bytes.extend(serde_json::to_vec(record).unwrap());
        bytes.push(b'\n');
    }
    fs::write(&path, bytes).unwrap();
    let target = normalize_windows_path(TEST_TARGET_CWD);
    let legacy = convert_codex_session_file(&path, &target, Path::new("missing-attachments"))
        .unwrap()
        .unwrap();
    let fused =
        convert_codex_session_file_streaming(&path, &target, Path::new("missing-attachments"))
            .unwrap()
            .unwrap();
    let _ = fs::remove_file(&path);
    let (_, a) = build_handoff(&legacy, &HashMap::new()).unwrap();
    let (_, b) = build_handoff(&fused, &HashMap::new()).unwrap();
    assert!(
        json_semantic_equal(&a, &b),
        "legacy and fused differ: {a}\n{b}"
    );
    b
}
fn meta(child: bool) -> Value {
    let mut value = json!({"type":"session_meta","timestamp":"2026-09-13T00:00:10Z","payload":{
        "id":"00000000-0000-7000-8000-000000000001","timestamp":"2026-09-13T00:00:10Z","cwd":TEST_TARGET_CWD,"source":"vscode"}});
    if child {
        value["payload"]["thread_source"] = json!("subagent");
        value["payload"]["session_id"] = json!("00000000-0000-7000-8000-000000000002");
        value["payload"]["source"] =
            json!({"subagent":{"thread_spawn":{"depth":1,"agent_role":"worker"}}});
        value["payload"]["parent_thread_id"] = json!("00000000-0000-7000-8000-000000000002");
        value["payload"]["agent_path"] = json!("/root/worker");
    }
    value
}
fn event(at: &str, payload: Value) -> Value {
    json!({"type":"event_msg","timestamp":at,"payload":payload})
}
fn message(at: &str, tid: &str, text: &str) -> Value {
    json!({"type":"response_item","timestamp":at,"payload":{"type":"message","role":"assistant","phase":"final_answer",
    "content":[{"type":"output_text","text":text}],"internal_chat_message_metadata_passthrough":{"turn_id":tid}}})
}
#[test]
fn e01_terminal_only_child_is_preserved_but_precreation_parent_is_not() {
    let h = fixture_handoff(&[
        meta(true),
        event(
            "2026-09-13T00:00:01Z",
            json!({"type":"task_started","turn_id":"parent"}),
        ),
        message("2026-09-13T00:00:02Z", "parent", "inherited answer"),
        event(
            "2026-09-13T00:00:03Z",
            json!({"type":"task_complete","turn_id":"parent"}),
        ),
        event(
            "2026-09-13T00:00:11Z",
            json!({"type":"task_started","turn_id":"rollout-4"}),
        ),
        message("2026-09-13T00:00:12Z", "real-local", "local answer"),
        event(
            "2026-09-13T00:00:13Z",
            json!({"type":"task_complete","turn_id":"real-local"}),
        ),
        event(
            "2026-09-13T00:00:14Z",
            json!({"type":"task_complete","turn_id":"rollout-4"}),
        ),
    ]);
    assert!(
        h["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["content"] == "local answer")
    );
    assert!(
        !h["messages"].to_string().contains("inherited answer"),
        "{h}"
    );
    assert!(!h["turns"].to_string().contains("parent"));
    assert!(
        h["turns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["turn_id"] == "real-local")
    );
}
#[test]
fn e01_rollback_does_not_restore_terminal_only_messages() {
    let h = fixture_handoff(&[
        meta(true),
        event(
            "2026-09-13T00:00:11Z",
            json!({"type":"task_started","turn_id":"rollout-4"}),
        ),
        message("2026-09-13T00:00:12Z", "real-local", "must disappear"),
        event(
            "2026-09-13T00:00:13Z",
            json!({"type":"task_complete","turn_id":"real-local"}),
        ),
        event(
            "2026-09-13T00:00:14Z",
            json!({"type":"thread_rolled_back","num_turns":1}),
        ),
        message("2026-09-13T00:00:15Z", "rollout-4", "surviving answer"),
        event(
            "2026-09-13T00:00:16Z",
            json!({"type":"task_complete","turn_id":"rollout-4"}),
        ),
    ]);
    assert!(!h["messages"].to_string().contains("must disappear"));
    assert!(h["messages"].to_string().contains("surviving answer"));
}
fn async_request() -> Value {
    let call = json!({"type":"function_call","name":"request_user_input_async","call_id":"async-1","arguments":"{\"questions\":[{\"title\":\"授權？\"}]}"});
    build_user_input_request(
        call.as_object().unwrap(),
        parse_datetime_text("2026-09-13T00:00:11Z"),
        Some("t"),
    )
    .unwrap()
    .1
}
#[test]
fn e02_acknowledgment_is_not_an_answer() {
    for accepted in [true, false] {
        let mut req = async_request();
        let output = json!({"output":json!({"accepted":accepted}).to_string()});
        complete_user_input_request(
            &mut req,
            output.as_object().unwrap(),
            parse_datetime_text("2026-09-13T00:00:12Z"),
        );
        assert_eq!(
            req["status"],
            if accepted {
                json!("awaiting_response")
            } else {
                json!("delivery_rejected")
            }
        );
        assert_eq!(req["accepted"], json!(accepted));
        assert!(req.get("acknowledged_at").is_some());
        assert!(req.get("answered_at").is_none());
        assert!(req.get("answers").is_none());
    }
}
#[test]
fn e02_unknown_ack_is_explicit_not_fabricated() {
    let mut req = async_request();
    complete_user_input_request(
        &mut req,
        json!({"output":"{\"unrecognized\":true}"})
            .as_object()
            .unwrap(),
        None,
    );
    assert_eq!(req["status"], "acknowledgment_unparsed");
    assert!(req.get("accepted").is_none());
    assert!(req.get("answered_at").is_none());
}
#[test]
fn e02_delivery_attaches_once_and_null_options_are_preserved() {
    let item = json!({"type":"AgentMessage","id":"async-1","delivery":"async","questions":[{"title":"授權？","options":null}],
        "phase":"final_answer","content":[{"type":"Text","text":"授權？"}]});
    let delivery = async_delivery(item.as_object().unwrap(), Some("t"), None).unwrap();
    let mut reqs = vec![async_request()];
    attach_async_deliveries(&mut reqs, vec![delivery.clone(), delivery]);
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0]["delivery"]["questions"][0]
            .get("options")
            .unwrap()
            .is_null()
    );
    assert!(reqs[0]["questions"][0].get("options").is_none());
}
#[test]
fn e02_typed_only_delivery_does_not_invent_call_or_answer() {
    let item = json!({"type":"AgentMessage","id":"item-only","delivery":"async","questions":[{"title":"Q","options":null}]});
    let mut reqs = vec![];
    attach_async_deliveries(
        &mut reqs,
        vec![async_delivery(item.as_object().unwrap(), None, None).unwrap()],
    );
    assert_eq!(reqs[0]["status"], "delivery_observed");
    assert!(reqs[0].get("call_id").is_none());
    assert!(reqs[0].get("answered_at").is_none());
}
#[test]
fn e02_selective_async_call_and_ack_are_retained() {
    let mut state = RolloutIngestionState::default();
    for value in [
        json!({"type":"response_item","timestamp":"2026-09-13T00:00:11Z","payload":{"type":"function_call",
            "name":"request_user_input_async","call_id":"async-1","arguments":"{\"questions\":[{\"title\":\"Q\"}]}"}}),
        json!({"type":"response_item","timestamp":"2026-09-13T00:00:12Z","payload":{"type":"function_call_output","call_id":"async-1","output":"{\"accepted\":true}"}}),
    ] {
        assert!(
            parse_rollout_line(&serde_json::to_vec(&value).unwrap(), &mut state)
                .unwrap()
                .is_some()
        );
    }
}
fn outputs(parts: Vec<Value>) -> Value {
    let mut v = vec![json!({"type":"text","text":"Script completed\nOutput:\n"})];
    v.extend(
        parts
            .into_iter()
            .map(|p| json!({"type":"text","text":p.to_string()})),
    );
    Value::Array(v)
}
#[test]
fn e03_separates_command_json_from_goal_json_without_losing_whitespace() {
    let content = "  leading\r\ntrailing\r\n";
    let output = outputs(vec![
        json!({"exit_code":0,"output":format!("{content}\r\n")}),
        json!({"goal":{"status":"active"}}),
    ]);
    assert_eq!(
        extract_successful_command_output(Some(&output)).as_deref(),
        Some(content)
    );
}
#[test]
fn e03_rejects_multiple_command_results_even_if_identical() {
    let cmd = json!({"exit_code":0,"output":"same\n"});
    assert!(extract_successful_command_output(Some(&outputs(vec![cmd.clone(), cmd]))).is_none());
}
#[test]
fn e03_rejects_failed_unstructured_or_explicitly_truncated_output() {
    for command in [
        json!({"exit_code":1,"output":"failure\n"}),
        json!({"exit_code":0,"output":"prefix …24 tokens truncated… suffix\n"}),
        json!({"exit_code":0,"output":"prefix …2489 tokens truncated… suffix\n"}),
        json!({"exit_code":0,"output":"text\n","truncated":true}),
    ] {
        assert!(extract_successful_command_output(Some(&outputs(vec![command]))).is_none());
    }
    let x = json!([{"type":"text","text":"Script completed\nOutput:\n"},
        {"type":"text","text":"{\"exit_code\":0,\"output\":\"body\\n\"}"},
        {"type":"text","text":"unstructured unrelated result"}]);
    assert!(extract_successful_command_output(Some(&x)).is_none());
}
#[test]
fn e03_nested_document_json_is_data_not_another_envelope() {
    let body = "{\"goal\":{\"status\":\"active\"}}\n";
    let x = outputs(vec![json!({"exit_code":0,"output":format!("{body}\n")})]);
    assert_eq!(
        extract_successful_command_output(Some(&x)).as_deref(),
        Some(body)
    );
}
#[test]
fn e04_get_goal_result_excludes_shell_stdout_and_rejects_ambiguity() {
    let goal = json!({"goal":{"threadId":"s","createdAt":1,"updatedAt":2,"tokensUsed":42}});
    let x = outputs(vec![
        json!({"exit_code":0,"output":goal.to_string()}),
        goal.clone(),
    ]);
    assert_eq!(
        extract_goal_observation_result(Some(&x)).unwrap()["goal"]["tokensUsed"],
        42
    );
    assert!(
        extract_goal_observation_result(Some(&outputs(vec![goal.clone(), goal.clone()]))).is_none()
    );
    assert!(
        extract_goal_observation_result(Some(&outputs(vec![
            json!({"exit_code":0,"output":goal.to_string()})
        ])))
        .is_none()
    );
}
#[test]
fn e04_observations_merge_same_generation_only_and_do_not_regress_time() {
    let updates = vec![
        json!({"created_at":"2026-01-01T00:00:00.000+00:00","updated_at":"2026-01-01T00:01:00.000+00:00","reported_status":"active","tokens_used":5}),
        json!({"observation":true,"created_at":"2026-01-01T00:00:00.000+00:00","updated_at":"2026-01-01T00:02:00.000+00:00","tokens_used":8}),
        json!({"observation":true,"created_at":"2026-01-01T00:00:00.000+00:00","updated_at":"2026-01-01T00:01:30.000+00:00","tokens_used":6}),
        json!({"observation":true,"created_at":"2025-12-01T00:00:00.000+00:00","updated_at":"2026-01-01T00:03:00.000+00:00","tokens_used":900}),
    ];
    let projected = projectable_goal_updates(&updates);
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[1]["tokens_used"], 8);
    assert_eq!(projected[1]["reported_status"], "active");
    assert_eq!(updates.len(), 4); // History remains intact.
}
#[test]
fn e04_exec_goal_name_ignores_comments_and_quoted_decoys() {
    assert_eq!(
        exec_goal_tool_name(Some(r#"// tools.update_goal({})\n"#)),
        None
    );
    assert_eq!(
        exec_goal_tool_name(Some(
            r#"const note = "tools.create_goal({})"; text(await tools.get_goal({}));"#
        ))
        .as_deref(),
        Some("get_goal")
    );
    assert!(
        exec_goal_tool_name(Some(
            "text(await tools.get_goal({})); text(await tools.get_goal({}));"
        ))
        .is_none()
    );
}
#[test]
fn e05_unicode_diagnostic_ranges_and_hash_are_exact() {
    let text = format!("HEAD{}TAIL", "兆😀".repeat(3000));
    let summary = bounded_text(&text, 4096);
    assert_eq!(summary["sha256"], sha256_text(Some(&text)));
    assert_eq!(summary["character_length"], text.chars().count());
    assert_eq!(summary["byte_length"], text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut kept = 0;
    for s in summary["segments"].as_array().unwrap() {
        let a = s["start"].as_u64().unwrap() as usize;
        let b = s["end"].as_u64().unwrap() as usize;
        assert_eq!(s["text"], chars[a..b].iter().collect::<String>());
        kept += b - a;
    }
    assert_eq!(kept, 4096);
}
#[test]
fn e05_success_does_not_duplicate_stdout_and_nonzero_does_not_invent_classification() {
    let mut item = json!({"type":"CommandExecution","id":"exec-1","command":["rg","needle"],"stdout":"no results","stderr":"","status":"completed","exit_code":0});
    let ok = typed_command_execution_output(item.as_object().unwrap(), None, None, &Obj::new());
    assert!(ok["executions"][0].get("diagnostics").is_none());
    item["exit_code"] = json!(1);
    let bad = typed_command_execution_output(item.as_object().unwrap(), None, None, &Obj::new());
    assert!(bad["executions"][0].get("diagnostics").is_some());
    assert!(bad["executions"][0].get("error_type").is_none());
}
#[test]
fn e06_dynamic_and_mcp_shapes_keep_errors_without_binary_metadata() {
    let dynamic = json!({"type":"DynamicToolCall","id":"d","namespace":"codex_app","tool":"read_thread","success":false,
        "content_items":[{"type":"inputText","text":"invalid maxOutputChars"}]});
    let d = typed_tool_activity(dynamic.as_object().unwrap(), Some("t"), None, &Obj::new());
    assert_eq!(d["success"], false);
    assert!(d.to_string().contains("invalid maxOutputChars"));
    let mcp = json!({"type":"McpToolCall","id":"m","server":"node_repl","tool":"js","status":"failed",
        "arguments":{"code":"test","screenshot":"RAW_BINARY"},
        "result":{"isError":true,"content":[{"type":"text","text":"access denied"},{"type":"image","data":"RAW_BINARY"}],"_meta":{"screenshot":"RAW_BINARY"}}});
    let m = typed_tool_activity(mcp.as_object().unwrap(), Some("t"), None, &Obj::new());
    assert_eq!(m["is_error"], true);
    assert_eq!(m["omitted_nontext_items"], 1);
    assert!(!m.to_string().contains("RAW_BINARY"));
    assert!(m.to_string().contains("access denied"));
    assert_eq!(m["input_summary"]["omitted_binary"], true);
}
#[test]
fn e06_argument_budget_is_bounded_and_marked() {
    let args =
        json!({"long":"x".repeat(8000),"nested":[{"a":"y".repeat(8000)},{"b":"z".repeat(8000)}]});
    let summary = compact_tool_arguments(&args);
    assert_eq!(summary["truncated"], true);
    assert!(summary.to_string().len() < 3000);
}
#[test]
fn e06_rollback_does_not_resurrect_new_tool_activity() {
    let h = fixture_handoff(&[
        meta(false),
        event(
            "2026-09-13T00:00:11Z",
            json!({"type":"task_started","turn_id":"rolled"}),
        ),
        event(
            "2026-09-13T00:00:12Z",
            json!({"type":"item_completed","turn_id":"rolled","item":{"type":"DynamicToolCall","id":"drop-me","tool":"t","success":false,"content_items":[{"type":"inputText","text":"error"}]}}),
        ),
        event(
            "2026-09-13T00:00:13Z",
            json!({"type":"task_complete","turn_id":"rolled"}),
        ),
        event(
            "2026-09-13T00:00:14Z",
            json!({"type":"thread_rolled_back","num_turns":1}),
        ),
        event(
            "2026-09-13T00:00:15Z",
            json!({"type":"task_started","turn_id":"keep"}),
        ),
        message("2026-09-13T00:00:16Z", "keep", "survivor"),
        event(
            "2026-09-13T00:00:17Z",
            json!({"type":"task_complete","turn_id":"keep"}),
        ),
    ]);
    assert!(!h["activities"].to_string().contains("drop-me"));
}
