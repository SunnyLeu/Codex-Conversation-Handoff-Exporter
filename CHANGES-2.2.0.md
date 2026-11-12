# 2.2.0 變更說明

基準為正式版本 2.1.0（Git commit `e6844394b52e5270dbe9321672ea2a870d3606b8`）。2.2.0 已通過 Windows R4 自動化驗收與指定真實專案的效能實測；GitHub Tag／Release 的正式發布狀態以儲存庫公開紀錄為準。

## 1. Codex 非同步問答整理

- 辨識經 `item_completed.UserMessage` 確認的 `<send_user_message_question_reply>` 格式，逐項解碼 `questionItemId`，以 `request_user_input_async` 的 call ID、題目索引及**完整題目文字**三者精確比對。
- 對已確認的回答於 `user_input_requests[*].responses` 保留 `question_index`、`question`、`answer`、`source_message_id`、`source_item_id` 與可取得的 `answered_at`。
- 完整回答時設為 `answered`；只確認部分題目設為 `partially_answered`；同一題有互斥回答則設為 `response_conflict` 並記下 `conflicting_question_indexes`。未確認的請求維持原狀；僅有 `accepted=true` 的工具回執仍不是回答。
- 保留原本 `messages[].content`、原始來源 ID、時間與回答字串，不改寫為推測的自然語言。不能解析、問題文字不符、來源非驗證 UserMessage、不是 async 工具或要求已被拒絕時，不建立未經證實的答案關聯。
- 這是額外的結構化關聯；`handoff_schema_version` 維持 `1.0`，原欄位不刪減。

## 2. 效能與觀測

- 既有 Handoff 的 JSON 安全檢查改為低配置頂層身分讀取，仍需完整解析並拒絕損壞、不安全或歸屬錯誤的輸出。
- 來源固定前綴擷取和寫入前重驗使用有界 I/O 平行處理（最多四個 worker），不取消 SHA-256、檔案長度與專案身分安全檢查。
- 產品模式於工作執行緒池尚存時釋放已輸出的候選 JSON；開發者的語意比較模式仍可取得完整候選。
- 增加來源快照、續接準備、來源重驗、目的地檢查、候選清理及進度執行緒結束計時；進度執行緒可被喚醒以結束等待。
- **完整重建、每次重寫全部選定 Handoff、不中斷 Codex、目的地安全保護與來源觀測限制均維持。**

## 3. 驗證結果與限制

- Windows 11 x64、Rust 1.98.0、MSVC Release 建置通過；246 項 Rust 單元測試、8 組合成 EXE 端到端測試、子代理整合、42 項 Schema 事實及 13 項附件檢查通過；合格 EXE 已完成獨立 SHA-256 身分核對。
- Linux x64 Rust 1.98.0 離線開發驗證：247 項 Rust 單元測試通過。
- 在一份真實 Codex rollout 的隔離匯出中，兩個 `request_user_input_async` 各關聯 3 與 1 個已驗證的回答；原始 UserMessage 完整保留。合成 Windows E2E 另覆蓋已回答、未回答與來源身分檢查。
- 在兩個真實 Windows 專案中，使用 2.1.0 與 2.2.0 各六次正式覆寫量測（另有預熱），平均外部耗時分別降低 23.50% 與 24.08%。這是所測資料、磁碟快取與背景負載條件下的觀測，不是普遍效能保證。
- 尚未取得所有真實 Handoff 的逐份 JSON 語意比對結果，因此不宣稱已對完整資料集證實所有內容完全一致；2.2.0 的新增結構化回答欄位本來就可能造成預期的輸出差異。
- `handoff_schema_version` 仍為 `1.0`；既有來源完整性、目的地安全拒絕及來源不完整時的誠實標示不變。
