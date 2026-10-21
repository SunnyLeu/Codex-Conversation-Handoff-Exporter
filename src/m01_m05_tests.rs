use super::*;
use serde_json::json;

#[test]
fn m01_typed_web_search_preserves_bounded_provenance_only() {
    let item = json!({
        "type":"WebSearch","id":"exec-web-1","query":"alpha",
        "action":{"type":"search","query":"alpha","queries":["alpha","beta"]},
        "results":[
            {"type":"text_result","ref_id":"turn0search0","url":"https://example.com/a","title":"TITLE","snippet":"SECRET-SNIPPET"},
            {"type":"text_result","ref_id":"turn0search1","url":"https://example.com/b","thumbnail":"BASE64"}
        ]
    });
    let activity = typed_web_activity(
        item.as_object().unwrap(),
        Some("turn-1"),
        Some(1_700_000_000_000_000),
    )
    .unwrap();
    assert_eq!(activity["type"], "web_research");
    assert_eq!(activity["source_item_id"], "exec-web-1");
    assert_eq!(activity["search_count"], 1);
    assert_eq!(activity["queries"], json!(["alpha", "beta"]));
    assert_eq!(
        activity["references"],
        json!(["turn0search0", "turn0search1"])
    );
    assert_eq!(
        activity["urls"],
        json!(["https://example.com/a", "https://example.com/b"])
    );
    let text = serde_json::to_string(&activity).unwrap();
    assert!(!text.contains("SECRET-SNIPPET"));
    assert!(!text.contains("BASE64"));
    assert!(!text.contains("TITLE"));
}

#[test]
fn m01_extension_only_accepts_web_search_kind() {
    let other = json!({"type":"Extension","kind":"other","id":"x"});
    assert!(typed_web_activity(other.as_object().unwrap(), None, None).is_none());
    let web = json!({"type":"Extension","kind":"web.search","id":"w","action":{"type":"search","queries":["q"]},"results":[]});
    assert!(typed_web_activity(web.as_object().unwrap(), None, None).is_some());
}

#[test]
fn m02_image_view_is_provenance_not_image_payload() {
    let item = json!({"type":"ImageView","id":"exec-img-1","path":"file:///C:/x/image.png","bytes":"SHOULD_NOT_LEAK"});
    let activity =
        typed_image_view_activity(item.as_object().unwrap(), Some("turn-2"), None).unwrap();
    assert_eq!(activity["type"], "image_view");
    assert_eq!(activity["source_item_id"], "exec-img-1");
    assert_eq!(activity["path"], "file:///C:/x/image.png");
    assert!(
        !serde_json::to_string(&activity)
            .unwrap()
            .contains("SHOULD_NOT_LEAK")
    );
}

#[test]
fn m03_tool_output_transport_preserves_scalar_and_fragment_bytes() {
    let scalar = json!("execution error: Io(boom)");
    assert_eq!(
        get_tool_output_text(Some(&scalar)).as_deref(),
        Some("execution error: Io(boom)")
    );
    let fragments = json!([
        {"type":"input_text","text":"Script failed\nOutput:\n"},
        {"type":"input_text","text":"Script error:\nboom"}
    ]);
    assert_eq!(
        get_tool_output_text(Some(&fragments)).as_deref(),
        Some("Script failed\nOutput:\nScript error:\nboom")
    );
}

#[test]
fn m03_failure_diagnostic_is_no_exit_top_level_only() {
    assert!(command_wrapper_failure_diagnostic(Some("Script failed\nboom"), &[]).is_some());
    assert!(command_wrapper_failure_diagnostic(Some("execution error: Io(boom)"), &[]).is_some());
    assert!(
        command_wrapper_failure_diagnostic(
            Some("Script completed\nOutput:\nold Script failed"),
            &[]
        )
        .is_none()
    );
    assert!(
        command_wrapper_failure_diagnostic(Some("Script failed\nExit code: 1"), &[1]).is_none()
    );
}

#[test]
fn m04_timestamp_conflict_falls_back_only_when_payload_end_repairs_order() {
    let start = 200;
    let record_end = 100;
    let payload_end = 300;
    let (end, conflict) =
        resolve_task_complete_end_time(Some(start), Some(record_end), Some(payload_end));
    assert_eq!(end, Some(payload_end));
    assert!(conflict.is_some());

    let (end, conflict) = resolve_task_complete_end_time(Some(start), Some(250), Some(300));
    assert_eq!(end, Some(250));
    assert!(conflict.is_none());

    let (end, conflict) = resolve_task_complete_end_time(Some(start), Some(record_end), Some(150));
    assert_eq!(end, Some(record_end));
    assert!(conflict.is_none());
}

#[test]
fn m05_redundant_parsed_cmd_cmd_is_removed_but_argv_stays() {
    let item = json!({
        "type":"CommandExecution","id":"exec-1","command":["pwsh","-Command","Write-Host hi"],
        "status":"completed","exit_code":0,
        "parsed_cmd":[{"cmd":"Write-Host hi","type":"shell","path":"x"}]
    });
    let payload = json!({"started_at_ms":1,"completed_at_ms":2});
    let activity = typed_command_execution_output(
        item.as_object().unwrap(),
        Some("turn"),
        None,
        payload.as_object().unwrap(),
    );
    let execution = &activity["executions"][0];
    assert_eq!(
        execution["argv"],
        json!(["pwsh", "-Command", "Write-Host hi"])
    );
    assert_eq!(execution["command"], "Write-Host hi");
    assert!(execution["parsed_cmd"][0].get("cmd").is_none());
    assert_eq!(execution["parsed_cmd"][0]["type"], "shell");
}

#[test]
fn m05_non_equal_parsed_cmd_cmd_is_preserved() {
    let item = json!({
        "type":"CommandExecution","id":"exec-2","command":["pwsh","-Command","outer"],
        "parsed_cmd":[{"cmd":"inner","type":"shell"}]
    });
    let payload = json!({});
    let activity = typed_command_execution_output(
        item.as_object().unwrap(),
        None,
        None,
        payload.as_object().unwrap(),
    );
    assert_eq!(activity["executions"][0]["parsed_cmd"][0]["cmd"], "inner");
}
