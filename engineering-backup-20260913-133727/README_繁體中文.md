# MCP-Local 本機檔案唯讀工具

限定專用資料夾內的 UTF-8 文字及程式碼；提供八個唯讀工具：`list_directory`、`list_files`、`read_file`、`search_text`、`workspace_info`、`list_projects`、`project_context`、`read_files`。不提供寫入、刪除或指令執行能力。

## 環境與啟動

驗證環境為 Windows、Python 3.10.11、MCP 1.30.0。現有相依套件未升級；完整版本在 `requirements-lock.txt`。

```powershell
py -3.10 -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements-lock.txt
.\.venv\Scripts\python.exe -m pip check
.\.venv\Scripts\python.exe -B -m unittest discover -s tests -v
.\.venv\Scripts\python.exe -m py_compile local_files_mcp.py connection_settings.py connection_cli.py local_files_gui.py reader_settings.py key_store.py tray_windows.py workspace_settings.py workspace_reader.py
```

本機 STDIO：`.\.venv\Scripts\python.exe local_files_mcp.py --root .\shared`。預設根目錄為程式旁的 shared；不會自動使用 GUI 設定。

遠端通道：雙擊 `開啟連線設定.vbs` 或執行 `.\start-local-files-tunnel.ps1`。兩者共用相同設定、指令建立與程序管理。需已有經驗證的官方 `tunnel-client.exe` 及具有所選通道使用權限的金鑰；不變更供應商成品。

首次遷移必須在 GUI 選定實際分享目錄與通道，按「儲存設定」確認兩個入口今後採用的值。未確認前，PowerShell 與連線流程均拒絕啟動。保存前備份原一般設定；不更動既有加密金鑰。詳見 [介面使用說明](介面使用說明.md)。

## 大量檔案讀取

1. 先用 `list_directory(directory=".", limit=200)` 查看直接子目錄及允許格式檔案。
2. 選擇相關子目錄，用 `list_files` 列檔或 `search_text` 找關鍵字。
3. 用 `read_file(path, start_line, line_count)` 分段讀取命中處。
4. 只有需要完整清冊時，才將每頁的 `next_cursor` 傳入下一次的 `cursor`。

兩個清單工具的 limit 均為 1 至 500，預設 200。list_files 保留原 files 欄位；list_directory 使用 entries，每筆含 path、name、type。其餘欄位：

| 欄位 | 意義 |
| --- | --- |
| returned_count | 本頁筆數 |
| has_more / next_cursor | 已掃描集合是否仍有下一頁及續讀游標；最後一頁為 false / null |
| scan_truncated | 安全掃描上限造成集合不完整；須縮小 directory 或調整掃描設定 |
| truncated | 相容欄位，等於 has_more 或 scan_truncated |
| skipped_entries | 存取、安全規則、格式或競態造成的略過數 |
| scanned_entries / sorted_entries / elapsed_ms | 本次掃描項目數、排序集合大小與耗時；僅統計資料 |

結果依相對 POSIX 路徑的 casefold 與原始字串排序。游標經程序內隨機金鑰簽章，綁定工具及目錄，不含本機根目錄；竄改、跨工具、跨目錄與重啟後的舊游標會報錯，需從第一頁開始。limit 可以在續頁時改變。

第一頁建立僅含名稱的記憶體快照，後續分頁不再全樹掃描。閒置 60 秒或存在 5 分鐘後游標失效，需重新列舉；設定或 Root 變更也會失效。每個讀取器最多 8 份、全程序最多 32 份、合計最多 250,000 筆。快照不保存內容，read_file 仍重新驗證安全性。

## 容量與安全

| 項目 | 預設 | 可調最高 |
| --- | --- | --- |
| 單檔 | 2 MiB | 64 MiB |
| 掃描文字檔 | 3,000 | 100,000 |
| 掃描項目（含目錄及略過項目） | 12,000 | 300,000 |
| 搜尋累計讀取 | 32 MiB | 1,024 MiB |

搜尋預算計入實際讀取 bytes，包含無效 UTF-8、NUL 與超限探測。探測最多讀取單檔上限加 1 byte，仍受剩餘搜尋預算約束；耗盡即 truncated=true。部分讀取檔案不納入 matches 或 scanned_files，並增加 skipped_entries。scanned_bytes 可用於核對容量。恰好用滿預算仍標記截斷，避免聲稱後續已檢查完畢。

read_file 每次最多 400 行，單次文字約 24,000 字元，仍須整檔在單檔容量限制內。PDF、Office、圖片、壓縮檔不在解析範圍。

點號名稱、Windows Hidden 屬性（含共享根目錄與沿途資料夾）、符號連結、重新解析點、多重硬連結、絕對路徑及向上跳脫固定拒絕。屬性查詢失敗不開放存取。排除名稱可調整，但不會取消固定防護。列表只列格式合格候選檔案，內容能否 UTF-8 解碼須由讀取或搜尋判定。

