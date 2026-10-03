# Windows 部署與封版

服務 `2026.10.03.1`、契約 8、設定版本 5、21 個唯讀工具、MCP 1.30.0。
核心工具行為及安全上限未因封版變更。

## 新電腦 clean clone

使用一般互動式 Windows 帳戶。安裝完整 CPython 3.13 x64，包含 pip、venv、Tcl/Tk；
本機驗證版本為 3.13.15。Python 3.10 相容分支仍保留，但不是本次完整功能部署環境。
若沒有 `py` launcher，將第一行的 `py -3.13` 換成 `& '實際 Python313\python.exe'`。
不要複製其他電腦的 `.venv`；它含有主機專用路徑。

在 clone 的專案根目錄執行，每一步確認成功再繼續：

```powershell
py -3.13 -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements-lock.txt
.\.venv\Scripts\python.exe -m pip check
.\.venv\Scripts\python.exe -B -c "import sys,tomllib,sqlite3,tkinter; assert sys.version_info[:2] == (3,13); assert tomllib.loads('ok=true')['ok']; assert hasattr(sqlite3.Connection,'deserialize') and hasattr(sqlite3.Connection,'setlimit'); w=tkinter.Tk(); print(sys.version,sqlite3.sqlite_version,w.tk.call('info','patchlevel')); w.destroy()"
.\.venv\Scripts\python.exe -m py_compile local_files_mcp.py
.\.venv\Scripts\python.exe -B -m unittest discover -s tests -v
.\.venv\Scripts\python.exe -B verify_isolated.py
.\.venv\Scripts\python.exe -B verify_release_stdio.py
```

`requirements.txt` 列直接依賴；正式部署使用固定完整版本的 `requirements-lock.txt`。
`pip check` 之外，封版紀錄核對全部 35 個固定套件版本；pip 是環境建立工具，版本不列入服務 lock。
TOML、SQLite 除了 API 檢查，最後一行會以真正 STDIO 呼叫解析 fixture 驗證。
本機可建立實體 symlink 時應完成相關測試；若權限不足而 skip，保留原因，不能稱為全數執行。
GUI/tray 測試需互動式桌面；不要為繞過失敗而以管理員執行服務。

STDIO 本機使用 `local_files_mcp.py --root .\shared`，不載入 GUI 的正式共享根。
本次驗收只產生非敏感暫存 fixture，不需要正式金鑰或通道。

## 本機部署封包

```powershell
.\.venv\Scripts\python.exe -B build_release.py
# 如需附上既有、已驗證的官方 Windows 通道成品：
.\.venv\Scripts\python.exe -B build_release.py --vendor-archive .\tunnel-client-v0.0.14-windows-amd64.zip
```

輸出位於 `release/`。同名 ZIP 已存在時會停止，避免覆寫已驗收成品。
`windows-source` 是精簡部署來源包；`windows-amd64` 另附官方通道成品。
ZIP 內的 `SHA256SUMS.json` 列出每個 payload 檔案的 SHA-256；ZIP 自身雜湊放在外部 `.zip.sha256`，
另輸出 `.manifest.json` 及 `.verification.json`。打包程式自動解包至自身暫存目錄再核對逐檔雜湊。
可用 `python -B build_release.py --verify 路徑.zip` 重驗。

封包只包含 runtime、必要圖示、安全 shared 範例、部署文件及 STDIO 驗收腳本。
不含 `.venv`、憑證、正式設定、備份、benchmark、歷史驗收輸出、測試套件或 Git metadata。
完整 unit／隔離 regression 在 Git 原始碼 checkout 執行；精簡封包解壓後依上節建立 `.venv`，
完成安裝、能力檢查及 `verify_release_stdio.py` 即可驗證部署。
封包的 VBS 直接使用 Python GUI fallback，不依賴選用的 `MCP-Local.exe` launcher。

供應商檔案不加入 Git。選用 vendor 模式只接受既有官方 ZIP，核對固定
`vendor-integrity.json` 的 tunnel-client v0.0.14 SHA-256、SPDX 內的全部六筆檔案雜湊，
並保留官方 LICENSE、NOTICE、licenses、SPDX、cloudflared 及其 manifest；不修改或重新編譯供應商檔案。
SPDX 雜湊是完整性核對，不能代替來源信任；請使用既有受信任官方交付管道取得同版成品，
不要以任意下載檔重寫 integrity。未提供 vendor 時，本機 STDIO 仍完整可用；遠端部署需另備官方成品。

金鑰與 DPAPI 不可從本機封包移植到另一台電腦。NCUE 使用者需自行建立授權共享根、
輸入自己的通道及金鑰，再另行授權啟動遠端連線。

## 驗收範圍

本次正式程序持續使用原 `.venv`；依使用者同意，clean install 與完整回歸在
`.release-work/clean-source/.venv` 執行。不更動正式共享根、設定、DPAPI、排程、通道或系統電源。
最終結果見 Git 原始碼內 `reports/engineering/release-final-20261003.md`；舊驗收報告保留為歷史證據。
本機成功不代表 GitHub push/tag/Release 或 NCUE clean-clone／遠端用戶端驗收完成。
