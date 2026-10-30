# 2.1.0 輸出契約

相對基線為 2.0.3 / c0d30b6d86d33e8a80bb6bf7785b6a664fdf7a97。R9 已在 Windows x64 完成 241 項 Rust、既有 7 組 EXE 與 1 組子代理 EXE、42 項 Schema 事實及附件／快照檢查。這是已觀測來源格式的工具輸出契約，不是完整無損保證。

## 1. 相容與範圍

既有 messages、turns、activities、requests、子代理、附件還原、專案歸屬與 N/N 重寫維持原有流程。新增 `handoff_schema_version="1.0"`；`exporter_version="2.1.0"` 仍是程式版本。子代理各自保存 local-history 的來源觀測；不因新欄位加入 Guardian 為一般對話、不重新啟用封存目錄或從別條對話複製 messages。

這不是所有 Codex 版本的完整標準，也不是任意 JSON 的無損備份。以 2026-10-29 提供的原始樣本建立已觀測版型，未知內容維持可見且待評估。

## 2. 原生值的有界保存

`input_summary`、`structured_result`、`error_detail`、`action_parameters` 及其他觀測中的 `projection` 使用相同包裝：

- `original_type`：來源原生型別；不是以字串取代物件或陣列。
- `value_retained`／`value`：值是否有保存。來源 `null` 與不存在的欄位不同；完全省略時不製造假的 `null`。
- `truncated`／`omitted_binary`：有界截短或明示二進位省略。
- `source_json_sha256`／`hash_encoding`：以 serde_json 壓縮 JSON 序列化計算，不是原始行的 exact bytes，也不是 RFC 8785。
- `omissions`：JSON Pointer、原因、原型別、大小／長度及雜湊；最多 128 筆，其餘用 `additional_omission_count` 計數。字串保留 Unicode 字元前綴；陣列只保留前綴，不刪中間項後改變索引。

| 邊界 | 一般參數 ARGUMENTS | 原生結果 RESULT |
|---|---:|---:|
| 保留值 JSON UTF-8 bytes | 8,192 | 65,536 |
| 每字串字元 | 2,048 | 4,096 |
| 節點 | 256 | 2,048 |
| 深度 | 10 | 16 |
| 每物件／陣列項目 | 64 | 128 |

大小預算是 `value`，不含來源雜湊及省略描述。明示 base64 data URL，或帶編碼／媒體型別的 `data` 值，保留描述而非本體。僅有 `screenshot`、`data`、`image`、`_meta` 等鍵名不足以刪除內容。無明示編碼的一般文字不按鍵名推定為二進位。

## 3. 工具、命令與網頁

MCP `result.structuredContent` 保留原生結構；`item.error` 保留 `error_detail`。結果文字仍沿用有界 `result_summary`。非文字結果以 `nontext_result_descriptions` 描述；不另外讀取其路徑或 URL。

失敗命令的 diagnostics 分別識別 stdout、stderr、aggregated_output；合併輸出若與某一原 stream 完全相同，使用 `diagnostic_aliases` 指向它，不重複文字。不把 aggregate 當成 stderr；成功命令提到歷史錯誤不會被改判失敗。既有 4,096 字元診斷上限維持。

Web 原 action 字串保留；openPage／open_page 對應 open_count，findInPage 對應 find_count。action_parameters 保留有界原生參數；未知動作仍保存原字串並提示，而非猜成已知行為。

## 4. 字串包住的固定工具外層

只辨識 producer 為 `codex_app` 且工具為下列名稱者：

| 工具 | 接受的已觀測外層 |
|---|---|
| open_in_codex | status 與 threadId 字串 |
| read_thread | schemaVersion 1；thread／page 物件，turns 陣列 |
| list_threads | schemaVersion 4；threads 陣列 |
| list_projects | schemaVersion 2；projects 陣列 |

