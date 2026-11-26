# Codex Conversation Handoff Exporter

將本機 Codex 對話匯出為 `.handoff.json` 的 Windows x64 工具，支援整個專案與獨立對話。提供互動式主控台、即時進度、原生資料夾選擇器及 CLI 自動化匯出。

本工具是非官方社群專案，與 OpenAI 無隸屬關係，亦未獲官方背書。

## 下載與執行

在本儲存庫的 [Releases](https://github.com/SunnyLeu/Codex-Conversation-Handoff-Exporter/releases)
下載 Windows x64 套件：

`Codex-Conversation-Handoff-Exporter-2.3.1-windows-x64.zip`

解壓後執行 `Codex-Conversation-Handoff-Exporter.exe`。
EXE 不依賴固定安裝目錄、捷徑的「開始位置」或相鄰資源檔，可以移至自己偏好的位置。
一般使用不需 Python、Rust、Windows SDK 或 .NET；C/C++ 執行階段及 Cargo 管理的 Zstandard 解碼器採靜態連結。
Windows 自帶的系統 DLL 仍為必要元件。重新散布時請保留套件內的授權聲明。

**平台範圍：僅支援 Windows x64（x86-64／AMD64）。**
Windows 11 x64 為主要驗證環境；請以對應 Release 的正式驗收紀錄為準。不發布 Windows 32 位元、ARM64、Linux 或 macOS 版本。
其他 Windows 版本尚未完成驗證。EXE、Codex 資料、系統暫存目錄與輸出位置仍受 Windows 存取權限限制。

## 基本操作

1. 沒有指定來源時，以數字選擇一個側邊欄專案或獨立對話，再按 Enter。
2. 沒有指定目的地時，確認頁等待 3 秒，自動開啟一次資料夾選擇器。
3. 選好資料夾後，輸入 `E` 並按 Enter 才開始匯出；完成後輸入 `Q` 並按 Enter 離開。

| 指令 | 功能 |
|---|---|
| 數字＋Enter | 選擇清單項目 |
| `F`＋Enter | 選擇／更換輸出資料夾 |
| `E`＋Enter | 確認開始匯出 |
| `B`＋Enter | 返回來源選單 |
| `Q`＋Enter | 在選單、確認或結果頁離開 |

英文字母不區分大小寫。取消資料夾選擇不會開始匯出或反覆彈窗。
即使來源與目的地均由參數指定，預設仍須 `E`＋Enter。
匯出進行中不提供 `Q` 中止，以免產生不完整結果。

## 命令列與捷徑

在 PowerShell 中，將範例的 ID 換成自己的 Codex ID；它不是專案名稱或目錄名稱。

```powershell
# 來源、目的地已指定，仍保留人工確認。
.\Codex-Conversation-Handoff-Exporter.exe `
  --project-id "YOUR_PROJECT_ID" `
  --output-directory "C:\Exports\MyProject"

# 只指定一條獨立對話，目的地由資料夾選擇器取得。
.\Codex-Conversation-Handoff-Exporter.exe --thread-id "YOUR_THREAD_ID"

# 明確免確認。來源與目的地必須同時提供。
.\Codex-Conversation-Handoff-Exporter.exe `
  --project-id "YOUR_PROJECT_ID" `
  --output-directory "C:\Exports\MyProject" `
  --yes

# 出現錯誤時保留完整 DEBUG JSONL；正常成功不產生日誌。
# 自動化匯出仍可使用 --yes，無需另外指定 DEBUG 參數。
.\Codex-Conversation-Handoff-Exporter.exe `
  --project-id "YOUR_PROJECT_ID" `
  --output-directory "C:\Exports\MyProject" `
  --yes --no-pause

# 唯讀列出登錄的專案 ID（可能包含未出現在側邊欄的專案）。
.\Codex-Conversation-Handoff-Exporter.exe --developer-mode --list-projects

.\Codex-Conversation-Handoff-Exporter.exe --help
.\Codex-Conversation-Handoff-Exporter.exe --version
```

Windows 捷徑的「目標」可使用：

```text
"C:\Tools\Codex-Conversation-Handoff-Exporter.exe" --project-id "YOUR_PROJECT_ID" --output-directory "C:\Exports\MyProject"
```

這些路徑只是範例。來源 ID 和目的地可以分別提供；缺少的資訊由互動介面取得。
`--project-id` 與 `--thread-id` 互斥，一次只能匯出一個目標。
`--yes` 不會略過來源、續接或輸出目錄的安全檢查。
非互動環境必須使用完整參數及 `--yes`，不會悄悄選用固定專案。

進階診斷以 `--developer-mode --help` 查看。
只有明示 `--developer-mode` 才啟用工程 CLI；正式寫入仍須明確來源與目的地。
一般模式會忽略已知的無害調校旗標，但對 `--no-write`、資料來源覆寫等具有行為影響的開發者旗標安全拒絕，不會忽略後意外改成正式寫入。

## 資料來源與匯出內容

預設讀取目前使用者的 `%USERPROFILE%\.codex`；
有設定絕對路徑的 `CODEX_HOME` 時優先使用該位置，與 EXE 所在位置無關。

首頁選單只讀側邊欄狀態、必要標題及專案登錄資料，不預先解析整個專案。若有 Codex SQLite Thread 資料庫，首頁亦以唯讀查詢排除已不在資料庫或已封存的獨立對話。

專案登錄同時支援全域 JSON 與新版 SQLite。能以專案 ID 或既有 Legacy／App Server ID 映射確認為同一專案時，使用 SQLite `project_roots` 的目前路徑及順序，不重新加入已移除的 JSON 舊路徑。SQLite 查無非空的 Thread 專案指派不會抹除既有 JSON 明確指派；兩份明確指派互相矛盾時不猜測歸屬。只在無法證明明確指派且工作目錄是專案獨占根目錄時採用 CWD 比對，仍拒絕無法判定歸屬的根對話。
沒有新版 SQLite 專案資料表時沿用 JSON 相容模式；沒有對應 SQLite 專案列的舊 JSON 登錄保留，不以缺列推定刪除。新版 SQLite 專案查詢從隔離的資料庫／WAL 快照進行，不在來源目錄建立 SQLite 輔助檔。
按 `E` 後才驗證選定目標及讀取 Session。側邊欄狀態、索引或本機紀錄缺失時，
程式會明確失敗，不會猜測其他 ID、固定路徑或對話歸屬。

- **專案：** 匯出該專案符合成員規則的全部主對話，不限於側邊欄當下展開的幾條。
- **獨立對話：** 只匯出該邏輯主對話，不提供專案內任意單條對話的首頁選項。
- 支援 `.jsonl`、`.jsonl.zst`、Worktree，以及可驗證的 `history_base` 多檔續接。
- 真正子代理及可辨識附件沿用匯出器的整合規則；Guardian 不當成一般主對話。
- 不讀取封存目錄來擴大匯出範圍，不會把磁碟裡的所有殘留 Session 列為可選。

每條邏輯主對話產生一份 JSON，保留對話 ID、訊息及相關匯出資料。
Handoff 不是 Codex 原始資料夾備份，也不是可匯入 Codex 的官方格式。
它可能包含完整對話、工具輸出、附件內容及本機路徑，分享前請自行檢查。

## 檔名與既有輸出

專案檔名一律帶數字前綴，位數由本次主對話總數決定：1～9 份為 `1.`，
10～99 份為 `01.`，100～999 份為 `001.`。只有一條對話的專案仍保留 `1.`。
獨立對話不加編號。兩種模式均保留標題清理及 GMT+8 更新時間戳規則。

每次匯出會完整重新產生本次目標的 Handoff，並整理屬於同一目標的過期輸出。
選擇的資料夾就是實際輸出位置，不會自動新增專案子資料夾。
同一個輸出資料夾可以保存其他對話（包含已從 Codex 刪除的對話）的舊 Handoff；只要檔案不是本次預定寫入或清理的目標，程式不會因對話 ID 不同而拒絕匯出，也不會擅自覆寫、移動或刪除它們。
若本次預定的實際輸出路徑已被**其他 `conversation_id`** 占用，程式會拒絕覆寫並提示衝突位置。即將清理的本次目標舊 Handoff 必須通過完整 JSON、身分與安全路徑檢查；無關檔案即使損壞，也不會被本次匯出修改或清理。沒有「強制覆寫所有檔案」選項。
不修改 Codex 原始來源資料。

## 從原始碼建置

請在 Windows x64 安裝 **Rust 1.100.0 或更新版本的 MSVC 工具鏈**，
以及 Visual Studio C++ Build Tools（MSVC x64 工具集與 Windows SDK）。
專案使用 `rust-toolchain.toml` 固定 Rust 1.100.0；如果改用其他版本，請重新執行相容性測試，並核對相應標準函式庫授權聲明。

在含有 `Cargo.toml` 的專案根目錄執行：

```powershell
cargo build --release --locked
cargo test --locked
```

首次建置需要網路取得 `Cargo.lock` 鎖定的相依套件。Zstandard 原生 C 程式碼由 Cargo 的 `zstd-sys` 相依套件提供並靜態編譯，不需要本儲存庫內的 `third_party/`。
公開來源不包含 `vendor/` 或 `third_party/`，也不需要本專案的內部 Python／BAT 驗收工具。
`.cargo/config.toml` 選定 `x86_64-pc-windows-msvc` 並啟用靜態 CRT，
不使用 `target-cpu=native`。不要移除這個設定後仍假定執行檔沒有 VC Runtime 相依。

產物位於：

```text
target\x86_64-pc-windows-msvc\release\Codex-Conversation-Handoff-Exporter.exe
```

## DEBUG 診斷

在程式啟動時於記憶體建立完整事件追蹤，涵蓋命令列選項、來源與目的地檢查、身分判斷、檔案處理、錯誤原因鏈與可取得的呼叫堆疊。當執行過程發生錯誤（即使後續恢復成功），才將先前及後續事件保存為 `%TEMP%\CodexHandoff-Diagnostics\v2.3.1-*.debug.jsonl`；完全沒有錯誤時不建立 DEBUG 檔案。程式不巡覽、不刪除歷史日誌，也不設自動清理期限。

DEBUG 檔可能包含本機完整路徑、對話 ID、原始錯誤內容及相關資料。匯出失敗時，主控台會提供原因與診斷檔位置；使用 `--yes` 時不等待按鍵，回傳非零結束碼、stderr 和 `V2_RESULT` 失敗收據。

## 限制與疑難排解

**Codex 正在執行任務時能匯出嗎？** 不需要先關閉 Codex。
匯出結果對應本輪實際讀到且通過檢查的資料，不保證涵蓋匯出結束後才新增的訊息，
也不宣稱跨所有檔案具備單一時間點的原子備份。
資料改寫或來源身分衝突仍可能讓本輪安全停止。

**為什麼選單有項目，匯出時卻失敗？** 選單是側邊欄候選清單；
原始紀錄可能尚未落盤、已缺失，或來源格式不相容，正式解析在確認開始後才執行。

**程式很快關閉／自動化卡住？** 在 PowerShell 中執行可保留輸出。
無人值守使用 `--yes` 並明確提供來源與目的地；正常互動模式在結果頁等待 `Q`＋Enter。

**執行時間超過 3 秒算失敗嗎？** 不算。一般模式顯示實際耗時；
成功為結束碼 0，參數錯誤或真正匯出錯誤為非零。

Codex 本機狀態檔不是本專案控制的穩定公開介面，未來版本改變可能需要更新解析器。
寫入途中遇到磁碟／權限錯誤時，不保證整批回滾，請依錯誤訊息檢查可能的部分輸出。
未簽章執行檔可能受到 Windows 安全性原則攔截；請確認下載來源及 SHA-256，不要關閉系統防護來繞過未知警告。

## 授權

主程式：[MIT](LICENSE)。相依套件、Zstandard 與 Noto Emoji 圖示保留原授權，
詳見 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
