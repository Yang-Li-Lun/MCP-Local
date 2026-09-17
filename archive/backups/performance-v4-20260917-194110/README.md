# MCP-Local 本機檔案唯讀工具

MCP-Local 是 Windows 上的本機檔案唯讀 MCP 服務，限定在使用者明確設定的專用資料夾內，提供安全、可控且可追溯的文字與程式碼讀取能力。

服務提供以下唯讀工具：

- `list_directory`
- `list_files`
- `read_file`
- `search_text`
- `workspace_info`
- `list_projects`
- `project_context`
- `read_files`
- `search_texts`

本服務不提供寫入、刪除或指令執行能力。

## 快速開始

環境需求：Windows、Python 3.10+。

```powershell
py -3.10 -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements-lock.txt
.\.venv\Scripts\python.exe -B verify_isolated.py
```

啟動本機 STDIO 服務：

```powershell
.\.venv\Scripts\python.exe local_files_mcp.py --root .\shared
```

預設共享資料夾為程式旁的 `shared`。遠端連線需先透過 GUI 完成共享資料夾、通道與金鑰設定，再使用 `start-local-files-tunnel.ps1` 啟動。

## 資料夾與電源模式

在連線設定首頁按「加入資料夾…」，最多選取八個互相獨立的唯讀資料夾。選取後可「變更選取…」或「移除選取」；資料夾識別碼由程式管理。保存後於下次連線套用，現有連線請使用「儲存並重連」。

GUI 與系統匣提供關閉、極致節能雙模式；切換立即生效，不重連，按「儲存設定」保留下次啟動偏好。極致節能減少背景喚醒，使用可還原電源方案並限制 CPU 最大效能，工具可能變慢。預設關閉。程式退出或異常結束時，獨立還原程序負責復原臨時方案。

服務版本 `2026.09.17.2`，設定版本 5（舊版遷移預設關閉節能），MCP 工具契約維持 3。功能實作與待驗證限制見 [低功耗模式工程設計](低功耗模式工程設計.md)。

## 安全特性

- 僅讀取明確設定的共享根目錄。
- 拒絕絕對路徑跳脫、向上跳脫、符號連結、重新解析點、Hidden 項目及多重硬連結。
- 讀取與搜尋均驗證 UTF-8、NUL、檔案容量及內容完整性。
- 支援分頁、游標、具名多資料夾與批次搜尋，並套用掃描及輸出上限。
- 不把本機絕對路徑或金鑰放入 MCP 回應或命令列參數。

完整的繁體中文使用說明、限制、設定遷移與驗證流程請參閱 [README_繁體中文.md](README_繁體中文.md) 及 [介面使用說明.md](介面使用說明.md)。

## 授權

本專案採用 [Apache License 2.0](LICENSE)。

Windows 11 支援時，極致節能會設定 AC/DC「最佳電源效率」；API 不可用或失敗時顯示警告，其餘策略繼續。此功能不強制開啟 Windows 節能器。退出時僅在目前值仍為本程式套用值時還原；使用者中途改選其他模式會被保留。設定 v4 的 low 遷移為 off，extreme 保留；v5 拒絕 low。manage_windows_power_mode 預設 true，可在一般設定停用。舊 journal v1 僅用於復原，不能進入 low 執行狀態。
