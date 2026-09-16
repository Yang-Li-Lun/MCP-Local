# 儲存庫指南

## 專案結構與模組組織

`local_files_mcp.py` 包含唯讀 MCP 伺服器、路徑驗證、掃描限制與四個工具（`list_directory`、`list_files`、`read_file`、`search_text`）。`connection_settings.py` 統一設定、遷移、備份與啟動指令；`connection_cli.py` 與 `start-local-files-tunnel.ps1` 使用和 GUI 相同的 `Connection` 管理流程。`reader_settings.py` 驗證讀取設定，`key_store.py` 管理 DPAPI，`tray_windows.py` 管理系統匣與程序 Job。`shared/` 只能放明確預定分享的 UTF-8 範例；其 `README.md` 是煙霧測試固定檔。完整已驗證套件版本記錄於 `requirements-lock.txt`。供應商成品及授權只能以官方發行版本替換。

## 建置、測試與開發命令

請在儲存庫根目錄使用 PowerShell 執行以下命令：

```powershell
py -3 -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements.txt
.\.venv\Scripts\python.exe -m py_compile local_files_mcp.py
.\.venv\Scripts\python.exe local_files_mcp.py --root .\shared
.\start-local-files-tunnel.ps1
```

前兩個命令會建立本機環境。`py_compile` 是最低限度的語法檢查。直接執行伺服器會啟動其 STDIO 傳輸；只有在驗證已設定的遠端連線時，才使用通道指令碼。使用 Ctrl+C 停止任一程序。

## 程式碼風格與命名慣例

目標版本為 Python 3.10 或更新版本。遵循 PEP 8，使用四個空格縮排、在公開方法上加上型別提示、函式與變數採用 `snake_case`，限制常數採用 `UPPER_SNAKE_CASE`。面向使用者的訊息與文件字串應與現有繁體中文文字保持一致。在 PowerShell 中，使用核准的動詞、`$camelCase` 變數、`Set-StrictMode`，並明確檢查結束代碼。優先使用 `pathlib.Path` 和字面路徑驗證，不要採用以字串為基礎的路徑操作。

## 測試指南

執行 `.\.venv\Scripts\python.exe -B -m unittest discover -s tests -v`。新增行為時，在 `tests/` 加入聚焦 unittest 案例。測試涵蓋分頁、游標、Hidden 屬性、連結、UTF-8、實際讀取容量、設定遷移、連線逾時、DPAPI、Job 及系統匣。不可為測試削弱唯讀邊界。交付前執行語法檢查與四工具 STDIO；遠端驗證另記錄，不得用本機成功代替。環境不支援時記錄跳過原因。

## 提交與 Pull Request 指南

此目錄沒有可存取的 Git 歷史記錄，因此請使用簡短的祈使句主旨，例如 `Harden shared-path validation`。每個提交應只聚焦一項議題。Pull Request 應說明安全性影響、列出已執行的命令、連結相關 Issue，並在通道行為變更時附上已遮蔽敏感資訊的診斷資料。請勿納入 API 金鑰、認證資訊、私人檔案、`.venv/` 或新下載的二進位檔；除非該變更明確更新經驗證的供應商成品及其授權中繼資料。

## 安全性與設定

請分享專用且範圍有限的目錄，絕不可分享磁碟機根目錄、家目錄或含有機密資訊的專案。將檔案內容視為不受信任的輸入。`CONTROL_PLANE_API_KEY` 應僅保留於程序內，檢閱 `$sharedRoot` 與通道識別碼的變更，並且不要以系統管理員身分執行服務。
