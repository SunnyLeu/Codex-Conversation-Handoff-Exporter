use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn annotated(record: Value, line: usize) -> Value {
    let bytes = serde_json::to_vec(&record).unwrap();
    let mut value = annotate(&bytes, Some(record)).unwrap().unwrap();
    set_line(&mut value, line);
    value
}
fn reduce(records: Vec<Value>) -> Value {
    let mut acc = Accumulator::default();
    for (i, record) in records.into_iter().enumerate() {
        acc.observe(&annotated(record, i + 1));
    }
    acc.finish("synthetic.jsonl", &[], &[], &[])
}
fn event(payload: Value) -> Value {
    json!({"timestamp":"2026-10-29T10:00:00Z","type":"event_msg","payload":payload})
}
fn tool(producer: &str, name: &str, text: &str) -> Value {
    json!({"type":"McpToolCall","id":"tool-test","server":producer,"tool":name,
        "status":"completed","result":{"content":[{"type":"text","text":text}]}})
}
fn activity(item: &Value) -> Value {
    crate::typed_tool_activity(
        item.as_object().unwrap(),
        Some("turn-test"),
        None,
        &Map::new(),
    )
}

#[test]
fn schema_attachment_links_preserve_r5_verification_and_unverified_evidence() {
    // This is the actual final handoff source_observations projection, not
    // merely the internal attachment registry. A limited selection used to
    // silently strip R5 verification fields even though restoration succeeded.
    let verified = json!({
        "attachment_id":"00000000-0000-4000-8000-000000000031",
        "filename":"goal-objective.md",
        "restoration_source":"attachment_file",
        "restoration_status":"restored",
        "sha256":"verified-original-sha",
        "byte_length":31,
        "character_length":31,
        "verification_method":"attachment_file_bytes",
        "attachment_file_status":"restored",
        "tool_output_comparison":"mismatch",
        "tool_candidate_count":1,
        "tool_candidate_sha256":["polluted-tool-sha"],
        "unrelated_untrusted_value":"must-not-leak"
    });
    let unverified = json!({
        "attachment_id":"00000000-0000-4000-8000-000000000032",
        "filename":"goal-objective.md",
        "restoration_source":"attachment_reference",
        "restoration_status":"tool_output_unverified",
        "verification_method":"none",
        "attachment_file_status":"missing",
        "tool_output_comparison":"original_unavailable",
        "tool_candidate_count":17,
        "tool_candidate_sha256":["tool-sha"],
        "additional_tool_candidate_count":1,
        "unverified_tool_evidence":[{
            "preview":"UNVERIFIED-TOOL-ONLY",
            "sha256":"tool-sha",
            "byte_length":20,
            "preview_truncated":false
        }]
    });
    let messages =
        [json!({"id":"u1","role":"user", "attachments":[verified.clone(),unverified.clone()]})];
    let out = Accumulator::default().finish("synthetic.jsonl", &messages, &[], &[]);
    let links = out["source_observations"]["attachment_links"]
        .as_array()
        .unwrap();
    assert_eq!(links.len(), 2);
    assert_eq!(links[0]["handoff_message_ref"], "u1");
    assert_eq!(links[0]["attachment_position"], 0);
    assert_eq!(links[1]["attachment_position"], 1);
    let mut expected_verified = verified.as_object().unwrap().clone();
    expected_verified.remove("unrelated_untrusted_value");
    assert_eq!(links[0]["metadata"], Value::Object(expected_verified));
    assert_eq!(links[1]["metadata"], unverified);
    assert!(links[1]["metadata"].get("sha256").is_none());
    assert_eq!(links[1]["metadata"]["verification_method"], "none");
    assert_eq!(
        links[1]["metadata"]["unverified_tool_evidence"][0]["preview"],
        "UNVERIFIED-TOOL-ONLY"
    );
    assert_eq!(
        links[0]["preservation_basis"],
        "existing_attachment_restoration_contract_no_new_file_reads"
    );
}

