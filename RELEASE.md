# Windows 部署與封版

服務 `2026.10.05.2`、契約 12、設定版本 7，MCP 1.30.0。
唯讀 22／完整控制 34／開發控制 34／主機控制 36 個註冊工具；原唯讀及完整控制 contract bytes 保留。
四模式交付須同時通過單元／隔離回歸及四個真實 STDIO 驗收腳本；僅 discovery 或缺少工具鏈時的拒絕測試不能代替 VM 驗收。
主機模式採受信任命令模型，並非憑證沙箱。詳見 [權限模式說明](完整控制模式.md)。

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
.\.venv\Scripts\python.exe -B verify_full_control_stdio.py
.\.venv\Scripts\python.exe -B verify_access_modes_stdio.py
.\.venv\Scripts\python.exe -B verify_developer_control_stdio.py
```

`requirements.txt` 列直接依賴；正式部署使用固定完整版本的 `requirements-lock.txt`。
`verify_access_modes_stdio.py` 驗證四模式 discovery、主機跨 root 檔案與命令、保護路徑與降權後拒絕；
其 `developer_vm_acceptance=NOT_RUN_IN_THIS_SCRIPT` 必須保留。實際 VM、映射工具鏈、逃逸拒絕與斷線清理由 `verify_developer_control_stdio.py` 單獨驗證。
VM 驗收需要已啟用的 Windows Sandbox、互動式桌面、新版 `wsb` CLI，以及本機 Python／Git／Node／Codex CLI；使用預設安裝探測或明確 `--toolchain` 目錄（Python、Git、Node、Codex 的順序）。
資料、假憑證與唯讀拒絕測試均使用暫存 fixture；已安裝工具鏈只做唯讀映射。不得使用正式通道或憑證替代測試。
`pip check` 之外，封版紀錄核對全部 35 個固定套件版本；pip 是環境建立工具，版本不列入服務 lock。
TOML、SQLite 除了 API 檢查，`verify_release_stdio.py` 會以真正 STDIO 呼叫解析 fixture 驗證。
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

## 前版驗收紀錄（2026.10.03.2）

本次正式程序持續使用原 `.venv`；依使用者同意，clean install 與完整回歸在
`.release-work/query-clean/.venv` 執行。不更動正式共享根、設定、DPAPI、排程、通道或系統電源。
最終結果見 Git 原始碼內 `reports/engineering/query-files-final-20261003.json`；舊驗收報告保留為歷史證據。
本機成功不代表 GitHub push/tag/Release 或 NCUE clean-clone／遠端用戶端驗收完成。

query_files 在精簡封包中包含 file_query.py；22-tool STDIO 會建立 605 檔 fixture，核對全域 creation time top-1、完整分頁、metadata 一致性及拒絕路徑。封包部署與 clean-source 驗證各自建立新的 .venv 並從 requirements-lock.txt 安裝，無需複製開發電腦的環境或私有設定。正式遠端通道及另一台實體 Windows 的實跑結果需分開記錄。

## 原有雙模式相容性驗收

新的 `.venv` 只需原有 requirements-lock.txt，不新增 Node、Docker 或 npm 相依。
完整模式使用一般權限 Windows 內建 PowerShell 5.1 與 AppContainer API；
若企業政策禁止建立 AppContainer，檔案功能仍可使用，Terminal 回報 SANDBOX_UNAVAILABLE，不能視為通過。
verify_full_control_stdio.py 不使用正式設定或金鑰，依序驗證 22／34／22 工具。
封裝白名單包含四份契約、各模式 runtime、guest PowerShell、VM guard、四個 STDIO 驗收腳本及 third_party/DesktopCommanderMCP 的 MIT 授權。
封裝不包含 Windows Sandbox App 或工具鏈；這些是目標電腦須自行配置的本機前置條件。
部署時需保留所有白名單檔案；省略 access-mode 永遠是唯讀。
正式 Secure MCP Tunnel discovery 與另一台實體 Windows 的驗收需另列結果，不可由本機 STDIO 推論。
