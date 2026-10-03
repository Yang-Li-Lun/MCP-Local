# MCP-Local 最終本機封版驗收（2026-10-03）

**判定：FINAL（本機 release-ready）；blocker：無。**
GitHub commit/push/tag/Release 與 NCUE clean-clone／遠端驗收尚未執行。
本報告記錄本次實測；不以較早的成功或失敗紀錄代替本次結果。

## 版本與範圍

| 項目 | 已驗證結果 |
| --- | --- |
| 起始 HEAD | `a871e32b810704da420975e40da579bf010470c4` |
| Service／contract／settings | `2026.10.03.1`／8／5 |
| MCP／Python／SQLite／Tcl | 1.30.0／3.13.15 x64／3.50.4／8.6.15 |
| MCP 工具 | 21；schema、annotations、服務版本與契約一致 |
| `server_diagnostics.consistent` | `true`；實際及預期 digest 相同 |
| canonical contract SHA-256 | `37de43a7e513f0ea2ee9532e26715012b453b07e1dd4513b8ae0c73052779922` |
| TOML／SQLite | tomllib 可用；deserialize／setlimit 可用；真實解析均通過 |

`tool-contract.json`、核心 reader/parser/server、tray 與正式設定行為均未修改，版本未增加。
Python 3.10 僅完成 72 個來源的語法相容檢查，未宣稱跑過 3.10 runtime。

正式 GUI／tunnel 原已使用根目錄 `.venv`。使用者明確同意改以
`D:\codee\MCP-Local\.release-work\clean-source\.venv` 建立全新環境驗證。
另一個獨立環境 `.release-work/package-deploy/.venv` 用於解壓封包後安裝與驗收。
兩者皆從現有完整 CPython 3.13.15 建立，未複製原 `.venv` 的 site-packages。
本機沒有 `py` launcher，實際使用 `.venv-runtime/Python313/python.exe -m venv`；文件保留完整路徑替代方式。
未啟停正式通道、未讀寫正式 Credential／DPAPI／共享根設定、未變更排程或實際系統電源。
起始正式程序 PID 44028、2724、28664、39112、39224、43796 在收尾時仍存在。

## 實際修改

- 統一 README、繁中 README、AGENTS 與格式文件的 Python 3.13 capability／固定 lock 安裝指引；lock 僅修正文案，套件版本未改。
- 新增 `RELEASE.md`，區分完整 Git checkout regression、精簡部署 ZIP、vendor 供應、DPAPI 不跨機移植及後續 NCUE 驗收。
- 新增 `verify_release_stdio.py`，使用可自動刪除的非敏感 fixture，核對完整 21 工具與正反向行為。
- 新增 `build_release.py`，依固定白名單封裝、逐檔 manifest、供應商 integrity／SPDX 檢查及解包回驗。
- 三個命令組裝測試改用 `tests/command_fixture.py` 的獨立非執行成品。仍呼叫真實 `verify_client`，沒有 mock 掉 integrity；另新增成品被變更時必須拒絕的測試。
- 整碟測試將 fixture 放在一般暫存目錄，避免隱藏 checkout 祖先路徑被安全規則正確拒絕。未放寬 Hidden、link、狀態目錄或其他安全守衛。
- `.gitignore` 排除隔離工作區、release、venv 備份及本機輸出；`engineering-unittest-v5.txt`、`performance-stdio-interleaved.log` 僅取消 Git 追蹤，本機檔案保留。
- 既有 `archive/backups` 仍供 benchmark 安全回歸及歷史重現使用，保留在 Git；部署 ZIP 不包含它們。既有 reports 不覆寫。

## 驗證結果

以下指令均使用對應副本內的 `.venv/Scripts/python.exe`。

| 實際檢查／指令 | 結果 |
| --- | --- |
| `-m pip install -r requirements-lock.txt` | 兩個全新 venv 安裝成功；未升級 lock |
| `-m pip check`、固定版本核對 | PASS；35 個 lock 套件完全相等，唯一額外套件為環境工具 pip 26.2.1 |
| 已安裝 distribution RECORD | 2,355 筆有雜湊記錄全數吻合；1,811 筆無 hash 記錄未宣稱已驗 hash（含生成檔及 RECORD 自身等） |
| TOML、SQLite memory deserialize/setlimit、Tk 建窗 | 全部 PASS；不是只檢查版本字串 |
| `py_compile`／AST | 72 個目前來源與測試檔全部通過；另完成 Python 3.10 語法解析 |
| `-B -m unittest discover -s tests -v` | **316 pass／0 fail／0 error／0 skip**；171.675 秒 |
| `-B verify_isolated.py` | **316 pass／0 fail／0 error／0 skip**；171.073 秒；pip check=0 |
| `-B -m unittest discover -s tests -p '*gui*.py' -v` | **42 pass／0 fail／0 error／0 skip** |
| `-B -m unittest discover -s tests -p test_windows_gui.py -v` | 原工作區 5 pass；clean-source 5 pass；解包部署 5 pass |
| `-B -m unittest discover -s tests -k command -v` | 6 pass；最終 fixture 型別註解補齊後再次驗證 |
| 解包環境的 `test_icon_assets.py` | 3 pass；測試 harness 取自來源 checkout，載入封包內 runtime/assets |
| `-B verify_release_stdio.py` | clean-source 與解包部署各 **21 工具、33 calls、PASS** |
| 固定供應商 ZIP | pinned tunnel-client SHA-256、六筆 SPDX SHA-256 及既有 LICENSE／NOTICE／cloudflared manifest 一致 |
| ZIP 解包與 manifest | 66 個 payload 檔案全數吻合；source 與 ZIP 一致；本地 Python import closure 無缺檔 |
| 封包竄改負向 | 修改 requirements.txt 後驗證明確拒絕 |
| Git、文件與來源 | 核心及契約未變；168 個來源／assets／歷史必要檔與隔離副本逐 byte 一致；`git diff --check` 通過 |