#[test]
fn schema_native_result_preserves_object_array_null_and_error() {
    let item = json!({"type":"McpToolCall","result":{"content":[{"type":"text","text":"Action completed."}],
        "structuredContent":{"status":"ok","values":[1,null,true,{"a":"b"}]}},"error":{"message":"native failure","code":409}});
    let out = activity(&item);
    assert_eq!(
        out["structured_result"]["value"],
        item["result"]["structuredContent"]
    );
    assert!(out["structured_result"]["value"]["values"].is_array());
    assert_eq!(out["error_detail"]["value"]["message"], "native failure");
}
#[test]
fn schema_screenshot_parameters_and_ordinary_data_are_not_binary() {
    let value = json!({"screenshot":[{"ref_id":"pdf","pageno":3}],"data":{"status":"ok"},"image":"filename.png","_meta":{"id":"keep"}});
    let out = project(&value, Limits::ARGUMENTS);
    assert_eq!(out["value"], value);
    assert_eq!(out["omitted_binary"], false);
    assert_eq!(out["truncated"], false);
}
#[test]
fn schema_explicit_binary_is_omitted_with_descriptor() {
    let v = json!({"type":"image","mimeType":"image/png","data":"YQ==","description":"keep"});
    let p = project(&v, Limits::RESULT);
    assert!(p["value"].get("data").is_none());
    assert_eq!(p["value"]["description"], "keep");
    assert_eq!(p["omitted_binary"], true);
    assert_eq!(p["omissions"][0]["reason"], "binary_not_embedded");
}
#[test]
fn schema_source_null_is_distinct_from_missing_or_omitted() {
    let p = project(
        &json!({"explicit":null,"binary":"data:image/png;base64,YQ=="}),
        Limits::RESULT,
    );
    assert_eq!(p["value"].get("explicit"), Some(&Value::Null));
    assert!(p["value"].get("not_provided").is_none());
    assert!(p["value"].get("binary").is_none());
    assert_eq!(
        project(&Value::Null, Limits::RESULT)["value_retained"],
        true
    );
    assert_eq!(
        project(&json!("data:image/png;base64,YQ=="), Limits::RESULT)["value_retained"],
        false
    );
}
#[test]
fn schema_array_prefix_never_shifts_an_omitted_middle_element() {
    let p = project(&json!([1, "data:image/png;base64,YQ==", 3]), Limits::RESULT);
    assert_eq!(p["value"], json!([1]));
    let desc = p["omissions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["reason"] == "array_prefix_only")
        .unwrap();
    assert_eq!(desc["retained_range"], json!([0, 1]));
    assert_eq!(desc["original_length"], 3);
}
#[test]
fn schema_unicode_truncation_preserves_valid_string_and_hash() {
    let value = json!("臺灣🙂資料");
    let p = project(
        &value,
        Limits {
            string_chars: 3,
            ..Limits::RESULT
        },
    );
    assert_eq!(p["value"], "臺灣🙂");
    assert_eq!(
        p["omissions"][0]["utf8_sha256"],
        sha("臺灣🙂資料".as_bytes())
    );
    assert_eq!(p["truncated"], true);
}
#[test]
fn schema_depth_limit_is_explicit_and_does_not_stringify_object() {
    let p = project(
        &json!({"a":{"b":{"c":1}}}),
        Limits {
            depth: 1,
            ..Limits::RESULT
        },
    );
    assert!(p["value"]["a"].is_object());
    assert!(p["value"]["a"].get("b").is_none());
    assert_eq!(p["truncated"], true);
}
#[test]
fn schema_projection_byte_budget_accounts_for_escaped_keys() {
    let v = json!({"\\\\\\\"\"\"":"中文中文中文中文中文","n":12345678901234_u64,"array":[1,2,3]});
    let p = project(
        &v,
        Limits {
            bytes: 40,
            ..Limits::RESULT
        },
    );
    assert!(serde_json::to_vec(&p["value"]).unwrap().len() <= 40);
    assert_eq!(p["truncated"], true);
}
#[test]
fn schema_native_business_command_fields_remain_opaque() {
    let native = json!({"command_template":"keep ${x}","command":"keep command","loop":{"x":1}});
    let before = json!({"type":"tool","structured_result":project(&native, Limits::RESULT)});
    let after = crate::clean_dynamic_command_value(&before);
    assert_eq!(after["structured_result"]["value"], native);
}
#[test]
fn schema_failed_aggregate_output_is_kept_in_actual_source_stream() {
    let i = json!({"type":"CommandExecution","id":"c","command":["false"],"exit_code":1,"status":"failed","aggregated_output":"SCHEMA_AGGREGATE_FAILURE"});
    let out =
        crate::typed_command_execution_output(i.as_object().unwrap(), None, None, &Map::new());
    assert_eq!(
        out["executions"][0]["diagnostics"]["aggregated_output"]["segments"][0]["text"],
        "SCHEMA_AGGREGATE_FAILURE"
    );
    assert!(out["executions"][0]["diagnostics"].get("stderr").is_none());
}
#[test]
fn schema_successful_command_with_error_words_is_not_failed() {
    let i = json!({"type":"CommandExecution","command":["echo"],"exit_code":0,"status":"completed","aggregated_output":"Historical Error: not the current failure"});
    let out =
        crate::typed_command_execution_output(i.as_object().unwrap(), None, None, &Map::new());
    assert!(out["executions"][0].get("diagnostics").is_none());
}
#[test]
fn schema_command_diagnostics_keep_separate_streams() {
    let i = json!({"type":"CommandExecution","command":["false"],"exit_code":1,"stdout":"one","stderr":"two","aggregated_output":"one\ntwo"});
    let out =
        crate::typed_command_execution_output(i.as_object().unwrap(), None, None, &Map::new());
    let d = &out["executions"][0]["diagnostics"];
    assert_eq!(d["stdout"]["segments"][0]["text"], "one");
    assert_eq!(d["stderr"]["segments"][0]["text"], "two");
    assert_eq!(d["aggregated_output"]["segments"][0]["text"], "one\ntwo");
}
#[test]
fn schema_web_aliases_preserve_original_action_and_parameters() {
    for (name, counter) in [
        ("openPage", "open_count"),
        ("open_page", "open_count"),
        ("findInPage", "find_count"),
        ("future_action", "other_count"),
    ] {
        let v = json!({"type":"WebSearch","action":{"type":name,"pattern":"keep","pageno":4}});
        let out = crate::typed_web_activity(v.as_object().unwrap(), None, None).unwrap();
        assert_eq!(out["action"], name);
        assert_eq!(out[counter], 1);
        assert_eq!(out["action_parameters"]["value"]["pattern"], "keep");
    }
}
#[test]
fn schema_completed_transport_does_not_replace_queued_business_status() {
    let i = tool(
        "codex_app",
        "open_in_codex",
        r#"{"status":"queued","threadId":"other"}"#,
    );
    let out = activity(&i);
    assert_eq!(out["status"], "completed");
    assert_eq!(
        out["result_envelope"]["projection"]["value"]["status"],
        "queued"
    );
}
#[test]
fn schema_read_thread_envelope_excludes_foreign_turns_and_keeps_cursor_opaque() {
    let raw = json!({"schemaVersion":1,"thread":{"id":"other"},"page":{"hasMore":true,"nextCursor":"{\"offset\":7}"},"turns":[{"text":"do not merge"}]});
    let i = tool("codex_app", "read_thread", &raw.to_string());
    let env = tool_envelope(i.as_object().unwrap(), &raw.to_string()).unwrap();
    assert_eq!(env["status"], "parsed");
    assert!(env["projection"]["value"].get("turns").is_none());
    assert_eq!(
        env["projection"]["value"]["page"]["nextCursor"],
        "{\"offset\":7}"
    );
    assert_eq!(
        env["excluded_result_sections"][0]["reason"],
        "referenced_thread_content_not_merged"
    );
}
#[test]
fn schema_read_thread_one_known_wrapper_is_supported_without_recursive_guessing() {
    let inner = json!({"schemaVersion":1,"thread":{},"page":{},"turns":[]}).to_string();
    let outer = json!({"content":[{"type":"text","text":inner}],"isError":false}).to_string();
    let i = tool("codex_app", "read_thread", &outer);
    let env = tool_envelope(i.as_object().unwrap(), &outer).unwrap();
    assert_eq!(env["status"], "parsed");
    assert_eq!(env["unwrapped_transport_layers"], 1);
    assert_eq!(env["transport_is_error"], false);
}
#[test]
fn schema_malformed_source_inner_json_is_never_repaired() {
    let outer = json!({"content":[{"type":"text","text":"{\"schemaVersion\":1,"}]}).to_string();
    let i = tool("codex_app", "read_thread", &outer);
    let env = tool_envelope(i.as_object().unwrap(), &outer).unwrap();
    assert_eq!(env["status"], "source_inner_not_parseable");
    assert!(env.get("projection").is_none());
    assert!(env["source_error"]["column"].is_number());
}
#[test]
fn schema_unknown_tool_version_does_not_claim_parsed() {
    let raw = r#"{"schemaVersion":999,"thread":{},"page":{},"turns":[]}"#;
    let i = tool("codex_app", "read_thread", raw);
    let env = tool_envelope(i.as_object().unwrap(), raw).unwrap();
    assert_eq!(env["status"], "unrecognized_version_or_shape");
    assert_eq!(env["observed_schema_version"], 999);
}
#[test]
fn schema_tool_envelope_is_parsed_before_legacy_text_truncation() {
    let raw=json!({"schemaVersion":1,"thread":{"id":"other"},"page":{"hasMore":true},"turns":[{"text":"x".repeat(10000)}]}).to_string();
    let out = activity(&tool("codex_app", "read_thread", &raw));
    assert_eq!(out["result_summary"]["truncated"], true);
    assert_eq!(
        out["result_envelope"]["projection"]["value"]["page"]["hasMore"],
        true
    );
}
#[test]
fn schema_custom_keyboard_report_and_exec_json_are_not_protocol_envelopes() {
    for producer in ["node_repl", "functions", "mcp__custom"] {
        let raw = r#"{"label":"keyboard","usb":true,"layout":"custom"}"#;
        assert!(tool_envelope(tool(producer, "exec", raw).as_object().unwrap(), raw).is_none());
    }
}
#[test]
fn schema_known_tool_plain_text_falls_back_without_fabricated_json() {
    let i = tool("codex_app", "open_in_codex", "Please open this manually.");
    let out = activity(&i);
    assert_eq!(out["result_envelope"]["status"], "text_fallback");
    assert_eq!(
        out["result_summary"]["segments"][0]["text"],
        "Please open this manually."
    );
}
#[test]
fn schema_list_tools_accept_only_observed_versions_and_keep_null() {
    for (name, raw) in [
        (
            "list_threads",
            json!({"schemaVersion":4,"threads":[],"page":{"nextCursor":null}}),
        ),
        ("list_projects", json!({"schemaVersion":2,"projects":[]})),
    ] {
        let text = raw.to_string();
        let e = tool_envelope(tool("codex_app", name, &text).as_object().unwrap(), &text).unwrap();
        assert_eq!(e["status"], "parsed");
        assert_eq!(e["projection"]["value"], raw);
    }
}
#[test]
fn schema_tool_unknown_outer_field_is_retained_and_flagged() {
    let text = r#"{"status":"queued","threadId":"other","future":{"enabled":true}}"#;
    let e = tool_envelope(
        tool("codex_app", "open_in_codex", text)
            .as_object()
            .unwrap(),
        text,
    )
    .unwrap();
    assert_eq!(e["unknown_envelope_fields"], json!(["future"]));
    assert_eq!(e["projection"]["value"]["future"]["enabled"], true);
}
#[test]
fn schema_settings_update_retains_authorship_not_user_messages() {
    let r = json!({"type":"response_item","payload":{"type":"configuration_update","reasoning":{"effort":"ultra"}},"metadata":{"client_authored":false,"harness_authored_configuration":true}});
    let out = reduce(vec![r]);
    assert_eq!(
        out["source_observations"]["events"]["settings"][0]["data"]["source_authorship"]["harness_authored_configuration"],
        true
    );
    assert_eq!(
        out["source_observations"]["events"]["settings"][0]["data"]["observation"]["value"]["reasoning"]
            ["effort"],
        "ultra"
    );
    assert!(out.get("messages").is_none());
}
#[test]
fn schema_permissions_deduplicate_exact_value_without_path_normalization() {
    let base = json!({"type":"turn_context","payload":{"turn_id":"t","model":"synthetic","file_system_sandbox_policy":{"path":"C:\\Path"},"active_permission_profile":null}});
    let mut changed = base.clone();
    changed["payload"]["file_system_sandbox_policy"]["path"] = json!("C:\\path");
    let out = reduce(vec![base.clone(), base, changed]);
    assert_eq!(
        out["source_observations"]["policies"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        out["source_observations"]["events"]["settings"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        out["source_observations"]["events"]["settings"][0]["data"]["policy_ref_retained"],
        true
    );
}
#[test]
fn schema_typed_agent_wait_does_not_infer_receiver_or_child_completion() {
    let out = reduce(vec![event(
        json!({"type":"item_completed","item":{"type":"CollabAgentToolCall","tool":"wait","status":"completed","receiver_thread_ids":[],"agents_states":{}}}),
    )]);
    let d = &out["source_observations"]["events"]["agents"][0]["data"];
    assert_eq!(d["observation"]["value"]["receiver_thread_ids"], json!([]));
    assert_eq!(
        d["claim"],
        "source_observation_not_inferred_child_completion"
    );
}
#[test]
fn schema_agent_call_links_use_explicit_id_only() {
    let out = reduce(vec![
        json!({"type":"response_item","payload":{"type":"function_call","call_id":"c","name":"spawn_agent","arguments":"{}"}}),
        event(
            json!({"type":"item_completed","item":{"type":"SubAgentActivity","id":"c","kind":"started","agent_thread_id":"child"}}),
        ),
    ]);
    assert_eq!(
        out["source_observations"]["events"]["agents"][0]["matching_call_source_refs"][0]["call_id"],
        "c"
    );
}
#[test]
fn schema_verified_answer_links_output_without_duplicate_answer() {
    let out = reduce(vec![
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"yes"}}),
        json!({"type":"retained_context","payload":{"type":"verified_answer","call_id":"c","acceptance_order":7,"questions":[{"answer":"yes"}]}}),
    ]);
    let c = &out["source_observations"]["events"]["confirmations"][0];
    assert_eq!(c["data"]["value"]["acceptance_order"], 7);
    assert_eq!(c["data"]["value"]["adds_messages"], false);
    assert_eq!(c["matching_answer_output_source_refs"][0]["call_id"], "c");
}
#[test]
fn schema_retained_exact_links_and_shorter_text_never_replace_messages() {
    let out = reduce(vec![
        json!({"type":"response_item","payload":{"type":"message","id":"m","role":"assistant","content":[{"type":"output_text","text":"full original"}]}}),
        json!({"type":"compacted","payload":{"retained_context":{"assistant_messages":[{"message_id":"m","text":"full original","complete":true},{"message_id":"m","text":"full","complete":false},{"message_id":"different","text":"full original","complete":true}]}}}),
    ]);
    let refs = &out["source_observations"]["events"]["retention"][0]["data"]["value"]["retained_message_refs"];
    assert_eq!(
        refs[0]["match_status"],
        "same_source_message_id_and_text_hash"
    );
    assert_eq!(
        refs[1]["match_status"],
        "same_id_different_text_not_replaced"
    );
    assert_eq!(refs[2]["match_status"], "not_resolved_in_selected_lineage");
    assert!(out.get("messages").is_none());
}
#[test]
fn schema_replacement_context_and_encryption_are_descriptors_only() {
    let out = reduce(vec![
        json!({"type":"compacted","payload":{"replacement_history":[{"type":"message","role":"user","content":"not a new user"}]}}),
        json!({"type":"response_item","payload":{"type":"compaction","encrypted_content":"opaque secret"}}),
    ]);
    let value = &out["source_observations"]["events"]["retention"];
    assert_eq!(value[0]["data"]["value"]["adds_messages"], false);
    assert_eq!(
        value[1]["data"]["value"]["encrypted_content"]["reason"],
        "encrypted_unreadable"
    );
    assert!(!out.to_string().contains("opaque secret"));
}
#[test]
fn schema_inline_media_hash_does_not_claim_body_embedded() {
    let d = media_descriptor(
        &json!({"type":"input_image","image_url":"data:image/png;base64,YQ=="}),
        1,
        &json!({"id":"m"}),
    );
    assert_eq!(d["body_in_handoff"], false);
    assert_eq!(d["sha256"], sha(b"a"));
    assert_eq!(d["decoded_bytes"], 1);
    assert_eq!(d["body_available_in_source_record"], true);
    assert!(!d.to_string().contains("data:image/png;base64,YQ=="));
}
#[test]
fn schema_local_image_never_reads_arbitrary_path() {
    let d = media_descriptor(
        &json!({"type":"local_image","path":"C:\\must-not-read\\secret.png"}),
        0,
        &json!({}),
    );
    assert_eq!(d["file_availability"], "not_checked");
    assert!(d.get("sha256").is_none());
}
#[test]
fn schema_base64_rejects_malformed_padding() {
    assert_eq!(decode_base64("YQ=="), Some(vec![b'a']));
    assert_eq!(decode_base64("YWJj"), Some(b"abc".to_vec()));
    assert!(decode_base64("YQ=A").is_none());
    assert!(decode_base64("YR==").is_none());
    assert!(decode_base64("==AA").is_none());
}
#[test]
fn schema_usage_preserves_scopes_latest_and_null_not_zero() {
    let rows=[10,12].into_iter().map(|n|json!({"type":"token_usage_record","payload":{"thread_id":"r","turn_id":"t","usage":{"total_tokens":2},"turn_token_usage":{"total_tokens":n,"cached_input_tokens":null},"thread_token_usage":{"total_tokens":100+n}}})).collect();
    let out = reduce(rows);
    let usage = &out["source_observations"]["usage"];
    assert_eq!(
        usage["turn_last_observations"][0]["observation"]["value"]["total_tokens"],
        12
    );
    assert_eq!(
        usage["turn_last_observations"][0]["observation"]["value"]["cached_input_tokens"],
        Value::Null
    );
    assert_eq!(
        usage["thread_last_observation"]["observation"]["value"]["total_tokens"],
        112
    );
    assert_eq!(usage["scopes_are_not_additive"], true);
    assert_eq!(usage["thread_last_observation"]["final_billing"], false);
}
#[test]
fn schema_unknown_record_survives_selective_drop_as_observation_only() {
    let raw = br#"{"type":"future_event","payload":{"x":1}}"#;
    let mut a = annotate(raw, None).unwrap().unwrap();
    set_line(&mut a, 17);
    assert_eq!(a["type"], "handoff_source_observation");
    let mut acc = Accumulator::default();
    acc.observe(&a);
    let out = acc.finish("source", &[], &[], &[]);
    assert_eq!(
        out["schema_coverage"]["unknowns"][0]["issue"]["kind"],
        "unrecognized_record_family"
    );
    assert_eq!(
        out["schema_coverage"]["unknowns"][0]["sample_sources"][0]["logical_snapshot_line"],
        17
    );
    assert!(out.get("messages").is_none());
}
#[test]
fn schema_unknown_field_type_and_enum_are_reported_without_open_data_noise() {
    let known = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"WebSearch","action":{"type":"never_seen"},"new_native_field":true}}});
    let f = family(&known);
    let u = unknown_fields(&known, &f);
    assert!(u.iter().any(|v| v["kind"] == "unrecognized_web_action"));
    assert!(
        u.iter()
            .any(|v| v["path"] == "/payload/item/new_native_field")
    );
    let native = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"McpToolCall","result":{"structuredContent":{"arbitrary_business_key":5}}}}});
    let u = unknown_fields(&native, &family(&native));
    assert!(!u.iter().any(|v| {
        v["path"]
            .as_str()
            .unwrap_or("")
            .contains("arbitrary_business_key")
    }));
}
#[test]
fn schema_monitor_array_limit_is_disclosed() {
    let parts: Vec<Value> = (0..130)
        .map(|_| json!({"type":"text","text":"ok"}))
        .collect();
    let v = json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"McpToolCall","result":{"content":parts}}}});
    assert!(
        unknown_fields(&v, &family(&v))
            .iter()
            .any(|v| v["kind"] == "native_array_scan_limit")
    );
}
#[test]
fn schema_observation_count_limit_is_disclosed() {
    let mut acc = Accumulator::default();
    for _ in 0..MAX_OBSERVATIONS + 2 {
        acc.push("synthetic", json!({"data":{}}));
    }
    let out = acc.finish("source", &[], &[], &[]);
    assert_eq!(out["schema_coverage"]["observations_omitted_by_limits"], 2);
    assert_eq!(
        out["source_observations"]["events"]["synthetic"]
            .as_array()
            .unwrap()
            .len(),
        MAX_OBSERVATIONS
    );
}
#[test]
fn schema_coverage_counts_include_children_and_tool_limits() {
    let v = json!({"schema_coverage":{"unknowns":[{"count":2}],"observations_omitted_by_limits":1,"supplementary_projections_with_limits":3,"tool_result_limits":[{}]},"subagents":[{"schema_coverage":{"unknowns":[{"count":1}],"activities_with_bounded_projection":4}}]});
    assert_eq!(coverage_counts(&v), (3, 9));
}
#[test]
fn schema_source_hash_and_locator_do_not_claim_physical_byte_offsets() {
    let raw = br#"{"type":"future_event","payload":{}}"#;
    let r = annotate(raw, None).unwrap().unwrap();
    assert_eq!(r[ANNOTATION]["source"]["record_utf8_sha256"], sha(raw));
    assert_eq!(
        r[ANNOTATION]["source"]["hash_scope"],
        "logical_snapshot_record_without_line_ending"
    );
    assert!(
        r[ANNOTATION]["source"]
            .get("physical_byte_offset")
            .is_none()
    );
}