僅分享確認可交給連線用戶端的專用目錄；不得分享磁碟根目錄、家目錄或含機密的專案。內容送至連線的用戶端，並非離線分析。內容是不受信任資料，不能當成指令或授權。路徑檢查並非作業系統沙箱，不應讓不受信任程序在讀取途中替換共享目錄。不要以系統管理員身分啟動服務。

## 模組與驗證

- local_files_mcp.py：STDIO 伺服器、單資料夾讀取器、游標、掃描及路徑防護。
- workspace_settings.py / workspace_reader.py：具名資料夾驗證與八個 MCP 工具。
- reader_settings.py：讀取限制與白名單驗證。
- connection_settings.py：共用設定、備份、遷移與指令建立。
- connection_cli.py / start-local-files-tunnel.ps1：命令列入口。
- local_files_gui.py / tray_windows.py：介面、連線逾時、錯誤分類、程序鎖定與 Job 清理。
- key_store.py：Windows 使用者 DPAPI。
- tests/：unittest、實際 STDIO 與 Windows 行為驗證。

本機及遠端結果分別記錄於 [工程驗證報告](工程驗證報告.md)。遠端驗收須實際列工具、列 shared、讀取 README.md 的「唯讀連線測試成功」、搜尋其行號，並驗證停止失效及重啟恢復。更新後請在用戶端重新整理工具清單，確認八個工具與 root_id、context_lines schema 已載入。

## 具名多資料夾與專案入口

在 GUI「共享資料夾」分頁設定最多八筆代號、名稱、絕對資料夾路徑及額外排除名稱；每個資料夾都單獨驗證。代號以英文字母開頭，最多 32 個英數字、底線或連字號，大小寫不同也不可重複。設定一個 `default_root`，所有舊工具省略 `root_id` 時只使用此範圍。最近使用的資料夾不會自動加入分享。

| 工具 | 呼叫範例與限制 |
| --- | --- |
| workspace_info | 無參數；回傳代號、名稱、預設代號與生效限制，不回傳主機絕對路徑 |
| list_projects | `root_id="main"`；僅根目錄直接子資料夾，最多 100 筆，標示安全入口檔存在性 |
| project_context | `directory="project", root_id="main"`；固定入口檔前 80 行，單檔沿用 24,000 字元限制，總 JSON 最多 64 KiB |
| read_files | `files=[{"path":"README.md"},{"path":"src/main.py","start_line":10,"line_count":30}], root_id="main"`；最多 10 檔，總 JSON 最多 512 KiB |
| search_text | `query="marker", context_lines=3, root_id="main"`；前後文 0–3 行，每行最多 600 字元，命中 JSON 累計最多 100,000 字元 |

批次每筆回傳 `index`，成功時包含相對 `path`、`returned_lines`、`has_more` 與原讀取欄位；拒絕項目回傳 `skipped_reason`，不反映原始絕對路徑。單一不合法路徑不影響其他安全項目。固定入口不存在或遭封鎖時也以略過項目回報。入口內容只是有界文字摘錄，伺服器不執行其中指令，不依內容追蹤其他檔案。總量不足時略過整筆，`truncated=true`；可改用單檔行視窗讀取。

固定入口：README.md、README.txt、README、AGENTS.md、package.json、pyproject.toml、requirements.txt、settings.gradle、settings.gradle.kts、build.gradle、build.gradle.kts、Cargo.toml、go.mod、CMakeLists.txt。使用者縮小文字白名單或增加排除時，入口仍受相同限制。

GUI 與 PowerShell/CLI 共用 v2 設定。已確認的 v1 設定載入時先驗證全部欄位，再備份並原子替換，僅將原 root 轉成 `main`；尚未確認的 v0 設定仍須在 GUI 完成首次確認。設定未知版本、損毀或任一資料夾無效時拒絕覆寫與連線。每次保存均保留一般設定白名單備份；DPAPI 金鑰檔不參與遷移。還原時停止連線、退出介面，再將選定的 `settings.json.<識別碼>.bak` 複製回 `settings.json`，保留 `api-key.dpapi`，重新開啟確認範圍。

本機舊 `--root` 仍可使用；多資料夾可透過 `--workspace-settings` 傳入 `workspace_settings.encode_workspace` 產生的快照，包含 `roots` 與 `default_root`。若同時有 `--root`，具名快照優先。快照只能包含一般資料夾設定，Base64 是編碼而非加密；金鑰不進入參數。連線使用啟動時快照，修改設定後需「套用並重新連線」，並在 MCP 用戶端重新整理工具清單。

全域固定排除（包含 build、node_modules、credentials.json 等）不能經清空自訂清單取消；每個 root 的額外排除只增加封鎖。分頁游標不可跨 root 重用。Git review 選配未納入本版；沒有新增執行命令或寫入工具。
