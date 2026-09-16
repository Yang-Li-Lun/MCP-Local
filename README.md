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

## 安全特性

- 僅讀取明確設定的共享根目錄。
- 拒絕絕對路徑跳脫、向上跳脫、符號連結、重新解析點、Hidden 項目及多重硬連結。
- 讀取與搜尋均驗證 UTF-8、NUL、檔案容量及內容完整性。
- 支援分頁、游標、具名多資料夾與批次搜尋，並套用掃描及輸出上限。
- 不把本機絕對路徑或金鑰放入 MCP 回應或命令列參數。

完整的繁體中文使用說明、限制、設定遷移與驗證流程請參閱 [README_繁體中文.md](README_繁體中文.md) 及 [介面使用說明.md](介面使用說明.md)。

## 授權

本專案採用 [Apache License 2.0](LICENSE)。