struct TempFixture(PathBuf);
impl TempFixture {
    fn new(contents: &[u8], ext: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "codex-schema-test-{}-{}.{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            ext
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, contents).unwrap();
        Self(path)
    }
}
impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn fixture_handoffs(path: &Path) -> Vec<Value> {
    let cwd = crate::normalize_windows_path(r"C:\synthetic-schema-qualification");
    let attachments = Path::new("schema-fixture-no-attachments");
    let legacy = crate::convert_codex_session_file_with_ingestion(path, &cwd, attachments, true)
        .unwrap()
        .unwrap();
    let prefix = crate::convert_codex_session_file_with_ingestion(path, &cwd, attachments, false)
        .unwrap()
        .unwrap();
    let mut fused = crate::convert_codex_session_file_streaming(path, &cwd, attachments)
        .unwrap()
        .unwrap();
    let titles = HashMap::new();
    let (_, a) = crate::build_handoff(&legacy, &titles).unwrap();
    let (_, b) = crate::build_handoff(&prefix, &titles).unwrap();
    let (_, c) = crate::build_handoff(&fused, &titles).unwrap();
    let (_, d) = crate::build_handoff_move(&mut fused, &titles).unwrap();
    vec![a, b, c, d]
}
#[test]
fn schema_real_ingestion_paths_and_owned_builder_match() {
    let fixture = TempFixture::new(include_bytes!("../tests/schema/source.jsonl"), "jsonl");
    let h = fixture_handoffs(&fixture.0);
    for (index, item) in h[1..].iter().enumerate() {
        let route = ["prefix", "fused", "owned"][index];
        assert!(
            crate::json_semantic_equal(&h[0], item),
            "SCHEMA PARITY DIAGNOSTIC: legacy vs {route}; first difference: {}",
            crate::first_json_difference(&h[0], item, "$.handoff")
                .unwrap_or_else(|| "difference not localized".to_string())
        );
    }
    // Regression: a payloadless compacted reducer record must not erase its
    // original retention provenance (or silently make all readers agree to omit it).
    for (index, handoff) in h.iter().enumerate() {
        let has_retained_refs = handoff
            .pointer("/source_observations/events/retention")
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter().any(|row| {
                    row.pointer("/data/value/retained_message_refs")
                        .and_then(Value::as_array)
                        .is_some_and(|refs| !refs.is_empty())
                })
            });
        assert!(
            has_retained_refs,
            "ingestion route {index} lost retention provenance"
        );
    }
    assert_eq!(h[0]["handoff_schema_version"], VERSION);
    let activities = h[0]["activities"].as_array().unwrap();
    assert!(
        activities.iter().any(
            |v| v.pointer("/structured_result/value/metadata/duration_ms") == Some(&json!(120))
        )
    );
    assert!(
        activities
            .iter()
            .any(|v| v.pointer("/input_summary/value/screenshot/0/pageno") == Some(&json!(3)))
    );
    assert!(activities.iter().any(|v| {
        v.pointer("/executions/0/diagnostics/aggregated_output/segments/0/text")
            == Some(&json!(
                "AttributeError: 'list' object has no attribute 'update'"
            ))
    }));
    for message in h[0]["messages"].as_array().unwrap() {
        let text = message.get("content").and_then(Value::as_str).unwrap_or("");
        assert!(
            !text.contains("DO_NOT_MERGE_OTHER_THREAD")
                && !text.contains("REPLACEMENT_NOT_NEW_USER")
        );
    }
}
#[test]
fn schema_zstandard_input_retains_same_source_observations() {
    let bytes = include_bytes!("../tests/schema/source.jsonl");
    let plain = TempFixture::new(bytes, "jsonl");
    let compressed = zstd::stream::encode_all(&bytes[..], 3).unwrap();
    let compressed = TempFixture::new(&compressed, "jsonl.zst");
    let a = fixture_handoffs(&plain.0);
    let b = fixture_handoffs(&compressed.0);
    assert_eq!(a[0]["schema_coverage"], b[0]["schema_coverage"]);
    assert_eq!(
        a[0]["source_observations"]["events"],
        b[0]["source_observations"]["events"]
    );
}

