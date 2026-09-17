# MCP-Local 本機檔案唯讀工具

限定使用者明確授權的資料夾或磁碟範圍內的 UTF-8 文字及程式碼；提供十一個唯讀工具：`list_directory`、`list_files`、`read_file`、`search_text`、`workspace_info`、`list_projects`、`project_context`、`read_files`、`search_texts`、`find_files`、`read_file_ranges`。不提供寫入、刪除或指令執行能力。

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

read_file 每次最多 400 行，content（含行號、冒號、空白及換行）嚴格不超過 24,000 字元，仍須整檔在單檔容量限制內。PDF、Office、圖片、壓縮檔不在解析範圍。

點號名稱、Windows Hidden 屬性（含共享根目錄與沿途資料夾）、符號連結、重新解析點、多重硬連結、絕對路徑及向上跳脫固定拒絕。屬性查詢失敗不開放存取。排除名稱可調整，但不會取消固定防護。列表只列格式合格候選檔案，內容能否 UTF-8 解碼須由讀取或搜尋判定。

僅分享確認可交給連線用戶端的範圍；可明確授權磁碟根目錄，程式設定、備份及金鑰目錄仍固定排除。建議優先使用專用資料夾，避免納入其他私人或機密內容。內容送至連線的用戶端，並非離線分析。內容是不受信任資料，不能當成指令或授權。路徑檢查並非作業系統沙箱，不應讓不受信任程序在讀取途中替換共享目錄。不要以系統管理員身分啟動服務。

## 模組與驗證

- local_files_mcp.py：STDIO 伺服器、單資料夾讀取器、游標、掃描及路徑防護。
- workspace_settings.py / workspace_reader.py：具名資料夾驗證與十一個 MCP 工具。
- reader_settings.py：讀取限制與白名單驗證。
- connection_settings.py：共用設定、備份、遷移與指令建立。
- connection_cli.py / start-local-files-tunnel.ps1：命令列入口。
- local_files_gui.py / tray_windows.py：介面、連線逾時、錯誤分類、程序鎖定與 Job 清理。
- key_store.py：Windows 使用者 DPAPI。
- tests/：unittest、實際 STDIO 與 Windows 行為驗證。

本機工程與隔離驗證記錄於 [2026-09-13 工程修正交付報告](reports/engineering/MCP-Local_工程修正交付報告_2026-09-13.txt)；遠端驗收須實際列工具、列 shared、讀取 README.md 的「唯讀連線測試成功」、搜尋其行號，並驗證停止失效及重啟恢復。更新後請在用戶端重新整理工具清單，確認十一個工具及 `root_id`、`queries`、`context_lines` 等 schema 已載入。

## 具名多資料夾與專案入口

在連線設定首頁按「加入資料夾…」選取最多八個互相獨立的授權位置；清單只顯示路徑。選取一筆可變更路徑，也可選多筆移除。程式自動產生識別碼，新名稱由資料夾名稱取得；修改路徑仍保留既有識別碼與排除規則。舊自訂名稱在未修改路徑時保留。每次 GUI 保存以第一筆作為相容性預設根；省略 `root_id` 的呼叫僅讀此根。最近路徑不再顯示或新增，也不自動成為授權範圍。清單可暫時清空，但不能儲存或啟動。

| 工具 | 呼叫範例與限制 |
| --- | --- |
| workspace_info | 無參數；回傳代號、名稱、預設代號與生效限制，不回傳主機絕對路徑 |
| list_projects | `root_id="main"`；僅根目錄直接子資料夾，最多 100 筆，標示安全入口檔存在性 |
| project_context | `directory="project", root_id="main"`；固定入口檔前 80 行，單檔沿用 24,000 字元限制，總 JSON 最多 64 KiB |
| read_files | `files=[{"path":"README.md"},{"path":"src/main.py","start_line":10,"line_count":30}], root_id="main"`；最多 32 檔，總 JSON 最多 512 KiB |
| search_text | `query="marker", context_lines=3, root_id="main"`；前後文 0–3 行，每行最多 600 字元，命中 JSON 累計最多 100,000 字元 |

