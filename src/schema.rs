//! Handoff 來源觀測與明示省略。此模組不執行來源文字、不取得網路資源，也不擴張專案範圍。
//! 原生協定外層與任意工作產物分開；「曾見過」不代表「完整收錄」。
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

pub const VERSION: &str = "1.0";
pub const ANNOTATION: &str = "__handoff_source_observation";
const MAX_OBSERVATIONS: usize = 4096;
const MAX_SESSION_BYTES: usize = 16 * 1024 * 1024;
const MAX_UNKNOWN_GROUPS: usize = 512;

pub fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn json_bytes(value: &Value) -> Vec<u8> {
    // Value 的序列化不包含非有限浮點數；雜湊表示此壓縮 JSON，不宣稱 RFC 8785。
    serde_json::to_vec(value).expect("JSON Value is serializable")
}
pub fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
fn selected(value: &Value, keys: &[&str]) -> Value {
    let mut output = Map::new();
    for key in keys {
        if let Some(v) = value.get(*key) {
            output.insert((*key).into(), v.clone());
        }
    }
    Value::Object(output)
}
fn pointer(parent: &str, key: &str) -> String {
    format!("{}/{}", parent, key.replace('~', "~0").replace('/', "~1"))
}
fn descriptor(value: &Value, reason: &str, path: &str) -> Value {
    let encoded = json_bytes(value);
    let mut output = json!({"path":path,"reason":reason,"original_type":value_type(value),
        "json_utf8_bytes":encoded.len(),"json_sha256":sha(&encoded)});
    if let Some(s) = value.as_str() {
        output["characters"] = json!(s.chars().count());
        output["utf8_sha256"] = json!(sha(s.as_bytes()));
    }
    if let Some(a) = value.as_array() {
        output["original_length"] = json!(a.len());
    }
    if let Some(a) = value.as_object() {
        output["original_members"] = json!(a.len());
    }
    output
}
fn is_data_url(text: &str) -> bool {
    text.starts_with("data:")
        && text
            .split_once(',')
            .is_some_and(|(header, _)| header.to_ascii_lowercase().contains(";base64"))
}
fn is_base64(text: &str) -> bool {
    !text.is_empty()
        && text.len().is_multiple_of(4)
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub bytes: usize,
    pub string_chars: usize,
    pub nodes: usize,
    pub depth: usize,
    pub items: usize,
}
impl Limits {
    pub const ARGUMENTS: Self = Self {
        bytes: 8192,
        string_chars: 2048,
        nodes: 256,
        depth: 10,
        items: 64,
    };
    pub const RESULT: Self = Self {
        bytes: 65536,
        string_chars: 4096,
        nodes: 2048,
        depth: 16,
        items: 128,
    };
}
struct Budget {
    limits: Limits,
    left: usize,
    nodes: usize,
    omissions: Vec<Value>,
    extra_omissions: usize,
    binary: bool,
}
impl Budget {
    fn omitted(&mut self, value: &Value, reason: &str, path: &str) {
        if self.omissions.len() < 128 {
            self.omissions.push(descriptor(value, reason, path));
        } else {
            self.extra_omissions += 1;
        }
    }
    fn visit(&mut self, value: &Value, path: &str, depth: usize, binary: bool) -> Option<Value> {
        if binary || value.as_str().is_some_and(is_data_url) {
            self.binary = true;
            self.omitted(value, "binary_not_embedded", path);
            return None;
        }
        if depth > self.limits.depth || self.nodes == 0 || self.left < 8 {
            self.omitted(value, "projection_limit", path);
            return None;
        }
        self.nodes -= 1;
        match value {
            Value::Object(map) => {
                self.left = self.left.saturating_sub(2);
                let mut result = Map::new();
                // 保持來源鍵順序；不依 screenshot/data/image 的名稱刪除普通參數。
                let encoded_binary =
                    matches!(map.get("encoding").and_then(Value::as_str), Some("base64"))
                        || (matches!(
                            map.get("type").and_then(Value::as_str),
                            Some("image" | "audio")
                        ) && (map.contains_key("mimeType") || map.contains_key("mime_type")));
                for (key, child) in map {
                    let location = pointer(path, key);
                    let key_cost = serde_json::to_vec(key).expect("JSON key").len() + 2;
                    if result.len() >= self.limits.items || key_cost + 8 > self.left {
                        self.omitted(child, "object_member_limit", &location);
                        continue;
                    }
                    self.left = self.left.saturating_sub(key_cost);
                    let binary =
                        encoded_binary && key == "data" && child.as_str().is_some_and(is_base64);
                    if let Some(v) = self.visit(child, &location, depth + 1, binary) {
                        result.insert(key.clone(), v);
                    }
                }
                Some(Value::Object(result))
            }
            Value::Array(array) => {
                self.left = self.left.saturating_sub(2);
                let mut result = Vec::new();
                for (index, child) in array.iter().enumerate() {
                    if index >= self.limits.items || self.left < 8 || self.nodes == 0 {
                        break;
                    }
                    if let Some(v) =
                        self.visit(child, &pointer(path, &index.to_string()), depth + 1, false)
                    {
                        self.left = self.left.saturating_sub(1);
                        result.push(v);
                    } else {
                        break;
                    } // 僅保留前綴，不能刪掉中間項後改變索引。
                }
                if result.len() != array.len() {
                    let mut d = descriptor(value, "array_prefix_only", path);
                    d["retained_range"] = json!([0, result.len()]);
                    if self.omissions.len() < 128 {
                        self.omissions.push(d);
                    } else {
                        self.extra_omissions += 1;
                    }
                }
                Some(Value::Array(result))
            }
            Value::String(text) => {
                let mut result = String::new();
                let mut count = 0usize;
                let mut cost = 2usize;
                for ch in text.chars() {
                    let c = match ch {
                        '"' | '\\' => 2,
                        '\n' | '\r' | '\t' => 2,
                        c if c < ' ' => 6,
                        c => c.len_utf8(),
                    };
                    if count >= self.limits.string_chars || cost + c > self.left {
                        break;
                    }
                    result.push(ch);
                    count += 1;
                    cost += c;
                }
                if result.len() != text.len() {
                    let before = self.omissions.len();
                    self.omitted(value, "string_prefix_only", path);
                    if self.omissions.len() > before {
                        self.omissions.last_mut().expect("new omission")["retained_characters"] =
                            json!(count);
                    }
                }
                self.left = self.left.saturating_sub(cost);
                Some(Value::String(result))
            }
            _ => {
                let cost = json_bytes(value).len();
                if cost > self.left {
                    self.omitted(value, "projection_limit", path);
                    None
                } else {
                    self.left -= cost;
                    Some(value.clone())
                }
            }
        }
    }
}
/// 保留原生型別；完整、省略、截短與來源 null 各自有明確表示。
pub fn project(value: &Value, limits: Limits) -> Value {
    let mut budget = Budget {
        limits,
        left: limits.bytes,
        nodes: limits.nodes,
        omissions: Vec::new(),
        extra_omissions: 0,
        binary: false,
    };
    let projected = budget.visit(value, "", 0, false);
    let truncated = !budget.omissions.is_empty() || budget.extra_omissions != 0;
    let mut output = json!({"original_type":value_type(value),"truncated":truncated,"omitted_binary":budget.binary,
        "source_json_sha256":sha(&json_bytes(value)),"hash_encoding":"serde_json_compact_utf8",
        "omissions":budget.omissions,"additional_omission_count":budget.extra_omissions});
    if let Some(value) = projected {
        output["value"] = value;
        output["value_retained"] = json!(true);
    } else {
        output["value_retained"] = json!(false);
    }
    output
}

