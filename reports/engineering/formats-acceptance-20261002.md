# 一般檔案探索與格式讀取驗收

## 本次結果

完成本機 repository 實作，服務 `2026.10.02.1`、契約 7、21 工具，設定版本維持 5。原本 16 個工具保留，新增 `file_info`、`read_document`、`inspect_media`、`inspect_archive`、`read_binary`。工具 schema／readonly annotations／tool-contract.json／實際 registry／server diagnostics 相符，診斷 `consistent=true`。完整機器可讀結果及 source hashes 見 [formats-acceptance-20261002.json](formats-acceptance-20261002.json)。

開始前工作目錄只有兩個未追蹤的圖示產物；兩者保留。未提交或推送，未修改正式 root、連線、金鑰、排程或電源設定，未重啟正式通道。初始隔離基準為 257 tests、0 failures、0 errors、4 skips。

## 架構與實際能力

- `FileReader.walk` 只以一般檔案、安全規則與掃描預算決定可見性。list_directory/list_files/find_files 不解析檔案內容；文字搜尋另選文字候選。
- `open_checked(binary=True)` 保留既有 root、relative path、Hidden、reparse point、symlink、hard-link、敏感名稱／狀態目錄、handle identity 與讀取前後變更查驗。只有 metadata 模式允許超大檔案有限 header 存取；內容工具仍遵守 root 容量。
- `format_reader.py` 讀取受限快照、基本 metadata、signature、SHA-256 工具導引及 raw range；相對路徑不會送入 parser。
- `format_worker.py` 固定 Python／腳本，無 shell／使用者命令，清除連線環境，10 秒 wall／CPU、512 MiB 記憶體限制，逾時或取消終止。先載入必要 parser／codec，之後 audit hook 封鎖檔案、socket、子程序等外部存取。這不是未知 native exploit 的完整 OS sandbox。
- `format_parsers.py` 提供 PDF、DOCX、XLSX、PPTX 文字片段與 ZIP/TAR/TAR.GZ 清冊；實際 header/container 驗證，不因副檔名直接信任格式。長段落可依 next_start 續讀。
- `media_parser.py` 提供 MP3、FLAC、Ogg Vorbis／Opus、WAV、MP4、AVI metadata。沒有 ffprobe／ffmpeg／Office 安裝或任意執行能力。
- `image_reader.py` 擴充至靜態 GIF/BMP/TIFF，原有 PNG/JPEG/WebP 介面保留；所有圖片解碼都移至受限 worker，保持像素限制、metadata 清除與原生 ImageContent。
- hash_files／compare_paths／project_status 改為一般檔案的原始位元組；metadata 快取只用於雜湊基準，文字 reader 仍驗證到 EOF。強二進位 header 改名 `.txt` 仍拒絕當文字。

## 限制與不支援範圍

全部限制、參數例子及支持矩陣見 [格式讀取說明](../../格式讀取說明.md)，同時可用 diagnostics 查詢數值。重點為來源 32 MiB 與 root 單檔／累計限制取最小、metadata header 4 KiB、raw 16 KiB、解析 10 秒／512 MiB、每次 100 片段／每片段 2,000 字元、文件輸出預算 128 KiB／parser JSON 256 KiB。

PDF 最多 2,000 頁／8 MiB stream；Office XML 每成員 8 MiB／200,000 節點／64 層。封存最多 4,096 entries／每成員 8 MiB／合計 128 MiB／200:1，ZIP directory 4 MiB、TAR.GZ 展開 32 MiB。媒體最多 32 tracks／10,000 boxes 或 chunks，tags 與 timing table 另有固定上限。圖片維持 16 MiB／25M pixels／來源邊長 16,384、輸出邊長 2,048／PNG 3 MiB。

不支援 OCR、加密文件、舊 Office、macro-enabled Office 主格式、完整版面還原、公式計算、外部關聯、embedded script／附件執行、動畫／多幀、RAR/7Z/ZIP64 完整解析、archive member read、遞迴封存或磁碟解壓。這些檔案仍能被安全探索，並依容量取得 metadata/hash/raw。超大檔案至少有基本 metadata，hash／raw 不放寬既有 root 限制。

## 驗證

本機：Windows、Python 3.13.15、MCP 1.30.0、Pillow 12.3.0。新增固定 pypdf 6.19.0 與 mutagen 1.48.1，requirements 與 lock 同步；用途、套件來源與部署要求見格式說明。

| 命令／驗證 | 結果 |
| --- | --- |
| `.\.venv\Scripts\python.exe -m py_compile local_files_mcp.py workspace_reader.py image_reader.py format_reader.py format_worker.py format_parsers.py media_parser.py stream_read.py stream_search.py incremental_state.py capability_diagnostics.py` | PASS；GUI 文案另執行 py_compile PASS |
| `.\.venv\Scripts\python.exe -B -m unittest discover -s tests -v` | 283 tests，0 failures／0 errors，5 skips，93.213 秒 |
| `.\.venv\Scripts\python.exe -B verify_isolated.py` | 283 tests，0 failures／0 errors，5 skips；pip check=0；隔離程序 94.913 秒 |
| 新增格式專項 `test_formats.py` | 26 tests，0 failures／0 errors，1 skip；已包含於完整測試 |
| 真實 STDIO | tools/list 契約、21 工具、readonly annotations、版本、file_info／read_document／inspect_media／inspect_archive／read_binary／binary hash、嚴格參數及 diagnostics；PASS |
| Windows parser 資源隔離 | 實際 worker、768 MiB allocation 被 512 MiB 限制拒絕、file open／socket／Popen 被 audit hook 拒絕、逾時回收及環境 key 不繼承；PASS |
| 格式／安全 | 文件分頁、格式偽裝、損毀、加密、2,001 頁 PDF、高壓縮 PDF、XML entity／節點／深度、Zip Slip／link／重複名／壓縮比／容量、巢狀封存不展開、response budget、Hidden／hard-link／模擬 reparse／TOCTOU；PASS |
| `git diff --check` | PASS |

測試使用程式生成的非敏感容器與圖片；媒體案例驗證 metadata，不宣稱內容可播放或覆蓋所有供應商產生的變體。既有文字、圖片、連線、GUI、電源 mock 與快照測試繼續通過。

## 未驗證項目

- 5 個實際 symlink 測試因目前 Windows 權限不足（1314）略過；沒有提升權限。硬連結、Hidden 真實屬性及模擬 reparse 防護已驗證。
- Linux／macOS 資源限制、Python 3.10 執行、所有第三方 Office／媒體變體未在本機逐一驗證。跨平台分支保留；本次實機環境為 Python 3.13.15 Windows。
- 正式 Connection Job 下的長期負載、遠端 ChatGPT 工具刷新／21 工具實際呼叫／新圖片視覺驗收未執行。本機 STDIO 不替代遠端驗收。
- 正式通道與既有 EXE 發行包未重建或重啟；原始碼服務須部署新增模組、安裝固定依賴後重新連線，再執行正式驗收。

## 後續啟動提示

> 請先讀 AGENTS.md、格式讀取說明.md 與 reports/engineering/formats-acceptance-20261002.md，核對目前工作目錄與 21 工具契約。延續本機驗收結果，不修改正式授權或金鑰。若已另獲重連授權，按既有 Connection 流程套用新版，從實際用戶端 tools/list／server_diagnostics 核對版本與契約，再以非敏感檔案驗收文件、媒體、封存、binary 及原生圖片；清楚分開本機與遠端證據。
