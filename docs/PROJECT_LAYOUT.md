# 專案結構與開發入口

本文件的命令從專案根目錄執行。公開模式為 `read_only`、`developer_control`、`host_control`；第四模式研究維持 PAUSED。

| 位置 | 用途 |
| --- | --- |
| `mcp_local/` | 正式 Python 套件；唯讀讀取、格式 worker、三模式控制、設定、連線、GUI 與電源實作 |
| `contracts/` | 四份固定工具契約；包含僅供內部回歸的 AppContainer 契約 |
| `tools/verification/` | 離線回歸及 STDIO 驗證；原生 VM／AppContainer 驗收另需明確授權 |
| `tools/benchmarks/`、`cases/` | 效能工具與固定案例；真實共享根、遠端、電源量測依各工具原授權條件 |
| `tools/diagnostics/` | 遠端煙霧測試與呼叫追蹤請求工具；不會因搬移而自動執行 |
| `tools/build_release.py` | 固定封裝白名單、供應商完整性及 ZIP 解包驗證 |
| `tests/` | 單元測試及合成 fixture；mock 路徑對應正式套件 |
| `docs/guides/`、`docs/architecture/` | 現行操作與架構文件 |
| `docs/history/` | 當時的設計及交付紀錄，不代表目前實作或驗收結果 |
| `assets/icons/`、`launcher/` | 固定圖示與 Windows launcher 原始碼 |
| `third_party/tunnel-client/` | 官方 tunnel-client／cloudflared、ZIP、完整性與授權資料；僅搬移，不改寫成品 |
| `third_party/DesktopCommanderMCP/` | 既有來源授權及 NOTICE |
| `reports/engineering/`、`reports/performance/` | 工程與量測證據；`legacy-root/` 保存先前根目錄輸出 |
| `reports/validation/` | 新的隔離回歸預設輸出；每次建立獨立目錄 |
| `shared/` | 固定非敏感煙霧測試範例 |

根目錄保留 `local_files_mcp.py`、`local_files_gui.py`、`connection_cli.py`、`autostart.py`、`build_release.py` 相容入口，以及 `MCP-Local.exe`、VBS 和 PowerShell 啟動檔。既有排程、GUI launcher 與 MCP 命令仍使用根目錄入口；它們匯入 `mcp_local` 中的同一實作。另保留 `access_mode.py` 相容匯入檔，讓原地封存文件的既有連結與舊匯入仍可使用；模式定義只存在於套件中。依賴繼續使用根目錄 `.venv`。正式設定與 Credential 的儲存位置沒有改變。

## 執行模組分類

| 責任 | `mcp_local/` 模組 |
| --- | --- |
| 服務與讀取 | `local_files_mcp`、`workspace_reader`、`file_query`、`stream_read`、`stream_search`、`snapshot_cache`、`incremental_state`、`operation_budget` |
| 安全格式解析 | `image_reader`、`format_reader`、`format_worker`、`format_parsers`、`media_parser`、`structured_parser`、`document_extras` |
| 模式與檔案控制 | `access_mode`、`control_files`、`control_edit`、`full_control`、`control_sessions`、`sandbox_windows`、`host_control`、`host_files`、`host_windows` |
| 開發 VM | `developer_control`、`developer_toolchains`、`developer_vm`、`developer_vm_client`、`developer_vm_guard`、`mcp_local/developer_guest.ps1` |
| 連線與設定 | `connection_cli`、`connection_runtime`、`connection_settings`、`workspace_settings`、`reader_settings`、`security_policy`、`key_store`、`integrity`、`backup_rotation`、`safe_logging` |
| 桌面與電源 | `local_files_gui`、`gui_tasks`、`tray_windows`、`icon_assets`、`autostart`、`autostart_windows`、`power_policy`、`power_windows`、`power_restore_guard` |
| 契約與診斷 | `tool_contract`、`capability_diagnostics`、`read_call_trace` |

## 離線驗證與封裝

```powershell
.\.venv\Scripts\python.exe -B -m tools.verification.verify_isolated --offline
.\.venv\Scripts\python.exe -B -m tools.verification.verify_release_stdio --report <本次報告.json>
.\.venv\Scripts\python.exe -B -m tools.verification.verify_access_modes_stdio --report <本次三模式報告.json>
.\.venv\Scripts\python.exe -B build_release.py --output-dir <新的封裝目錄>
```

`--offline` 明列跳過兩項需要建立 AppContainer profile 的單元測試及內部 AppContainer STDIO 測試，阻擋 VM guard／AppContainer 啟動；電源變更、正式通道及排程繼續由隔離 guard 拒絕。三模式 STDIO 使用合成檔案與主機子程序，開發後端刻意不可用，不能代替真實 VM 驗收。測試暫存目錄必須可寫且位於受保護的服務目錄之外；不得因此放寬產品路徑防護。

封裝使用新目錄及排他建立 ZIP，不覆寫既有版本。完整 Git 原始碼依 `contracts/source-delivery-files.json` 提供 `tools/`、`tests/`、程式、契約、文件與授權資料。`tools/build_source_candidate.py` 產生固定清單及逐檔 SHA-256 的來源候選。精簡部署 ZIP 只含固定白名單列出的執行資源和驗證工具；`reports/`、`archive/` 與不明資料只保留於本機，工程證據連結僅適用保有這些資料的工作樹。

## 保留與追溯

`archive/backups/`、`.venv*`、`release/`、`.release-work/`、`.test-tmp/` 與用途未確認的 `NVIDIA Corporation/` 保留原位。第四模式的封存與研究目錄不搬移、不修改。`.test-tmp` 既有清理曾被政策阻擋，不因目錄整理而重試清理；其中原始還原資料的獨立完整副本仍需另外確認。

歷史比較工具 `tools/benchmarks/benchmark_actual_performance.py` 需要本機保留的固定 `archive/backups/` 快照與另行授權的共享根；乾淨來源不含該快照，工具會拒絕執行。其路徑隔離回歸使用合成 fixture，不依賴備份原件。

歷史報告與雜湊清單中的原始路徑保留當時語意；查找搬移後檔案請使用工程報告中的 `moves.json`。搬移不等於刪除，歷史驗證結果也不等於現行版本重新通過。