read_thread 可解開一層已觀測的單一 content/text wrapper；不修補來源非法 JSON，不繼續解析游標、程式碼或工具產物。先解析完整來源外層，再做有界投影；讀到的其他 thread 的 turns 不加入目前 messages，使用 excluded_result_sections 留下來源描述。

`result_envelope.status` 說明 parsed、text_fallback、source_not_parseable、source_inner_not_parseable、unrecognized_transport_shape 或 unrecognized_version_or_shape。未知外層欄位若形狀其餘成立可保留，但列出 unknown_envelope_fields。原工具 status 與內層 status 各自保存；completed 不覆蓋 queued。無法解析時保留原有有界文字摘要及來源 hash，而非以空物件冒充完整結果。

## 5. source_observations

這是選定 lineage 的來源觀測，不是新的使用者訊息或有效工作成果。

- 設定／權限：turn_context、configuration_update、thread_settings_applied 按來源順序保存；保留 client／harness 作者標記。路徑權限以精確值雜湊去重，`policy_ref_retained=false` 明示引用因限制未保存；不把不同大小寫／文字擅自當成相同權限。
- 關聯：保存 parent/root turn、turn_trigger、外層 metadata 等原值。不推論 max/xhigh 的運算等價性，也不替來源補時間。缺 turn ID 而採最近明示 task/turn 時，記錄推定依據。
- 子代理：typed SubAgentActivity、CollabAgentToolCall 及通訊 metadata 保存為觀測。只以明示 call ID 連結；空 wait 接收清單不猜代理名單，工具回傳不替代真正 child 完成狀態。
- 保留／壓縮：retained message 記錄來源 ID、role、文字 hash、phase／revision（來源有提供時）及完整性旗標。完全匹配來源 ID+role+hash 才建立相符參照；同 ID 不同文字、不完整與未解關係維持原狀，不覆寫或新增 messages。replacement_history 只保留描述與身分，不再當新對話輸入。encrypted_content 只存不可讀描述。
- verified answer：以同一選定 lineage 的 call_id 連到既有 output source；未解則明示。此連結不是另外認證答案正確，也不複製答案。
- 媒體：保留原生圖片／音訊種類、位置、MIME、來源與可用性。inline image data URL 可在 24 MiB 編碼上限內解碼後計算本體雜湊，但 Handoff 不內嵌 base64，也不聲稱驗證了圖片有效性。路徑／URL 不讀取、不下載；圖片與訊息連結採同 turn 唯一 user 的保守規則，歧義只留來源位置。
- 附件（R5 修訂）：只讀取選定 Handoff 已引用的 `附件 ID／檔名`，並先核對可取得的附件檔案原始位元組；原件是 UTF-8 時，直接以它還原正文，SHA-256 必須等於原件 bytes，包含 BOM、CRLF。若工具文字與原件不同，不從字串猜測如何切除錯誤前綴，改保存 `tool_output_comparison=mismatch` 及工具候選雜湊。
- 附件原件缺失或不能安全解碼時，`restoration_status` 使用 `tool_output_unverified`、`content_conflict`、`missing`、`decode_error`、`unreadable` 或 `unsafe_path`，不以未驗證工具文字冒充使用者附件正文；保留原始傳輸參照，將可用的候選有限節錄留在 `unverified_tool_evidence`。`verification_method=attachment_file_bytes` 僅在已從被選定的附件檔案讀取並還原時成立，並不證明歷史來源在更早時點從未變動。
- 附件來源對照（R7 修訂）：`source_observations.attachment_links[*].metadata` 必須同步反映已選定訊息附件的 `verification_method`、`attachment_file_status`、`tool_output_comparison`、工具候選數量／hash，以及必要的有界 `unverified_tool_evidence`。這是對已核對 metadata 的輸出投影，不會額外讀取附件或從雜訊重建原文；缺少原件時不得出現已驗證原文的 `sha256`。
- 附件實際讀取時仍由 target-scoped 來源保護器記錄前後一致性，不掃描別的專案或未引用附件；拒絕子目錄或檔案重新解析點，不因單個工具輸出返回成功就證明精確附件還原。
- 用量：每個 thread/turn 保存最後一個 turn cumulative，另保留最後 thread cumulative；不相加重疊 scope，不把 null 當 0，不聲稱最終帳單。逐 response usage 只計觀測次數，**沒有全部個別用量明細**。
- 工具定義／環境／時鐘：必要身分與有界描述分開；不把完整工具定義、world_state 或 reasoning 當一般訊息。

