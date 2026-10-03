# MCP-Local 本機檔案唯讀工具

MCP-Local 是 Windows 上的本機檔案唯讀 MCP 服務，在使用者明確授權的資料夾或磁碟範圍內，提供一般檔案探索、格式辨識、metadata、雜湊與有界原始資料，並支援文字、文件、靜態圖片、影音 metadata 與壓縮檔清冊。支援本機 STDIO，也可透過已設定的遠端通道連線。

**服務版本：`2026.10.03.1` · MCP 工具契約：8 · 設定版本：5 · 唯讀工具：21 個**

## 主要功能

- 最多八個互不包含的具名共享根目錄；可明確授權整個磁碟，程式設定、備份與金鑰目錄仍固定排除。
- 目錄探索、檔名定位、專案入口摘要、批次內容搜尋與多區段讀取。
- SHA-256、檔案／目錄差異、專案增量基準與實際 MCP 工具註冊自我診斷。
- Windows GUI、系統匣、登入自動連線及 DPAPI 金鑰保存。
- 關閉／極致節能雙模式，預設關閉，可即時切換並保存偏好。

MCP 工具不提供來源檔案寫入、刪除或指令執行能力。

## 圖示選擇

開啟 GUI 的「外觀」頁籤，可預覽並選擇「原版圖示」或「資料夾連結」。按「儲存設定」後立即套用至視窗與系統匣，不需重新連線；下次啟動沿用已保存的選擇。舊設定未指定 `icon_style` 時維持原版，設定版本仍為 5。

兩款圖示共存，原始 `mcp-local.ico` 保留。ChatGPT 端需另外選用 PNG，本機外觀設定不會同步至 ChatGPT：

| 樣式 | Windows ICO | MCP 圖標 PNG |
| --- | --- | --- |
| 原版圖示 | [mcp-local.ico](mcp-local.ico) | [mcp-local-classic.png](assets/icons/mcp-local-classic.png) |
| 資料夾連結 | [mcp-local-v2.ico](assets/icons/mcp-local-v2.ico) | [mcp-local-v2.png](assets/icons/mcp-local-v2.png) |

散布專案時請一併保留 `assets/icons/`。PNG 為 512×512；兩款 ICO 均含 16、24、32、48、64、128、256 像素尺寸。

## 快速開始

環境需求：Windows。完整 TOML／SQLite 解析需 Python 3.11+，且 `sqlite3.Connection` 必須提供 `deserialize` 與 `setlimit`；建議使用完整的 Python 3.13 安裝（含 GUI 所需 Tcl/Tk）。Python 3.10 保留相容性，但不具這兩項解析能力。相依套件版本見 [requirements-lock.txt](requirements-lock.txt)。

在專案根目錄執行：

```powershell
py -3.13 -m venv .venv
.\.venv\Scripts\python.exe -B -c "import sys,tomllib,sqlite3; assert hasattr(sqlite3.Connection,'deserialize') and hasattr(sqlite3.Connection,'setlimit'), 'SQLite runtime 缺少完整解析 API'; print(sys.version, sqlite3.sqlite_version)"
.\.venv\Scripts\python.exe -m pip install -r requirements-lock.txt
.\.venv\Scripts\python.exe -m pip check
.\.venv\Scripts\python.exe -B verify_isolated.py
.\.venv\Scripts\python.exe -B verify_release_stdio.py
```

若未安裝 `py` launcher，可用完整 Python 路徑執行 `-m venv .venv`。升級 Python 不會自動升級既有 `.venv`；重建前先停止 MCP，將舊 `.venv` 移至備份，保留新環境驗證失敗時的回復路徑。不要修改 `%LOCALAPPDATA%\MCP-Local` 的設定、金鑰或共享根。正式 GUI、CLI 與 MCP 子程序均使用專案 `.venv`，應以此環境的能力檢查及實際 `read_document` 呼叫驗收。

完整 clean-clone、Tcl/Tk 檢查與本機 ZIP／SHA-256 封裝步驟見 [Windows 部署與封版](RELEASE.md)。`requirements.txt` 是直接依賴；可重現部署使用完整 lock。精簡部署 ZIP 不含測試套件，完整 regression 請在 Git 原始碼 checkout 執行。

啟動本機 STDIO 服務：

```powershell
.\.venv\Scripts\python.exe local_files_mcp.py --root .\shared
```