#[test]
fn schema_duplicate_aggregate_uses_explicit_stream_alias() {
    let i = json!({"type":"CommandExecution","command":["false"],"exit_code":1,"stdout":"failure","aggregated_output":"failure"});
    let out =
        crate::typed_command_execution_output(i.as_object().unwrap(), None, None, &Map::new());
    assert_eq!(
        out["executions"][0]["diagnostic_aliases"]["aggregated_output"],
        "stdout"
    );
    assert_eq!(
        out["executions"][0]["diagnostics"]["stdout"]["segments"][0]["text"],
        "failure"
    );
    assert!(
        out["executions"][0]["diagnostics"]
            .get("aggregated_output")
            .is_none()
    );
}
#[test]
fn schema_observed_content_array_elements_are_known_native_shells() {
    let r = json!({"timestamp":"2026-10-29T10:00:00Z","type":"response_item","payload":{"type":"message","id":"m","role":"user","content":[{"type":"input_text","text":"plain"}]}});
    assert!(unknown_fields(&r, &family(&r)).is_empty());
}

#[test]
fn schema_deidentified_real_case_regressions_preserve_audited_fields() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../tests/schema/real-case-derivatives.json")).unwrap();
    assert_eq!(fixtures["cases"].as_array().unwrap().len(), 5);
    for case in fixtures["cases"].as_array().unwrap() {
        let r = &case["record"];
        let i = &r["payload"]["item"];
        let kind = i["type"].as_str().unwrap();
        if kind == "McpToolCall" {
            let out = activity(i);
            if let Some(native) = i.pointer("/result/structuredContent") {
                assert_eq!(&out["structured_result"]["value"], native);
            }
            if let Some(screenshot) = i.pointer("/arguments/screenshot") {
                assert_eq!(&out["input_summary"]["value"]["screenshot"], screenshot);
                assert_eq!(out["input_summary"]["omitted_binary"], false);
            }
            if let Some(error) = i.get("error") {
                assert_eq!(&out["error_detail"]["value"], error);
            }
        } else if kind == "CommandExecution" {
            let out = crate::typed_command_execution_output(
                i.as_object().unwrap(),
                None,
                None,
                r["payload"].as_object().unwrap(),
            );
            assert_eq!(
                out["executions"][0]["diagnostics"]["aggregated_output"]["segments"][0]["text"],
                i["aggregated_output"]
            );
        } else if kind == "Extension" {
            let out = crate::typed_web_activity(i.as_object().unwrap(), None, None).unwrap();
            assert_eq!(out["action"], "openPage");
            assert_eq!(out["open_count"], 1);
        } else {
            panic!("unrecognized test fixture type {kind}");
        }
    }
}