來源定位以選定邏輯快照行號及去除行尾的原始行 SHA-256；經 history_base 重建者不能將此行號誤用為某個實體檔案 offset。時間、ordinal 以原值保留。

每 Session 補充 events 最多 4,096 筆，events+policies 共 16 MiB。其他訊息身分索引、call/output 索引、turn 用量等各有獨立 4,096 項上限，call/output 每 ID 最多 32 個參照。**16 MiB 不是整份 Handoff 的總上限。** 超限有計數；既有 messages 不受此新補充觀測限額截斷。

## 6. schema_coverage 與完成提示

本批 42 種來源家族、726 個 native shell 路徑／型別條目為觀測基準，集中在頂層、payload、typed item、結果外層與必要內容節點。未知 family／field／type 及選取的 enum 會計數並最多提供三筆來源例；最多 512 個未知群組，超額仍計數。

每 record 最多列舉 64 個未知問題；native array 掃描最多 128 項，超過另報 scan_limit。任意 arguments、structuredContent 的業務鍵，以及使用者／助手文字內的自訂 JSON 不被掃描成通用 native schema。`record_dispositions` 是來源路由類別，不是逐欄位的完整性證書。

`semantic_completeness` 固定為 `not_certified_by_export_success`。記錄未知、保留限制與具名工具解析失敗；V2_RESULT 增加 schema_unknown_occurrences／schema_limited_observations，Console 也提醒範圍。未知資料不自動提升成新正式欄位，也不因出現就將正常安全匯出判成失敗。

## 7. 驗收範圍與尚存界線

R9 的 Windows x64 完整驗收已通過：241 項 Rust 單元測試、原有 7 組 EXE E2E、增加的 1 組子代理 EXE E2E、42 項 Schema 事實與附件／快照核對。實際來源對照已確認先前 38 筆附件污染不再出現，原本正確的 109 筆亦維持正確；兩份先前錯判為無可匯出訊息的真實子代理已在另一份新匯出中收錄。這些證據均屬所測來源與版本的有限範圍，不構成所有版本、檔案與未來欄位的完整認證。

若來源附件原件不存在或不可讀，不能藉由工具候選文字推斷原件位元組。`source_snapshot` 是每份來源檔案的固定讀取邊界，不代表多檔案在同一原子時間點同步擷取。來源觀測受明示大小預算限制，保留／壓縮等語意可能受來源資訊不足限制。真實世界的效能與檔案體積依 Session 大小而異。


## R8 source_snapshot 欄位與快照語意

Root Handoff 與有實體來源的 Subagent 可包含 `source_snapshot`，其中 `mode=selected_physical_files_with_fixed_byte_limits`，`physical_files[*]` 帶有 `path`、`captured_bytes`、`source_sha256` 及 `byte_scope`。`physical_jsonl_prefix` 表示限制讀取到已選實體 `.jsonl` 的固定 byte 位置；`compressed_physical_file` 表示已核對的 `.jsonl.zst` 完整檔案。`later_appends_and_new_files=excluded_until_next_export`，不應解讀為 Codex 已停止寫入。`consistency=per_file_verified_prefix_not_atomic_cross_file_snapshot` 表示各實體檔案的前綴獨立驗證，不保證跨檔案原子時間點。`source_sha256` 是來源檔起始截取 bytes 的身分，不是 Handoff 訊息或整份多檔原子快照的雜湊。