批次每筆回傳 `index`，成功時包含相對 `path`、`returned_lines`、`has_more` 與原讀取欄位；拒絕項目回傳 `skipped_reason`，不反映原始絕對路徑。單一不合法路徑不影響其他安全項目。固定入口不存在或遭封鎖時也以略過項目回報。入口內容只是有界文字摘錄，伺服器不執行其中指令，不依內容追蹤其他檔案。總量不足時略過整筆，`truncated=true`；可改用單檔行視窗讀取。

固定入口：README.md、README.txt、README、AGENTS.md、package.json、pyproject.toml、requirements.txt、settings.gradle、settings.gradle.kts、build.gradle、build.gradle.kts、Cargo.toml、go.mod、CMakeLists.txt。使用者縮小文字白名單或增加排除時，入口仍受相同限制。

GUI 與 PowerShell/CLI 共用 v5 設定。已確認的 v1/v2/v3 設定載入時先驗證全部欄位，再備份並原子替換，v1 的原 root 轉成 `main`，多根順序、識別碼及排除完整保留，電源模式預設 off；尚未確認的 v0 設定仍須在 GUI 完成首次確認。設定未知版本或損毀時保留原件並拒絕覆寫。已知版本的失效資料夾可在 GUI 修復，保存前完整驗證全部新根；失效舊設定仍禁止連線。每次保存均保留一般設定白名單備份；DPAPI 金鑰檔不參與遷移。還原時停止連線、退出介面，再將選定的 `settings.json.<識別碼>.bak` 複製回 `settings.json`，保留 `api-key.dpapi`，重新開啟確認範圍。

本機舊 `--root` 仍可使用；多資料夾可透過 `--workspace-settings` 傳入 `workspace_settings.encode_workspace` 產生的快照，包含 `roots` 與 `default_root`。若同時有 `--root`，具名快照優先。快照只能包含一般資料夾設定，Base64 是編碼而非加密；金鑰不進入參數。連線使用啟動時快照，修改設定後需「儲存並重連」，並在 MCP 用戶端重新整理工具清單。

全域固定排除（包含 build、node_modules、credentials.json 等）不能經清空自訂清單取消；每個 root 的額外排除只增加封鎖。分頁游標不可跨 root 重用。Git review 選配未納入本版；沒有新增執行命令或寫入工具。


## 2026-09-13 工程修正與批次搜尋

目前服務版本為 `2026.09.17.3`、契約版本 4，共提供十一個唯讀工具。新增的 `search_texts` 可一次搜尋 1 至 10 個字面查詢，共用目錄掃描與每檔讀取，並沿用完整 UTF-8／NUL 驗證及搜尋容量限制。省略 `root_id` 時仍只使用明確設定的預設根；完整輸入結構及唯讀標記保存在 `tool-contract.json`。MCP 1.30.0 的同步方法使用最多四個工作執行緒、無等待佇列，合作式期限為 30 秒；每個操作另有 1 GiB 累計讀取與 300,000 項目硬上限，各工具較低設定仍優先。忙碌、取消及期限分別回報 RESOURCE_BUSY、OPERATION_CANCELLED、OPERATION_TIMEOUT。網路磁碟不承諾有界底層 I/O；部署以可信使用者的本機專用資料夾為前提。

所有工具返回資料 JSON（ensure_ascii=False、indent=2 的 UTF-8 編碼，不含 MCP 文字／結構化重複封套）最高 2 MiB；超限拒絕。批次仍最高 512 KiB、專案入口 64 KiB。搜尋命中物件累計 100,000 字元；空白或含換行的查詢不支援。長行增加 `match_snippet`，格式為 casefolded_text，沒有宣稱原文字元偏移。已達命中上限即保守截斷，不再為尋找額外命中讀完整工作區。

