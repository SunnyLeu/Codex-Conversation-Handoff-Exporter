# 2.3.0 變更說明

基於正式發布的 2.2.0，整合舊 Handoff 安全共存、完整錯誤追蹤、Rust 工具鏈與相依套件更新，以及 Dependabot 版本維護設定。

## 匯出與目的地安全

- 允許不同 `conversation_id`、不同實際輸出檔名的舊 Handoff 與新對話共存，即使舊對話已從 Codex 刪除。只管理本次選定目標的檔案；不自動移除其他對話資料。
- 真正的同路徑／不同對話 ID 碰撞仍安全拒絕；對本次可能清理的舊檔執行完整 JSON 與對話身分檢查，不以唯讀前綴索引作為刪除授權。
- 維持 Codex 原始來源快照、來源改寫／截斷防護、附件原文核對、子代理與續接整合，以及原有 `handoff_schema_version: "1.0"`。

## 錯誤診斷與交互行為

- 每次執行從 CLI 解析前開始保留記憶體事件追蹤；只有發生可控錯誤才保留完整 DEBUG JSONL，無錯誤則不寫入診斷檔。不掃描、刪除或過期清理歷史日誌。
- 錯誤事件保留詳細階段、操作、路徑、ID、錯誤鏈及可取得的 I/O 錯誤碼／backtrace；互動模式顯示錯誤頁，`--yes` 模式保留非零結束碼、stderr 與結構化失敗收據。

## 工具鏈與相依套件

- 最低支援 Rust 改為 1.100.0，新增 `rust-toolchain.toml` 並更新 Windows CI 的對應工具鏈檢查。
- 更新直接及間接 Cargo 相依套件，使用完整鎖定的 `Cargo.lock` 與 128 個第三方套件；包含 `sonic-rs`、`zstd`、`regex`、`clap` 等更新。未因效能猜測而更改輸出語義或放寬安全檢查。
- 更新 Rust 標準函式庫與第三方授權聲明，包含新版 Zstandard BSD-2-Clause／BSD-3-Clause 聲明。
- Dependabot 每日追蹤 `rust-toolchain.toml`、直接及間接 Cargo 相依套件和 GitHub Actions。Minor／Patch 可分組，Major 更新獨立交付 PR；不自動合併或發布。

## 驗收要求

- 使用 Rust 1.100.0 與更新後的鎖定相依套件重新執行完整 Windows x64 MSVC Release 建置、252 項 Windows Rust 測試、11 組合成 EXE 測試、Schema 42 項、子代理與附件／來源安全驗證。
- Linux 開發與真實資料測試不等於 Windows 正式驗收；實際結果以該次 Windows 驗收收據為準。Rust 1.100.0 與相依套件的效能收益也須以同條件 A/B 實測確認。