/// 只解析已觀測的具名工具外層；不解析游標、程式碼、stdout 或自訂報表。
pub fn tool_envelope(item: &Map<String, Value>, text: &str) -> Option<Value> {
    let producer = item
        .get("server")
        .or_else(|| item.get("namespace"))
        .and_then(Value::as_str)?;
    if producer != "codex_app" {
        return None;
    }
    let name = item.get("tool").and_then(Value::as_str)?;
    if !matches!(
        name,
        "open_in_codex" | "read_thread" | "list_threads" | "list_projects"
    ) {
        return None;
    }
    let mut result = json!({"parser":format!("codex_app.{name}"),"parser_version":"1",
        "source_type":"string","source_utf8_sha256":sha(text.as_bytes()),"source_characters":text.chars().count()});
    if !text.trim_start().starts_with('{') {
        result["status"] = json!("text_fallback");
        return Some(result);
    }
    let mut value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(error) => {
            result["status"] = json!("source_not_parseable");
            result["source_error"] = json!({"line":error.line(),"column":error.column()});
            return Some(result);
        }
    };
    if name == "read_thread"
        && value.get("schemaVersion").is_none()
        && value.get("content").is_some()
    {
        result["transport_is_error"] = value.get("isError").cloned().unwrap_or(Value::Null);
        let inner = value
            .get("content")
            .and_then(Value::as_array)
            .filter(|a| a.len() == 1)
            .and_then(|a| a[0].get("text"))
            .and_then(Value::as_str);
        match inner {
            Some(inner) => {
                result["inner_utf8_sha256"] = json!(sha(inner.as_bytes()));
                match serde_json::from_str::<Value>(inner) {
                    Ok(v) => {
                        value = v;
                        result["unwrapped_transport_layers"] = json!(1);
                    }
                    Err(error) => {
                        result["status"] = json!("source_inner_not_parseable");
                        result["source_error"] =
                            json!({"line":error.line(),"column":error.column()});
                        return Some(result);
                    }
                }
            }
            None => {
                result["status"] = json!("unrecognized_transport_shape");
                return Some(result);
            }
        }
    }
    let version = value.get("schemaVersion").and_then(Value::as_u64);
    let recognized = match name {
        "open_in_codex" => {
            value.get("status").is_some_and(Value::is_string)
                && value.get("threadId").is_some_and(Value::is_string)
        }
        "read_thread" => {
            version == Some(1)
                && value.get("thread").is_some_and(Value::is_object)
                && value.get("page").is_some_and(Value::is_object)
                && value.get("turns").is_some_and(Value::is_array)
        }
        "list_threads" => version == Some(4) && value.get("threads").is_some_and(Value::is_array),
        "list_projects" => version == Some(2) && value.get("projects").is_some_and(Value::is_array),
        _ => false,
    };
    if !recognized {
        result["status"] = json!("unrecognized_version_or_shape");
        if let Some(v) = value.get("schemaVersion") {
            result["observed_schema_version"] = v.clone();
        }
        return Some(result);
    }
    // 被讀取的另一條對話不是本條對話的新 messages。其內容只留下明示摘要與原來源身分。
    if name == "read_thread" {
        if let Some(turns) = value.as_object_mut().and_then(|v| v.remove("turns")) {
            result["excluded_result_sections"] = json!([descriptor(
                &turns,
                "referenced_thread_content_not_merged",
                "/turns"
            )]);
        }
    }
    let supported: &[&str] = match name {
        "open_in_codex" => &["status", "threadId"],
        "read_thread" => &[
            "schemaVersion",
            "thread",
            "page",
            "unavailableSources",
            "unavailableHosts",
        ],
        "list_threads" => &[
            "schemaVersion",
            "threads",
            "page",
            "sections",
            "sidebarPreferences",
            "unavailableSources",
            "unavailableHosts",
        ],
        _ => &[
            "schemaVersion",
            "projects",
            "page",
            "unavailableSources",
            "unavailableHosts",
        ],
    };
    let unknown: Vec<String> = value
        .as_object()
        .into_iter()
        .flat_map(|v| v.keys())
        .filter(|k| !supported.contains(&k.as_str()))
        .cloned()
        .collect();
    result["unknown_envelope_fields"] = json!(unknown);
    result["status"] = json!("parsed");
    result["projection"] = project(&value, Limits::RESULT);
    Some(result)
}