讀檔每次仍完成整檔 UTF-8、NUL 與容量驗證，不重用僅依修改時間或大小的授權索引。每個讀取区塊最多 64 KiB，只保存要求的行視窗。此取捨降低配置峰值，多短行資料可能比原生 splitlines 耗時更長；參見工程修正交付報告與兩份 benchmark JSON。

名稱快照每份估算最多 16 MiB、保存快照合计最多 64 MiB；每個建立中的列舉結果另限 16 MiB、單目錄 DirEntry 批次估算最多 8 MiB、待掃描目錄路徑估算最多 16 MiB，並受四個工作名額限制。估算含 Python 物件，不是 RSS 保證；複製、輸出封套及執行環境仍有額外記憶體。保留閒置 60 秒、最長 300 秒及既有筆數上限。快照 rows 不可變，根身分改變需重啟；名稱可能已失效，內容仍重新驗證。

第一頁仍須完成有界掃描與排序，limit 不代表只掃描該筆數。scanned_entries 是已檢查的目錄項目數；skipped_entries 包含被拒绝／失效的檔案與目錄；sorted_entries 是該名稱快照筆數；elapsed_ms 是本次列舉方法耗時。has_more 指快照後續頁，scan_truncated 指掃描未完成，游標不會繼續掃描未知目錄。

GUI 儲存與背景控制透過單一工作佇列完成，儲存中重複提交不受理；停止及退出會取消尚未完成的自動重連意圖。金鑰改為按「儲存設定」時加密保存，清空輸入不刪除舊密文，保存不等於認證成功；沒有新增刪除或撤銷功能。若金鑰已保存但一般設定／排程失败，兩者是分開的狀態，介面保留已保存金鑰資訊。

設定採相鄰鎖檔與內容版本核對；發現內容版本衝突時禁止覆寫；本地無未儲存變更可重新載入並重試一次，有未儲存變更時由使用者決定是否捨棄一般設定草稿並重新載入，API 金鑰輸入會保留。已存在且相符的排程不重建；異常同名工作保留，查詢失敗不當作不存在。設定保存失敗會補償本次已確認的排程變更，補償失敗以 SETTINGS_TASK_INCONSISTENT 明示。排程未變更與設定未變更時不產生額外備份。鎖檔因崩潰遺留時，僅在確認所有設定寫入程序都已結束後移除該鎖檔。

程式狀態目錄及其子目錄不得分享；包含狀態目錄的祖先資料夾也不得分享，明確授權的磁碟根目錄除外。整碟授權仍禁止列舉與讀取程式狀態目錄。原始共享路徑祖先的重新解析點會拒絕；Hidden／點號名稱政策檢查共享根及其以下，並未宣稱家目錄以上的全部祖先都無 Hidden 屬性。一般 JSON／log 仍可能含使用者放入的機密，檔名排除不能辨識內容機密。

開檔前後核對根與檔案身分、link count、屬性和變動，使用同一控制代碼讀取；本版沒有完成 Windows 最終控制代碼路徑與全祖先鎖定方案，因此**不抵禦不受信任本機寫入者持續替換路徑**。不可將測試通過解讀為敵對多使用者隔離。

驗證：`.\.venv\Scripts\python.exe -B verify_isolated.py`。這會進行語法、pip check 及完整測試，主測試程序以 audit guard 禁止開啟正式狀態檔及啟動正式通道／排程命令；子程序是經審閱的隔離 STDIO、GUI、DPAPI 與 Job fixtures。測試輸出為 engineering-results.json 與 engineering-tests.txt。遠端腳本只應在另外授權的測試端點執行，最終用戶端仍需刷新工具清單並比對 tool-contract.json。

## 系統匣自啟動