一般測試未啟用真實 Windows 電源整合，隔離 audit guard 保持作用。所有實體 symlink 案例在本 session 實際執行，沒有沿用歷史 5 skip。

### 初次失敗及修正依據

初次隔離副本完整與 isolated 回歸均為 315 tests、310 pass、5 errors、0 failures、0 skips：
三項命令測試因副本沒有 Git 排除的官方 EXE 失敗；一項整碟 fixture 因 `.release-work` 隱藏祖先被拒絕；
一項 benchmark 回歸因建立副本時漏帶已追蹤的歷史 snapshot 失敗。
已分別以獨立 integrity fixture、非隱藏暫存 fixture 及補齊 tracked snapshot 解決，沒有取消任何測試或降低安全限制。
最後兩套完整回歸皆通過。新增 STDIO 驗收腳本初次將根目錄誤當 `list_projects` 的子專案；
修正 fixture 為真正子專案後，33 次呼叫通過。這是驗收 harness 修正，不是 MCP 行為變更。

### Tray WinError 5

在目前 Session 1、`WinSta0\Default`、非提權帳戶，未修改 tray 前先執行原 5 項測試，全部通過。
真實右鍵 native menu、左鍵顯示、隱藏／還原、關閉及 Job 清理在 clean-source 與解包環境再次通過。
舊報告的兩個 `WinError 5` 目前未重現；當時究竟哪一層權限／環境拒絕仍未確證，不能從拒絕碼推定根因。
現有證據不支持更改 tray 程式，本次正式本機 tray gate 已有實測通過。

## 21 工具 STDIO

以下每個工具均為真實 `tools/call` 成功；`tools/list` 全量 schema／annotations 對照成功：

`list_directory`、`list_files`、`read_file`、`search_text`、`workspace_info`、`list_projects`、
`project_context`、`read_files`、`search_texts`、`read_file_ranges`、`find_files`、`hash_files`、
`compare_paths`、`project_status`、`server_diagnostics`、`read_image`、`file_info`、`read_document`、
`inspect_media`、`inspect_archive`、`read_binary`。

25 次正向呼叫核對 UTF-8 值、原生 PNG ImageContent／解碼尺寸與像素、TOML、
獨立完整 SQLite 記憶體快照／資料列續讀／expected_sha256、WAV metadata、ZIP 成員 range、
binary range、SHA-256、compare 及 project baseline。驗收前後 fixture 檔案清單、內容 hash 與 mtime 不變。

8 次預期拒絕：WAL header SQLite、archive traversal、壓縮炸彈、SQLite SQL 注入式 table 名、
錯誤 expected_sha256、binary 超過上限、錯誤參數型別及越界路徑。拒絕不是測試失敗。
完整 regression 另涵蓋 archive CRC／尾端、worker memory／外部存取、Hidden/link、來源完整性等案例。

## 本機成品

- 路徑：`D:\codee\MCP-Local\release\MCP-Local-2026.10.03.1-windows-amd64.zip`
- 大小：28,549,027 bytes；66 個 payload 檔案及內含 `SHA256SUMS.json`。
- SHA-256：`d90648e2f184edc90677d3c307b5ea5cec735897ab79669da20f8acad117e1ea`
- 同目錄附 `.zip.sha256`、`.manifest.json`、`.verification.json`。
- 供應商成品與授權檔以官方 ZIP 原 bytes 納入；未執行通道程式。沒有 `.venv`、私有設定、金鑰、測試暫存或歷史備份。
- 封包是部署來源及已驗證 Windows vendor 成品；Python 與固定 Python dependencies 由新電腦依文件建立，並非免安裝 EXE。

## Git 與後續

未 commit／push／tag／建立 GitHub Release。工作樹只包含上述封版文件、驗收／封裝腳本、測試 fixture 修正及兩項本機輸出取消追蹤。
新增報告與來源 manifest 應連同上述修改一起提交；`release/`、`.release-work/`、venv、vendor EXE／ZIP／授權附檔皆維持 ignored。
沒有 tracked credential、DPAPI、正式設定或 venv；本次變更文字未發現常見 credential pattern，此靜態掃描不等於所有歷史內容的完整秘密稽核。

下一階段：

1. 使用者另行授權後，在 GitHub 提交／push 此來源版本，並核對遠端 commit；tag／Release 依發布決策另行處理。
2. NCUE 在另一台 Windows clean clone，使用完整 Python 3.13，依 `RELEASE.md` 安裝固定 lock、檢查 TOML／SQLite／Tk、完整 regression 及 21 工具 STDIO。
3. NCUE 自行配置授權根與自己的憑證；獲授權後才啟動遠端通道，刷新用戶端 tools/list，核對契約 digest、21 工具實際呼叫及原生圖片。不能以本機成功代替此驗收。

機器可讀證據：[release-final-20261003.json](release-final-20261003.json)。
來源指紋：[release-final-source-manifest-20261003.json](release-final-source-manifest-20261003.json)。
完整本機 stdout／stderr 與初次失敗記錄保留於 `.release-work/evidence/`，各檔 SHA-256 列入機器可讀報告；不把這些 local test output 加入 Git。