fn family(record: &Value) -> String {
    let Some(kind) = string(record, "type") else {
        return "__legacy_meta".into();
    };
    let p = record.get("payload").unwrap_or(&Value::Null);
    let subtype = string(p, "type");
    if matches!(kind, "response_item" | "event_msg" | "retained_context") {
        if let Some(sub) = subtype {
            if kind == "event_msg" && sub == "item_completed" {
                return format!(
                    "{kind}:{sub}:{}",
                    p.pointer("/item/type")
                        .and_then(Value::as_str)
                        .unwrap_or("<missing>")
                );
            }
            return format!("{kind}:{sub}");
        }
    }
    kind.into()
}
fn shapes() -> &'static Value {
    static SHAPES: OnceLock<Value> = OnceLock::new();
    SHAPES.get_or_init(|| {
        serde_json::from_str(include_str!("schema_shapes.json"))
            .expect("embedded shape specification")
    })
}
fn unknown_fields(record: &Value, family: &str) -> Vec<Value> {
    let spec = shapes();
    let Some(fields) = spec
        .get("families")
        .and_then(|s| s.get(family))
        .and_then(Value::as_object)
    else {
        return vec![json!({"kind":"unrecognized_record_family","path":"","observed":family})];
    };
    fn visit(
        v: &Value,
        path: &str,
        fields: &Map<String, Value>,
        scopes: &[Value],
        out: &mut Vec<Value>,
    ) {
        if out.len() >= 64 {
            return;
        }
        let observed = value_type(v);
        if let Some(allowed) = fields.get(path).and_then(Value::as_array) {
            if !allowed.iter().any(|a| a.as_str() == Some(observed)) {
                out.push(
                    json!({"kind":"unrecognized_native_type","path":path,"observed":observed}),
                );
            }
        } else {
            out.push(json!({"kind":"unrecognized_native_field","path":path,"observed":observed}));
        }
        if !scopes.iter().any(|p| p.as_str() == Some(path)) {
            return;
        }
        if let Some(map) = v.as_object() {
            for (k, child) in map {
                visit(child, &pointer(path, k), fields, scopes, out);
            }
        }
        if let Some(array) = v.as_array() {
            for child in array.iter().take(128) {
                visit(child, &pointer(path, "[]"), fields, scopes, out);
            }
            if array.len() > 128 && out.len() < 64 {
                out.push(json!({"kind":"native_array_scan_limit","path":path,"observed":"first_128_elements_only","original_length":array.len()}));
            }
        }
    }
    let scopes = spec["scopes"].as_array().expect("shape scopes");
    let mut unknown = Vec::new();
    visit(record, "", fields, scopes, &mut unknown);
    if let Some(action) = record
        .pointer("/payload/item/action/type")
        .and_then(Value::as_str)
    {
        if !matches!(
            action,
            "search"
                | "open"
                | "openPage"
                | "open_page"
                | "find"
                | "findInPage"
                | "find_in_page"
                | "click"
                | "other"
        ) {
            unknown.push(json!({"kind":"unrecognized_web_action","path":"/payload/item/action/type","observed":action}));
        }
    }
    if let Some(kind) = record.pointer("/payload/item/type").and_then(Value::as_str) {
        let pair = match kind {
            "SubAgentActivity" => Some((
                "kind",
                &["started", "interacted", "completed", "interrupted"][..],
            )),
            "McpToolCall" | "DynamicToolCall" | "CommandExecution" => Some((
                "status",
                &[
                    "in_progress",
                    "completed",
                    "failed",
                    "error",
                    "interrupted",
                    "cancelled",
                    "canceled",
                    "pending",
                    "running",
                ][..],
            )),
            _ => None,
        };
        if let Some((key, known)) = pair {
            if let Some(value) = record["payload"]["item"].get(key).and_then(Value::as_str) {
                if !known.contains(&value) {
                    unknown.push(json!({"kind":"unrecognized_native_enum","path":format!("/payload/item/{key}"),"observed":value}));
                }
            }
        }
    }
    if unknown.len() >= 64 {
        unknown.push(json!({"kind":"native_field_scan_limit","path":"","observed":"at_least_64_issues; further issues not enumerated"}));
    }
    unknown
}