#[test]
fn schema_child_observations_do_not_import_precreation_parent_settings() {
    let records = vec![
        json!({"type":"session_meta","timestamp":"2026-10-29T10:00:10Z","payload":{
            "id":"66666666-6666-7666-8666-666666666666","timestamp":"2026-10-29T10:00:10Z","cwd":"C:\\synthetic-schema-qualification",
            "thread_source":"subagent","session_id":"77777777-7777-7777-8777-777777777777",
            "parent_thread_id":"77777777-7777-7777-8777-777777777777","agent_path":"/root/worker",
            "source":{"subagent":{"thread_spawn":{"depth":1,"agent_role":"worker"}}}}}),
        json!({"type":"event_msg","timestamp":"2026-10-29T10:00:01Z","payload":{"type":"task_started","turn_id":"parent"}}),
        json!({"type":"turn_context","timestamp":"2026-10-29T10:00:02Z","payload":{"turn_id":"parent","model":"PARENT_ONLY_POLICY"}}),
        json!({"type":"response_item","timestamp":"2026-10-29T10:00:03Z","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"PARENT_ONLY_MESSAGE"}],"internal_chat_message_metadata_passthrough":{"turn_id":"parent"}}}),
        json!({"type":"event_msg","timestamp":"2026-10-29T10:00:04Z","payload":{"type":"task_complete","turn_id":"parent"}}),
        json!({"type":"event_msg","timestamp":"2026-10-29T10:00:11Z","payload":{"type":"task_started","turn_id":"local"}}),
        json!({"type":"turn_context","timestamp":"2026-10-29T10:00:12Z","payload":{"turn_id":"local","model":"CHILD_LOCAL_POLICY"}}),
        json!({"type":"response_item","timestamp":"2026-10-29T10:00:13Z","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"CHILD_LOCAL_MESSAGE"}],"internal_chat_message_metadata_passthrough":{"turn_id":"local"}}}),
        json!({"type":"event_msg","timestamp":"2026-10-29T10:00:14Z","payload":{"type":"task_complete","turn_id":"local"}}),
    ];
    let bytes = records
        .iter()
        .map(|v| serde_json::to_string(v).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let fixture = TempFixture::new(bytes.as_bytes(), "jsonl");
    let handoffs = fixture_handoffs(&fixture.0);
    for h in &handoffs {
        assert!(
            !h["source_observations"]
                .to_string()
                .contains("PARENT_ONLY_POLICY")
        );
        assert!(
            h["source_observations"]
                .to_string()
                .contains("CHILD_LOCAL_POLICY")
        );
        assert!(!h["messages"].to_string().contains("PARENT_ONLY_MESSAGE"));
        assert!(h["messages"].to_string().contains("CHILD_LOCAL_MESSAGE"));
        assert!(crate::json_semantic_equal(h, &handoffs[0]));
    }
}