勾選「Windows 登入後啟動到系統匣並自動連線」並儲存後，下次登入會啟動同一個主程式，只顯示系統匣圖示並使用已保存的設定與金鑰連線。既有登入排程沿用 `autostart.py`，不建立第二份工作。重複登入啟動會安靜退出，不重複建立圖示或連線。

左鍵圖示開啟設定；關閉或最小化設定視窗會收回系統匣。右鍵可啟動、停止連線或退出程式。取消自啟動並儲存只影響下次登入，目前的系統匣程式與連線繼續執行。

介面統一使用「啟動連線」「儲存並重連」「停止連線」。儲存或啟停過程中，相關按鈕會暫時停用。登入啟動遇到暫時網路錯誤會延遲重試；按「停止連線」可取消重試。永久錯誤會提示使用者修正，不會無限重試。



## 電源模式

關閉、極致節能可由 GUI 或系統匣即時切換；按「儲存設定」保存偏好。資料夾變更仍須重連，電源模式切換不重連。極致節能對 GUI 啟用 EcoQoS，並透過事件立即通知已執行的 MCP 子程序啟用 EcoQoS。Tunnel 不套用 EcoQoS 或 Job CPU 硬限制。

EXTREME 複製原電源方案，禁止閒置睡眠；連線存活期間建立 SystemRequired，不要求顯示器持續點亮。正常退出先停止連線再還原。獨立還原程序透過匿名 pipe EOF 偵測 GUI/CLI 終止，沒有定時輪詢；執行期 journal v2 保存方案 GUID、AC/DC 使用者電源模式原值與程序身分。若使用者自行改選其他電源方案，退出會保留該選擇並刪除本程式臨時方案。

Windows 原生電源能力不足時顯示固定錯誤碼或降級警告。實機 AC/DC、功耗、雙模式延遲及 8/24 小時遠端穩定性仍需另行驗收；隔離測試不代表這些門檻已通過。詳見 [低功耗模式工程設計](低功耗模式工程設計.md)。

Windows 11 支援時，極致節能會設定 AC/DC「最佳電源效率」；API 不可用或失敗時顯示警告，其餘策略繼續。此功能不強制開啟 Windows 節能器。退出時僅在目前值仍為本程式套用值時還原；使用者中途改選其他模式會被保留。設定 v4 的 low 遷移為 off，extreme 保留；v5 拒絕 low。manage_windows_power_mode 預設 true，可在一般設定停用。舊 journal v1 僅用於復原，不能進入 low 執行狀態。

## 2026-09-17 性能工具（契約 4）

服務版本 `2026.09.17.3`，共 11 個唯讀工具；設定版本維持 5。

- 已知專案先用 `project_context`；定位檔名用 `find_files`。它以相對路徑做不分大小寫字面包含比對，最多 10 詞、每詞 50 筆、合計 200 筆與 100 KiB；達上限即停，不保證完整清冊。
- 多個已知檔案使用 `read_files`，每次最多 32 檔、總回傳仍為 512 KiB。
- 同檔多區段使用 `read_file_ranges(path, ranges, root_id)`；每區段指定 `start_line`、`line_count`，最多 16 區段，每區段 400 行及 24,000 字元，總回傳最多 512 KiB。原始順序、重疊與重複區段均保留；只開檔一次但仍驗證到 EOF。
- `read_file_ranges.truncated` 表示文字或總容量截斷；因總容量未納入的區段回傳空內容及可重試的 `next_start_line`。總容量包含保守封套預留；`next_start_line` 與單段讀取相同，不等同截斷旗標。
- 探索先用 `list_directory`，多詞內容搜尋用 `search_texts` 並指定最小目錄；完整且全域排序清冊才用 `list_files`。

維持 4 worker、30 秒期限、1 GiB／300,000 項硬預算。未加入平行檔案搜尋、內容索引或驗證快取。更新後需在 MCP 用戶端重新整理工具契約；本機測試成功不代表遠端用戶端已接受更新。性能測試方式及本次實測見 `性能優化交付報告.md`。