fn text_sequence(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.into());
    }
    if let Some(a) = v.as_array() {
        return Some(
            a.iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    v.get("text").and_then(Value::as_str).map(str::to_string)
}
fn source_ref(record: &Value, line: &[u8]) -> Value {
    let mut r = selected(record, &["timestamp", "ordinal"]);
    r["record_utf8_sha256"] = json!(sha(line));
    r["hash_scope"] = json!("logical_snapshot_record_without_line_ending");
    let p = record.get("payload").unwrap_or(&Value::Null);
    for key in [
        "thread_id",
        "turn_id",
        "response_id",
        "call_id",
        "root_turn_id",
    ] {
        if let Some(v) = p.get(key) {
            r[key] = v.clone();
        }
    }
    if let Some(v) = p.pointer("/internal_chat_message_metadata_passthrough/turn_id") {
        r["turn_id"] = v.clone();
    }
    if let Some(v) = p.pointer("/item/id").or_else(|| p.get("id")) {
        r["item_id"] = v.clone();
    }
    r
}
fn observation(kind: &str, payload: Value) -> Value {
    json!({"kind":kind,"data":payload})
}

/// 一次處理原始行：觀測在 selective ingestion 之前形成，不能只在輸出末端補欄位。
/// 回傳的觀測只走已有 Session／子代理歸屬與 local-history 邊界，不新增主對話。
pub fn annotate(line: &[u8], compact: Option<Value>) -> anyhow::Result<Option<Value>> {
    let raw: Value = serde_json::from_slice(line)?;
    if !raw.is_object() {
        return Ok(compact);
    }
    let f = family(&raw);
    let p = raw.get("payload").unwrap_or(&Value::Null);
    let item = p.get("item").unwrap_or(&Value::Null);
    let kind = string(&raw, "type").unwrap_or("");
    let sub = string(p, "type").unwrap_or("");
    let mut entries = Vec::new();
    let mut disposition = "existing_projection";
    match kind {
        "turn_context" => {
            let v = selected(
                p,
                &[
                    "turn_id",
                    "model",
                    "effort",
                    "reasoning_effort",
                    "summary",
                    "collaboration_mode",
                    "multi_agent_mode",
                    "multi_agent_version",
                    "approval_policy",
                    "approvals_reviewer",
                    "cwd",
                    "workspace_roots",
                    "current_date",
                    "timezone",
                ],
            );
            let policies = selected(
                p,
                &[
                    "sandbox_policy",
                    "permission_profile",
                    "file_system_sandbox_policy",
                    "active_permission_profile",
                    "network_sandbox_policy",
                    "network_access",
                ],
            );
            entries.push(observation("settings",json!({"observation":project(&v,Limits::RESULT),"policies":policies,"source_authorship":selected(&raw["metadata"],&["client_authored","harness_authored_configuration"])})));
        }
        "response_item" if sub == "configuration_update" => {
            entries.push(observation("settings",json!({"observation":project(p,Limits::RESULT),"source_authorship":selected(&raw["metadata"],&["client_authored","harness_authored_configuration"])})));
        }
        "event_msg" if sub == "thread_settings_applied" => {
            entries.push(observation(
                "settings",
                json!({"observation":project(p,Limits::RESULT)}),
            ));
        }
        "event_msg"
            if sub == "item_completed"
                && matches!(
                    string(item, "type"),
                    Some("SubAgentActivity" | "CollabAgentToolCall")
                ) =>
        {
            entries.push(observation("agents",json!({"observation":project(item,Limits::RESULT),"timing":selected(p,&["started_at_ms","completed_at_ms"]),"claim":"source_observation_not_inferred_child_completion"})));
        }
        "inter_agent_communication_metadata" => {
            entries.push(observation("communications",json!({"observation":project(p,Limits::ARGUMENTS),"association":"source_order_only_no_inferred_recipient"})));
        }
        "token_usage_record" => {
            entries.push(observation(
                "usage",
                selected(
                    p,
                    &[
                        "thread_id",
                        "turn_id",
                        "session_id",
                        "response_id",
                        "root_turn_id",
                        "usage",
                        "turn_token_usage",
                        "thread_token_usage",
                    ],
                ),
            ));
        }
        "compacted" => {
            let mut data = selected(
                p,
                &[
                    "window_number",
                    "first_window_id",
                    "previous_window_id",
                    "window_id",
                    "compaction_response_id",
                ],
            );
            if let Some(resume) = p.get("resume_metadata") {
                data["resume_metadata"] = project(resume, Limits::RESULT);
            }
            if let Some(retained) = p.get("retained_context") {
                data["source_flags"] = selected(
                    retained,
                    &[
                        "incomplete",
                        "user_messages_incomplete",
                        "assistant_messages_incomplete",
                        "next_order",
                    ],
                );
                let mut refs = Vec::new();
                for role in ["user", "assistant"] {
                    if let Some(messages) = retained
                        .get(format!("{role}_messages"))
                        .and_then(Value::as_array)
                    {
                        for msg in messages {
                            let mut d = selected(
                                msg,
                                &[
                                    "message_id",
                                    "id",
                                    "turn_id",
                                    "order",
                                    "phase",
                                    "revision",
                                    "complete",
                                    "timestamp",
                                ],
                            );
                            d["role"] = json!(role);
                            let text = msg
                                .get("content")
                                .or_else(|| msg.get("text"))
                                .and_then(text_sequence);
                            if let Some(text) = text {
                                d["text_utf8_sha256"] = json!(sha(text.as_bytes()));
                                d["text_characters"] = json!(text.chars().count());
                            }
                            d["source_entry_sha256"] = json!(sha(&json_bytes(msg)));
                            refs.push(d);
                        }
                    }
                }
                data["retained_message_refs"] = json!(refs);
                if let Some(v) = retained.get("verified_answers") {
                    data["verified_answers_descriptor"] = descriptor(
                        v,
                        "referenced_answers_not_new_messages",
                        "/retained_context/verified_answers",
                    );
                }
            }
            if let Some(history) = p.get("replacement_history").and_then(Value::as_array) {
                let descriptors: Vec<Value> = history
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let mut d = selected(v, &["id", "type", "role", "call_id"]);
                        d["position"] = json!(i);
                        d["content_descriptor"] = descriptor(
                            v,
                            "replacement_context_not_new_messages",
                            &format!("/replacement_history/{i}"),
                        );
                        d
                    })
                    .collect();
                data["replacement_history"] = json!(descriptors);
            }
            if let Some(v) = p.get("replacement_history_metadata") {
                data["replacement_history_metadata"] = project(v, Limits::RESULT);
            }
            if let Some(v) = p.get("guardian_history") {
                data["guardian_history_descriptor"] =
                    descriptor(v, "guardian_content_excluded", "/guardian_history");
            }
            data["adds_messages"] = json!(false);
            entries.push(observation("retention", project(&data, Limits::RESULT)));
        }
        "retained_context" => {
            let mut data = selected(p, &["type", "turn_id", "call_id", "acceptance_order"]);
            if let Some(q) = p.get("questions") {
                data["questions_descriptor"] =
                    descriptor(q, "answer_provenance_not_duplicate_answer", "/questions");
            }
            data["adds_messages"] = json!(false);
            entries.push(observation("confirmations", project(&data, Limits::RESULT)));
        }
        "response_item" if sub == "compaction" => {
            let mut data = selected(p, &["type", "id"]);
            if let Some(v) = p.get("encrypted_content") {
                data["encrypted_content"] =
                    descriptor(v, "encrypted_unreadable", "/encrypted_content");
            }
            entries.push(observation("retention", project(&data, Limits::ARGUMENTS)));
        }
        "event_msg"
            if sub == "item_completed" && string(item, "type") == Some("ContextCompaction") =>
        {
            entries.push(observation("retention", project(item, Limits::ARGUMENTS)));
        }
        "event_msg"
            if sub == "item_completed"
                && string(item, "type") == Some("Extension")
                && string(item, "kind") == Some("clock.sleep") =>
        {
            entries.push(observation("clock",json!({"observation":project(item,Limits::ARGUMENTS),"timing":selected(p,&["started_at_ms","completed_at_ms"])})));
        }
        "response_item"
            if matches!(
                sub,
                "tool_search_call" | "tool_search_output" | "additional_tools"
            ) =>
        {
            entries.push(observation("capabilities", capability_summary(p)));
            disposition = "bounded_summary";
        }
        "world_state" => {
            entries.push(observation(
                "background",
                descriptor(p, "environment_background_not_messages", "/payload"),
            ));
            disposition = "intentional_exclusion";
        }
        "response_item" if sub == "reasoning" => disposition = "intentional_exclusion",
        "event_msg" if matches!(sub, "agent_reasoning" | "mcp_tool_call_end") => {
            disposition = "intentional_exclusion"
        }
        _ => {}
    }
    // 時間、父 turn、來源作者與 retained 來源不是新的使用者指令。
    let relation = selected(p, &["root_turn_id", "turn_attribution"]);
    let metadata = raw.get("metadata").filter(|m| !m.is_null());
    if relation.as_object().is_some_and(|m| !m.is_empty()) || metadata.is_some() {
        let mut data = json!({"relation":project(&relation,Limits::ARGUMENTS)});
        if let Some(m) = metadata {
            data["metadata"] = project(m, Limits::RESULT);
        }
        entries.push(observation("provenance", data));
    }
    // 媒體只是參照。圖片本體不放入 Handoff；不從本機任意路徑讀檔。
    if kind == "response_item" && sub == "message" {
        if let Some(content) = p.get("content").and_then(Value::as_array) {
            for (i, part) in content.iter().enumerate() {
                if matches!(
                    string(part, "type"),
                    Some("input_image" | "image" | "local_image" | "input_audio")
                ) {
                    entries.push(observation("media", media_descriptor(part, i, p)));
                }
            }
        }
    }
    if kind == "event_msg" && sub == "item_completed" && string(item, "type") == Some("UserMessage")
    {
        if let Some(content) = item.get("content").and_then(Value::as_array) {
            for (i, part) in content.iter().enumerate() {
                if matches!(
                    string(part, "type"),
                    Some("input_image" | "image" | "local_image" | "input_audio")
                ) {
                    entries.push(observation("media", media_descriptor(part, i, item)));
                }
            }
        }
    }
    // 訊息身分只用來連結已由原流程匯出的訊息，不在這裡重建文字。
    let message_identity = if kind == "response_item" && matches!(sub, "message" | "agent_message")
    {
        let mut v = selected(p, &["id", "role", "phase", "revision"]);
        if let Some(text) = p.get("content").and_then(text_sequence) {
            v["text_utf8_sha256"] = json!(sha(text.as_bytes()));
        }
        Some(v)
    } else {
        None
    };
    let unknown = unknown_fields(&raw, &f);
    if !shapes()["families"]
        .as_object()
        .is_some_and(|m| m.contains_key(&f))
    {
        disposition = "unrecognized";
    } else if !entries.is_empty() && disposition == "existing_projection" {
        disposition = "supplemented_projection";
    }
    if compact.is_none() && disposition == "existing_projection" {
        disposition = "existing_filter_not_semantically_certified";
    }
    let mut annotation = json!({"family":f,"source":source_ref(&raw,line),"disposition":disposition,"observations":entries,"unknown":unknown});
    if let Some(m) = message_identity {
        annotation["message_identity"] = m;
    }
    let mut record=compact.unwrap_or_else(||json!({"type":"handoff_source_observation","timestamp":raw.get("timestamp").cloned().unwrap_or(Value::Null),"payload":{}}));
    if let Some(object) = record.as_object_mut() {
        object.insert(ANNOTATION.into(), annotation);
    }
    Ok(Some(record))
}