此命令僅分享專案內的 `shared`，不會自動套用 GUI 設定。STDIO 供支援 MCP 的用戶端啟動與連接；手動執行時可用 `Ctrl+C` 停止。

### GUI 與遠端連線

1. 準備經驗證的官方 `tunnel-client.exe`，以及具備通道使用權限的金鑰。供應商二進位檔不包含在 Git 原始碼內。
2. 雙擊 `開啟連線設定.vbs`，選取共享資料夾，填入自己的通道識別碼與金鑰，再按「儲存設定」。
3. 按「啟動連線」，或於完成設定後執行 `.\start-local-files-tunnel.ps1`。

範例通道識別碼不能直接用於正式連線。修改共享資料夾後，使用「儲存並重連」套用；更新工具後，需在 MCP 用戶端重新整理工具清單。

可勾選登入後啟動到系統匣並自動連線。左鍵系統匣圖示開啟設定，右鍵可啟停連線、切換電源模式或退出。

## 唯讀工具

| 工具 | 用途 |
| --- | --- |
| `read_image` | 讀取 image_limits 公開的靜態格式（含 ICO/PNM 與可用 AVIF），原生 image 回傳有界 PNG |
| `file_info` | 大小、時間、屬性、實際 header／容器格式與可用能力 |
| `read_document` | PDF/Office/OpenDocument/EPUB、結構化資料、郵件與字幕的有界分頁 |
| `inspect_media` | MP3/FLAC/Ogg/WAV/MP4/AVI 的 metadata |
| `inspect_archive` | ZIP/TAR/TAR.GZ 安全分頁清冊與完整驗證後的成員 byte range |
| `read_binary` | 最多 16 KiB 的 base64 位元組區段 |
| `workspace_info` | 查看共享根代號、名稱、版本與生效限制 |
| `list_projects` | 查看共享根下的直接子資料夾與專案入口存在性 |
| `project_context` | 讀取 README 等固定入口的有界摘要 |
| `list_directory` | 分頁列出直接子目錄與安全規則允許的一般檔案 |
| `list_files` | 取得有界掃描、全域排序後的分頁檔案清冊 |
| `find_files` | 依相對路徑做不分大小寫的字面包含比對，定位檔名 |
| `read_file` | 讀取單檔指定行視窗 |
| `read_files` | 一次讀取最多 32 個檔案，總回傳上限 512 KiB |
| `read_file_ranges` | 一次讀取同檔最多 16 個區段，總回傳上限 512 KiB |
| `search_text` | 搜尋單個字面查詢，可附前後文 |
| `search_texts` | 一次搜尋最多 10 個字面查詢，共用掃描與檔案讀取 |
| `hash_files` | 對 1 至 32 個授權一般檔案的原始位元組計算 SHA-256 |
| `compare_paths` | 比較檔案或目錄，辨識新增、刪除、修改及相同項目 |
| `project_status` | 建立或比較限時記憶體基準；metadata 未變時重用雜湊 |
| `server_diagnostics` | 核對程式宣告、實際註冊與契約，回報版本、限制及格式 |

省略 `root_id` 時只使用預設根，不會搜尋全部共享根。完整輸入結構與唯讀標記見 [tool-contract.json](tool-contract.json)。

建議先用 `workspace_info` 確認範圍；已知專案用 `project_context`，探索目錄用 `list_directory`，找檔名用 `find_files`。內容搜尋盡量指定較小目錄，多個已知檔案用 `read_files`，同檔多段用 `read_file_ranges`。

完整格式、限制、呼叫範例與部署方式見 [格式讀取說明](格式讀取說明.md)。一般列舉不做內容解析；副檔名設定只控制文字 reader，與檔案是否可見分離。

## 限制與安全邊界

