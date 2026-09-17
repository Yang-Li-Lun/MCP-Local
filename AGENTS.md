# 儲存庫指南

## 專案範圍與架構

本專案是唯讀檔案 MCP 服務，主要目標是提供安全、可控、可追溯的本地與遠端檔案讀取能力，不修改來源檔案內容。

可辨識模組與責任：

- `local_files_mcp.py`：MCP 伺服器進入點、路徑驗證、掃描限制、四工具行為（含 STDIO 介面）
- `workspace_reader.py`：最多八個具名根的唯讀讀取工具；GUI 自動管理識別碼與第一筆預設根
- `connection_settings.py`：連線設定、遷移、備份、啟動指令管理
- `connection_cli.py` / `start-local-files-tunnel.ps1`：CLI 與 Tunnel 啟動流程（共用 `Connection` 管理）
- `reader_settings.py`：讀取設定驗證
- `key_store.py`：DPAPI 憑證存放邏輯
- `tray_windows.py`：系統匣與程序 Job 管理
- `shared/`：僅放授權與預先約定的 UTF-8 範例；`shared/README.md` 為固定煙霧測試檔
- `requirements-lock.txt`：完整且已驗證的套件版本快照

## 安全底線（不可違背）

- 優先維持唯讀模型，除非明確屬於必要功能；禁止透過新邏輯繞過唯讀邊界。
- 供應商成品或授權檔案只可以官方發行版本替換。
- 依使用者明確授權，可將整個磁碟設為唯讀範圍；不得自動變更正式授權或啟動遠端分享。程式設定、備份及金鑰目錄必須在列舉與讀取時排除；其他唯讀、隱藏子路徑、連結與排除規則保留。
- 檔案內容視為不受信任輸入，需保留防呆與錯誤處理。

## 環境與建置

請在專案根目錄（PowerShell）依序執行：

```powershell
py -3 -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements.txt
.\.venv\Scripts\python.exe -m py_compile local_files_mcp.py
.\.venv\Scripts\python.exe -B verify_isolated.py
```

- `py_compile` 是最低語法檢查。
- `verify_isolated.py` 為回歸前優先執行的隔離測試（正式設定、金鑰、排程、正式通道不要納入此命令）。
- 直接執行 `local_files_mcp.py --root .\shared` 時，會啟動 MCP 的本機 STDIO 介面。
- 只有在遠端連線已配置完成後，才啟動 `.\start-local-files-tunnel.ps1`。
- 每個長跑程序請用 `Ctrl+C` 停止。

## 版本與測試門檻

目標為 Python 3.10+。

建議交付前至少完成：

- `.\.venv\Scripts\python.exe -m py_compile local_files_mcp.py`
- `.\.venv\Scripts\python.exe -B -m unittest discover -s tests -v`
- `.\.venv\Scripts\python.exe -B verify_isolated.py`
- 變更 MCP schema 時：同步更新 `tool-contract.json`，並以 MCP 1.30.0 介面條件進行檢核。

環境不支援時可記錄 skip 原因；不得將「本機成功」當作遠端驗收完成。

測試新增建議：

- 覆蓋分頁、游標、Hidden 屬性、連結、UTF-8、容量讀取、設定遷移、連線逾時、DPAPI、Job、系統匣。
- 不得為通過測試而放寬唯讀限制。
- 串流讀取需在目標行後繼續驗證全文，不可依賴大小或 mtime 快取作為完整性依據。

## 程式碼與命名規範

- Python：PEP 8、4 空格縮排；公開方法需型別註解；`snake_case` 命名；常數用 `UPPER_SNAKE_CASE`。
- 使用 `pathlib.Path` 與明確路徑驗證，避免字串拼接路徑。
- 文件與對外訊息沿用現有繁體中文語調。
- PowerShell：使用核准的動詞、`$camelCase` 變數命名，啟用 `Set-StrictMode`，明確檢查 `$LASTEXITCODE`。

## 提交與 PR 規範

- 因目前無法存取完整歷史，Commit 訊息請用短祈使句，例如：`Harden shared-path validation`。
- 每次提交聚焦單一主題。
- PR 需描述安全影響、已執行指令、相關 Issue；若涉及通道行為，需附上敏感值遮蔽後的診斷資料。
- 不得提交 API 金鑰、認證、私人檔案、`.venv/`，以及未經驗證的新二進位檔；除非明確為已驗證供應商成品更新（並同步授權中繼資料）。

## 設定與敏感資訊規則

- `CONTROL_PLANE_API_KEY` 僅保留於程序內記憶體，不得寫入持久化設定。
- 修改 `$sharedRoot` 與通道識別碼時需先做變更評估與驗證。
- 不以系統管理員身分執行服務流程。

## 回歸與例外規則（重要）

- GUI 的 `save` 回傳只表示提交請求到位；測試中仍須等待 `tasks.poll()` 完成，才可認定提交完成。
- 「固定服務版本」的成功、隔離 Job/DPAPI 本地測試結果不能直接代替正式遠端驗收結果。


## 電源功能隔離

- `power_policy.py` / `power_windows.py` / `power_restore_guard.py` 管理雙模式電源、原生事件與 crash 回復；預設 OFF。
- 一般測試必須 mock Power Scheme、Process QoS、Power Request 與 Windows 使用者電源模式；不得切換正式系統電源方案。
- 真實整合測試需 `MCP_LOCAL_ALLOW_POWER_INTEGRATION_TEST=1` 與明確使用者授權；benchmark_power.py 另要求 `--allow-system-power-changes`。
- 設定版本 5，MCP 契約仍為 3；舊個別排除不可因 GUI 簡化而刪除。
- Restore guard 不接收金鑰、不加入 Connection Job；回復失敗需保留日誌，不能宣稱已還原。