fn capability_summary(p: &Value) -> Value {
    fn names(v: &Value, out: &mut Vec<Value>, depth: usize) {
        if depth > 6 || out.len() >= 256 {
            return;
        }
        if let Some(a) = v.as_array() {
            for v in a {
                names(v, out, depth + 1);
            }
        }
        if let Some(m) = v.as_object() {
            if m.get("name").is_some_and(Value::is_string) {
                out.push(selected(v, &["name", "type", "namespace", "server"]));
            }
            for key in ["tools", "results"] {
                if let Some(v) = m.get(key) {
                    names(v, out, depth + 1);
                }
            }
        }
    }
    let mut list = Vec::new();
    names(p, &mut list, 0);
    json!({"identity":selected(p,&["type","id","call_id","status","query"]),"tool_identities":list,
        "full_definition":descriptor(p,"capability_summary_not_full_definitions","/payload")})
}
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    if !is_base64(text) || text.len() > 24 * 1024 * 1024 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, c) in bytes.chunks_exact(4).enumerate() {
        let decode = |b| match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        };
        let a = decode(c[0])?;
        let b = decode(c[1])?;
        let last = i + 1 == bytes.len() / 4;
        out.push((a << 2) | (b >> 4));
        if c[2] == b'=' {
            if !last || c[3] != b'=' || b & 15 != 0 {
                return None;
            }
            continue;
        }
        let d = decode(c[2])?;
        out.push((b << 4) | (d >> 2));
        if c[3] == b'=' {
            if !last || d & 3 != 0 {
                return None;
            }
            continue;
        }
        out.push((d << 6) | decode(c[3])?);
    }
    Some(out)
}
fn media_descriptor(part: &Value, position: usize, message: &Value) -> Value {
    let mut data = selected(part, &["type", "path", "detail", "mime_type", "mimeType"]);
    data["content_position"] = json!(position);
    data["body_in_handoff"] = json!(false);
    data["source_descriptor"] = descriptor(
        part,
        "media_reference_only",
        &format!("/content/{position}"),
    );
    data["representation_equivalence"] = json!("not_inferred_from_time_or_path");
    if let Some(v) = message.get("id") {
        data["source_message_id"] = v.clone();
    }
    if let Some(url) = part.get("image_url").and_then(Value::as_str) {
        if is_data_url(url) {
            let (header, encoded) = url.split_once(',').expect("checked data URL");
            data["source_representation"] = json!("inline_data_url");
            data["body_available_in_source_record"] = json!(true);
            data["mime_type"] = json!(
                header
                    .trim_start_matches("data:")
                    .split(';')
                    .next()
                    .unwrap_or("")
            );
            data["encoded_utf8_sha256"] = json!(sha(encoded.as_bytes()));
            if let Some(bytes) = decode_base64(encoded) {
                data["decoded_bytes"] = json!(bytes.len());
                data["sha256"] = json!(sha(&bytes));
                data["source_decode_status"] = json!("decoded");
            } else {
                data["source_decode_status"] = json!("invalid_or_over_decode_limit");
            }
        } else {
            data["source_representation"] = json!("url_reference");
            data["url"] = project(&json!(url), Limits::ARGUMENTS);
        }
    } else if part.get("path").is_some() {
        data["source_representation"] = json!("local_path_reference");
        data["file_availability"] = json!("not_checked");
    }
    data
}