- 單檔預設上限 2 MiB，可調至 64 MiB；單次讀取視窗最多 400 行、24,000 字元。
- `find_files` 最多 10 個查詢，每詞最多 50 筆，合計最多 200 筆與 100 KiB；達上限即停止，不保證完整清冊。
- 列舉與搜尋均有掃描及輸出預算。`has_more` 代表快照仍有下一頁；`scan_truncated` 代表掃描未完成，需縮小範圍。
- 讀取與內容搜尋保留整檔 UTF-8、NUL 與容量驗證；多區段讀取只開檔一次，仍驗證到檔尾。一般文字讀取與內容搜尋不解析 PDF、Office、圖片或壓縮檔；圖片請使用 `read_image`，另有文件、媒體與壓縮檔專用工具，詳見格式讀取說明。
- 拒絕工具輸入中的絕對路徑、向上跳脫、點號名稱、Hidden 項目、符號連結、重新解析點與多重硬連結。
- 整碟授權仍套用排除規則；程式狀態目錄禁止列舉與讀取。檔名排除不能辨識所有內容機密，應只授權願意交給連線用戶端的範圍。
- MCP 中繼資料不揭露主機根目錄絕對路徑；金鑰不放入命令列參數。檔案內容本身可能含路徑或敏感資訊。
- 內容視為不受信任資料。路徑防護不是作業系統沙箱，不抵禦不受信任本機程序持續替換共享路徑；不要以系統管理員身分執行服務。

## 電源模式

GUI 與系統匣可即時切換「關閉」或「極致節能」，不需重連；按「儲存設定」保留下次啟動偏好。極致節能使用 EcoQoS、臨時電源方案與 CPU 效能限制，可能增加工具回應時間；連線存活期間會提出防止系統睡眠的要求。

Windows 11 支援時會套用「最佳電源效率」，不強制開啟 Windows 節能器。正常退出或異常結束由還原流程處理；使用者中途改選其他方案或模式時保留其選擇，還原失敗需查看日誌。實機功耗與長時間遠端穩定性仍需另行驗收，詳見 [低功耗模式工程設計](低功耗模式工程設計.md)。

## 驗證與文件

`verify_isolated.py` 執行隔離語法、相依套件及回歸檢查，並寫出測試報告。一般電源測試使用 mock；真實系統電源測試需另外明確授權。本機驗收與正式用戶端工具發現分別記錄。

遠端驗收須在實際 MCP 用戶端重新整理並確認 21 個工具、比對契約，以及實際列目錄、讀檔與搜尋。本機測試成功不能代替遠端驗收。

- [增量與診斷工具說明](增量與診斷工具說明.md)：呼叫範例、基準生命週期、差異語意與驗收。
- [完整繁體中文說明](README_繁體中文.md)：容量、游標、多根設定、遷移及錯誤處理。
- [介面使用說明](介面使用說明.md)：GUI、系統匣與連線操作。
- [性能優化交付報告](性能優化交付報告.md)：性能工具、測試方法與既有實測。

## 授權

本專案採用 [Apache License 2.0](LICENSE)。


## 2026-09-29 雜湊、差異與增量診斷

新增 `hash_files`、`compare_paths`、`project_status`、`server_diagnostics`。服務版本 `2026.10.03.1`、契約 8、共 21 工具，設定版本仍為 5。

雜湊涵蓋一般檔案的原始位元組，保留 Hidden、link、排除名稱、共享根及容量限制。增量基準僅存在 MCP 程序記憶體，最長 24 小時，每個 workspace 最多 8 份；重啟或淘汰後須重新建立。只快取 metadata 與雜湊，文字讀取仍完整驗證。

更新服務後，用 `server_diagnostics` 核對實際註冊，再從用戶端 `tools/list` 確認工具可見；server 無法自行證明 ChatGPT 已取得工具。完整參數、界線與示例見 [增量與診斷工具說明](增量與診斷工具說明.md)。

## 原生圖片讀取（契約 8）

新增 `read_image(path, root_id?)`，以原生 MCP `ImageContent` 傳回 PNG 圖片，既有工具皆保留，目前共 21 個唯讀工具。支援靜態 PNG、JPEG、WebP、GIF、BMP、TIFF、ICO、PNM 與可用 build 的 AVIF；僅使用既有授權根，檔案須通過路徑、Hidden、link、排除名稱、狀態目錄及檔案身分檢查。詳見 [圖片讀取說明](圖片讀取說明.md)。

## 契約 8 格式擴充

保留 21 工具，在 read_document 加入 format_hint/table/expected_sha256，在 inspect_archive 加入 member_path/offset/length/expected_sha256。支援結構化資料、OpenDocument、EPUB、郵件、外掛字幕與更多圖片／媒體 metadata；來源唯讀與原有安全上限保留。詳見 [格式讀取說明](格式讀取說明.md)。