#[derive(Default)]
pub struct Accumulator {
    entries: BTreeMap<String, Vec<Value>>,
    policies: BTreeMap<String, Value>,
    unknown: BTreeMap<String, Value>,
    families: BTreeMap<String, u64>,
    dispositions: BTreeMap<String, u64>,
    turns: BTreeMap<String, Value>,
    thread_usage: Option<Value>,
    messages: Vec<Value>,
    calls: HashMap<String, Vec<Value>>,
    observed: usize,
    bytes: usize,
    omitted: usize,
    unknown_overflow: usize,
    response_usage_observations: usize,
    current_turn: Option<String>,
    outputs: HashMap<String, Vec<Value>>,
}
impl Accumulator {
    pub fn observe(&mut self, record: &Value) {
        let Some(a) = record.get(ANNOTATION) else {
            return;
        };
        let f = string(a, "family").unwrap_or("unknown");
        *self.families.entry(f.into()).or_default() += 1;
        *self
            .dispositions
            .entry(string(a, "disposition").unwrap_or("unclassified").into())
            .or_default() += 1;
        let mut source = a.get("source").cloned().unwrap_or(Value::Null);
        if matches!(f, "event_msg:task_started" | "turn_context") {
            if let Some(turn) = source.get("turn_id").and_then(Value::as_str) {
                self.current_turn = Some(turn.to_string());
            }
        }
        if source.get("turn_id").is_none() {
            if let Some(turn) = self.current_turn.as_ref() {
                source["turn_id"] = json!(turn);
                source["turn_id_basis"] = json!("most_recent_explicit_task_or_turn_context");
            }
        }
        if matches!(f, "event_msg:task_complete" | "event_msg:turn_aborted") {
            self.current_turn = None;
        }
        if matches!(
            f,
            "response_item:function_call_output" | "response_item:custom_tool_call_output"
        ) {
            if let Some(id) = source.get("call_id").and_then(Value::as_str) {
                if (self.outputs.contains_key(id) || self.outputs.len() < MAX_OBSERVATIONS)
                    && self.outputs.get(id).is_none_or(|refs| refs.len() < 32)
                {
                    self.outputs
                        .entry(id.to_string())
                        .or_default()
                        .push(source.clone());
                } else {
                    self.omitted += 1;
                }
            }
        }
        if let Some(list) = a.get("unknown").and_then(Value::as_array) {
            for item in list {
                let key = format!(
                    "{}:{}:{}:{}",
                    f,
                    string(item, "kind").unwrap_or(""),
                    string(item, "path").unwrap_or(""),
                    string(item, "observed").unwrap_or("")
                );
                if !self.unknown.contains_key(&key) && self.unknown.len() >= MAX_UNKNOWN_GROUPS {
                    self.unknown_overflow += 1;
                    continue;
                }
                let row = self.unknown.entry(key).or_insert_with(
                    || json!({"family":f,"issue":item,"count":0,"sample_sources":[]}),
                );
                row["count"] = json!(row["count"].as_u64().unwrap_or(0) + 1);
                let sources = row["sample_sources"].as_array_mut().expect("sources");
                if sources.len() < 3 {
                    sources.push(source.clone());
                }
            }
        }
        if let Some(m) = a.get("message_identity") {
            if self.messages.len() < MAX_OBSERVATIONS {
                let mut m = m.clone();
                m["source"] = source.clone();
                self.messages.push(m);
            } else {
                self.omitted += 1;
            }
        }
        if matches!(
            f,
            "response_item:function_call" | "response_item:custom_tool_call"
        ) {
            if let Some(id) = source.get("call_id").and_then(Value::as_str) {
                if (self.calls.contains_key(id) || self.calls.len() < MAX_OBSERVATIONS)
                    && self.calls.get(id).is_none_or(|refs| refs.len() < 32)
                {
                    self.calls
                        .entry(id.into())
                        .or_default()
                        .push(source.clone());
                } else {
                    self.omitted += 1;
                }
            }
        }
        if let Some(list) = a.get("observations").and_then(Value::as_array) {
            for e in list {
                let kind = string(e, "kind").unwrap_or("unknown");
                let mut data = e.get("data").cloned().unwrap_or(Value::Null);
                if kind == "usage" {
                    self.usage(data, source.clone());
                    continue;
                }
                if kind == "settings" {
                    if let Some(policies) = data.as_object_mut().and_then(|m| m.remove("policies"))
                    {
                        if policies.as_object().is_some_and(|v| !v.is_empty()) {
                            let id = sha(&json_bytes(&policies));
                            data["policy_ref"] = json!(id);
                            if !self.policies.contains_key(&id)
                                && self.policies.len() < MAX_OBSERVATIONS
                            {
                                let p = project(&policies, Limits::RESULT);
                                let size = json_bytes(&p).len();
                                if self.bytes + size <= MAX_SESSION_BYTES {
                                    self.bytes += size;
                                    self.policies.insert(id.clone(), p);
                                }
                            }
                            data["policy_ref_retained"] = json!(self.policies.contains_key(&id));
                            if !self.policies.contains_key(&id) {
                                self.omitted += 1;
                            }
                        }
                    }
                }
                self.push(kind, json!({"source":source,"data":data}));
            }
        }
    }
    fn push(&mut self, kind: &str, value: Value) {
        let size = json_bytes(&value).len();
        if self.observed >= MAX_OBSERVATIONS || self.bytes + size > MAX_SESSION_BYTES {
            self.omitted += 1;
            return;
        }
        self.observed += 1;
        self.bytes += size;
        self.entries.entry(kind.into()).or_default().push(value);
    }
    fn usage(&mut self, data: Value, source: Value) {
        self.response_usage_observations += usize::from(data.get("usage").is_some());
        let id = selected(
            &data,
            &[
                "thread_id",
                "turn_id",
                "session_id",
                "response_id",
                "root_turn_id",
            ],
        );
        if let Some(usage) = data.get("turn_token_usage") {
            if let Some(turn) = string(&data, "turn_id") {
                let key = format!("{}:{turn}", string(&data, "thread_id").unwrap_or(""));
                if self.turns.contains_key(&key) || self.turns.len() < MAX_OBSERVATIONS {
                    self.turns.insert(key,json!({"scope":"turn_cumulative","identity":id,"source":source,"observation":project(usage,Limits::ARGUMENTS),"final_billing":false}));
                } else {
                    self.omitted += 1;
                }
            } else {
                self.push("usage_unkeyed",json!({"scope":"turn_cumulative","source":source,"identity":id,"observation":project(usage,Limits::ARGUMENTS)}));
            }
        }
        if let Some(usage) = data.get("thread_token_usage") {
            self.thread_usage = Some(
                json!({"scope":"thread_cumulative","identity":id,"source":source,"observation":project(usage,Limits::ARGUMENTS),"final_billing":false}),
            );
        }
    }
    pub fn finish(
        mut self,
        source_path: &str,
        messages: &[Value],
        activities: &[Value],
        effective_turns: &[Value],
    ) -> Value {
        // 用來源 ID + 原文 hash 連結，不能只因文字或時間接近而合併。
        if let Some(retention) = self.entries.get_mut("retention") {
            for row in retention {
                if let Some(refs) = row
                    .pointer_mut("/data/value/retained_message_refs")
                    .and_then(Value::as_array_mut)
                {
                    for entry in refs {
                        let id = entry
                            .get("message_id")
                            .or_else(|| entry.get("id"))
                            .and_then(Value::as_str);
                        let hash = string(entry, "text_utf8_sha256");
                        let candidates: Vec<&Value> = self
                            .messages
                            .iter()
                            .filter(|m| {
                                id.is_some()
                                    && m.get("id").and_then(Value::as_str) == id
                                    && m.get("role") == entry.get("role")
                            })
                            .collect();
                        let exact: Vec<Value> = candidates
                            .iter()
                            .filter(|m| {
                                m.get("text_utf8_sha256").and_then(Value::as_str) == hash
                                    && hash.is_some()
                            })
                            .map(|m| m["source"].clone())
                            .collect();
                        if !exact.is_empty() {
                            entry["match_status"] = json!("same_source_message_id_and_text_hash");
                            entry["matching_source_refs"] = json!(exact);
                        } else if !candidates.is_empty() {
                            entry["match_status"] = json!("same_id_different_text_not_replaced");
                        } else {
                            entry["match_status"] = json!("not_resolved_in_selected_lineage");
                        }
                        entry["adds_messages"] = json!(false);
                    }
                }
            }
        }
        if let Some(rows) = self.entries.get_mut("confirmations") {
            for row in rows {
                let id = row.pointer("/data/value/call_id").and_then(Value::as_str);
                if let Some(refs) = id.and_then(|id| self.outputs.get(id)) {
                    row["matching_answer_output_source_refs"] = json!(refs);
                    row["association_basis"] = json!("same_selected_lineage_call_id");
                } else {
                    row["association_basis"] = json!("unresolved_no_new_answer_generated");
                }
            }
        }
        if let Some(rows) = self.entries.get_mut("agents") {
            for row in rows {
                let id = row
                    .pointer("/data/observation/value/call_id")
                    .or_else(|| row.pointer("/data/observation/value/id"))
                    .and_then(Value::as_str);
                if let Some(refs) = id.and_then(|id| self.calls.get(id)) {
                    row["matching_call_source_refs"] = json!(refs);
                } else {
                    row["call_link_status"] = json!("not_resolved_no_inference");
                }
            }
        }
        // 媒體連結保守採同一 turn 內唯一的使用者訊息；多筆時不任意配對。
        if let Some(rows) = self.entries.get_mut("media") {
            for row in rows {
                let turn = row.pointer("/source/turn_id").and_then(Value::as_str);
                let candidates: Vec<&Value> = messages
                    .iter()
                    .filter(|m| {
                        m.get("role").and_then(Value::as_str) == Some("user")
                            && turn.is_some()
                            && m.get("turn_id").and_then(Value::as_str) == turn
                    })
                    .collect();
                if candidates.len() == 1 {
                    if let Some(id) = candidates[0].get("id") {
                        row["handoff_message_ref"] = id.clone();
                        row["association_basis"] =
                            json!("unique_exported_user_message_in_same_turn");
                    }
                } else {
                    row["association_basis"] = json!("source_message_and_position_only");
                }
            }
        }
        let effective: Vec<&str> = effective_turns
            .iter()
            .filter_map(|v| v.get("turn_id").and_then(Value::as_str))
            .collect();
        // 觀測歷史不是有效 messages；回滾的 turn 不得被當成有效工作成果。
        for rows in self.entries.values_mut() {
            for row in rows {
                if let Some(turn) = row.pointer("/source/turn_id").and_then(Value::as_str) {
                    let status = if effective.contains(&turn) {
                        "present_in_exported_turns"
                    } else {
                        "not_in_exported_turns"
                    };
                    row["turn_membership"] = json!(status);
                }
            }
        }
        let mut tool_result_limits = Vec::new();
        for activity in activities {
            if let Some(envelope) = activity.get("result_envelope") {
                let status = string(envelope, "status").unwrap_or("unknown");
                let unknown = envelope
                    .get("unknown_envelope_fields")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty());
                if unknown || !matches!(status, "parsed" | "text_fallback") {
                    tool_result_limits.push(json!({"source_item_id":activity.get("source_item_id"),"parser":envelope.get("parser"),
                        "status":status,"unknown_envelope_fields":envelope.get("unknown_envelope_fields"),
                        "source_error":envelope.get("source_error")}));
                }
            }
        }
        let observation_projection_limits = self
            .entries
            .values()
            .flatten()
            .map(projection_limit_count)
            .sum::<usize>()
            + self
                .policies
                .values()
                .map(projection_limit_count)
                .sum::<usize>();
        let attachment_links: Vec<Value> = messages.iter().flat_map(|message| {
            message.get("attachments").and_then(Value::as_array).into_iter().flatten().enumerate().map(move |(index, attachment)| {
                json!({"handoff_message_ref":message.get("id"),"attachment_position":index,
                    "metadata":selected(attachment,&["attachment_id","filename","restoration_source","restoration_status","sha256","byte_length","character_length",
                        "verification_method","attachment_file_status","tool_output_comparison","tool_candidate_count",
                        "tool_candidate_sha256","additional_tool_candidate_count","unverified_tool_evidence"]),
                    "preservation_basis":"existing_attachment_restoration_contract_no_new_file_reads"})
            })
        }).collect();
        let activity_projection_limits = activities
            .iter()
            .filter(|activity| {
                projection_limit_count(activity) > 0
                    || activity
                        .pointer("/result_summary/truncated")
                        .and_then(Value::as_bool)
                        == Some(true)
                    || activity
                        .get("omitted_nontext_items")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        > 0
            })
            .count();
        json!({"source_observations":{
            "scope":"selected_logical_lineage_local_history_observations_not_messages",
            "source_path":source_path,"locator_basis":"logical_snapshot_lines_not_physical_file_offsets",
            "policies":self.policies,"events":self.entries,"attachment_links":attachment_links,
            "usage":{"turn_last_observations":self.turns.into_values().collect::<Vec<_>>(),"thread_last_observation":self.thread_usage,
                "response_observations_summarized":self.response_usage_observations,"scopes_are_not_additive":true}},
            "schema_coverage":{"schema_version":VERSION,"observed_layout_baseline":"2026-10-29",
                "monitor_scope":"native_shell_fields_types_families_and_selected_enums_not_arbitrary_tool_business_data",
                "semantic_completeness":"not_certified_by_export_success","record_families":self.families,
                "record_dispositions":self.dispositions,"unknowns":self.unknown.into_values().collect::<Vec<_>>(),
                "unknown_occurrences_beyond_group_limit":self.unknown_overflow,"observations_omitted_by_limits":self.omitted,
                "activities_with_bounded_projection":activity_projection_limits,
                "supplementary_projections_with_limits":observation_projection_limits,
                "tool_result_limits":tool_result_limits,
                "limits":{"event_count":MAX_OBSERVATIONS,"event_and_policy_bytes":MAX_SESSION_BYTES,"unknown_groups":MAX_UNKNOWN_GROUPS},
                "scope_notes":["Guardian exclusion and project membership remain unchanged","Plain text and task-specific JSON are not recursively interpreted","Source hashes are references, not copies of omitted content"]}})
    }
}

pub fn set_line(record: &mut Value, line: usize) {
    if let Some(source) = record
        .get_mut(ANNOTATION)
        .and_then(|a| a.get_mut("source"))
        .and_then(Value::as_object_mut)
    {
        source.insert("logical_snapshot_line".into(), json!(line));
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;

pub fn coverage_counts(handoff: &Value) -> (u64, u64) {
    let coverage = handoff.get("schema_coverage").unwrap_or(&Value::Null);
    let mut unknown = coverage
        .get("unknowns")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.get("count").and_then(Value::as_u64))
                .sum()
        })
        .unwrap_or(0_u64);
    unknown += coverage
        .get("unknown_occurrences_beyond_group_limit")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut limited = coverage
        .get("observations_omitted_by_limits")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + coverage
            .get("activities_with_bounded_projection")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    limited = limited.saturating_add(
        coverage
            .get("supplementary_projections_with_limits")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    limited = limited.saturating_add(
        coverage
            .get("tool_result_limits")
            .and_then(Value::as_array)
            .map(|v| v.len() as u64)
            .unwrap_or(0),
    );
    if let Some(children) = handoff.get("subagents").and_then(Value::as_array) {
        for child in children {
            let (u, l) = coverage_counts(child);
            unknown = unknown.saturating_add(u);
            limited = limited.saturating_add(l);
        }
    }
    (unknown, limited)
}

fn projection_limit_count(value: &Value) -> usize {
    match value {
        Value::Array(array) => array.iter().map(projection_limit_count).sum(),
        Value::Object(map) => {
            if map.get("hash_encoding").and_then(Value::as_str) == Some("serde_json_compact_utf8") {
                usize::from(map.get("truncated").and_then(Value::as_bool) == Some(true))
            } else {
                map.values().map(projection_limit_count).sum()
            }
        }
        _ => 0,
    }
}
