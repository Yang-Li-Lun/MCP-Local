# 格式能力擴充與重連驗收（2026-10-03）

完成本機擴充、修改後完整驗證與正式重連。服務 **2026.10.03.1**、契約 **8**、工具仍 **21 個**，設定版本仍為 5。NCUE 遠端已回報相同版本、21 工具與 consistent=true，授權 root 代號與本機一致。新選填參數的遠端呼叫仍受此聊天已載入的舊 schema 限制，須刷新連接器後另驗。

## 交付能力

- `inspect_archive`：ZIP/TAR/TAR.GZ 目錄分頁與 member byte-range；不落地、不遞迴。完整選定成員驗證後才回傳片段與 SHA-256，檢查 traversal、absolute/link、duplicate/case collision、Hidden 父目錄、炸彈、CRC／長度、descriptor、deflate EOF 與回應預算。
- `read_document`：CSV/TSV、JSON/JSONL、XML、YAML、TOML、INI、SQLite；共用分頁、型別／欄位資訊與來源位置。SQLite 僅 deserialize 到記憶體、固定讀取一般資料表，無 SQL 輸入、原始 DB 開啟或 WAL/sidecar 存取。
- 文件：ODT/ODS/ODP、EPUB、EML/MBOX；附件只回 metadata/hash。原 PDF/DOCX/XLSX/PPTX 保留。
- 圖片：ICO、PNM、現有固定 Pillow decoder 支援的靜態 AVIF；`inspect_media` 增加圖片尺寸／格式 metadata。音訊新增 AIFF/AU，影音新增 Matroska/WebM 有界 metadata；另支援獨立 SRT/WebVTT 文字與時間戳。
- `source_sha256` 與選填 `expected_sha256` 可固定續讀快照。file_info、workspace_info、server_diagnostics、契約快照及實際 registry 同步。

新增參數皆選填，root_id 保留原參數位置，既有呼叫相容；沒有為各格式另增獨立 MCP 工具。詳細語意、上限與範例見 [格式讀取說明](../../格式讀取說明.md)。

## 安全與依賴

來源保持唯讀；原有 root、路徑、Hidden、reparse/symlink/hard-link、敏感名稱／狀態目錄、TOCTOU、容量與操作預算保留。所有新 parser 共用固定 worker、memory snapshot、10 秒 wall/CPU、512 MiB、audit 外部存取阻擋。沒有 shell/executable/使用者 SQL／模型下載／外部 URL／落地 extract 能力。這不是針對未知 native exploit 的完整 OS 安全沙箱。

僅新增固定 [PyYAML 6.0.3](https://pypi.org/project/PyYAML/6.0.3/)（MIT、Python 3.10+ 相容、Windows wheels）；查核官方維護來源與授權，使用純 Python SafeLoader，額外拒絕 aliases、tags、merge、重複 key 與過量結構。既有 Pillow 12.3.0 已附 libavif 1.4.2，無額外 codec 安裝。

TOML 需 Python 3.11+，SQLite 需 deserialize/setlimit build；缺少時明確拒絕。通過 Python 3.10 語法解析不等於實際跑過 3.10 runtime。本次 runtime 為 Python 3.13.15、SQLite 3.50.4、MCP 1.30.0。

## 修改後驗證

| 指令／檢查 | 本次結果 |
| --- | --- |
| `python -m py_compile local_files_mcp.py` 及受影響 parser/reader | PASS |
| `python -B -m unittest discover -s tests -p 'test_format*.py' -v` | 58 項、0 failures/errors、1 skip；最終完整回歸再次包含全部案例 |
| `python -B -m unittest discover -s tests -v` | **315 項；310 通過、5 跳過；0 failures/errors；226.141 秒** |
| `python -B verify_isolated.py` | **315 項；310 通過、5 跳過；0 failures/errors；223.116 秒**；69 個來源語法檢查、pip check=0 |
| `python -m pip check` | PASS，No broken requirements found |
| 真實 MCP STDIO `tools/list`／`tools/call` | PASS；完整 21 工具 schema/annotations、嚴格型別、新 archive member／structured 呼叫、diagnostics 一致性 |
| worker 隔離負向 | 實際 1 GiB 配置被記憶體限制阻擋；socket、檔案、子程序、SQLite file connect／extension 阻擋；原 timeout/cancel 測試通過 |
| source SHA-256 manifest／`git diff --check` | PASS；測試後原始碼與記錄一致 |

新增 32 個測試方法，含多格式與安全負向 subtests。正常、損毀、偽裝、容量、頁面續讀、損毀尾端、回應預算、來源不變與不落地皆有真實 parser／fixture 驗證。

5 項 skip 全部因 Windows 不允許建立實體 symlink（WinError 1314），未提權：

1. `test_snapshot_link_outside_repository_is_rejected`
2. `test_new_tools_reject_symlink`
3. `test_symlink_rejected`
4. `test_symbolic_link`
5. `test_symlink_entry`

實際 Hidden/hard-link、reparse/TOCTOU 守衛與模擬連結負向另有通過案例，不能把它們冒充上述實體 symlink 測試已執行。電源整合測試／正式排程變更未執行。

## 重啟與遠端證據

使用者明確授權「完成後重啟重連」。在最終隔離測試通過後，停止本專案舊 GUI／tunnel／MCP 程序樹，沿用現有 autostart.py 系統匣與自動連線設定啟動；未更改排程。確認單一 project tunnel 與單一 MCP server 程序樹，正式 settings.json 與 api-key.dpapi SHA-256 前後完全一致，電源設定保持 OFF。

NCUE 實際遠端驗證通過：

- `server_diagnostics`：2026.10.03.1、契約 8、21 registered tools、consistent=true，契約 digest 相等，root 代號與本機一致。
- `read_file`：固定 shared/README.md 煙霧字串成功。
- `file_info`：真實內容辨識 tool-contract.json 為 JSON。
- `read_document`：JSON 結構化分頁成功，next_start=2 的續讀從 index 2 開始，來源 SHA-256 一致。
- `read_image`：固定 image-acceptance.png 回傳原生 PNG ImageContent。

一開始查到的另一個連接器回傳舊服務及不同 root 代號；確認與本專案不同後未更改它。NCUE 已更新，但此聊天載入的工具參數仍為舊快照；**遠端 member_path、format_hint、table、expected_sha256 呼叫尚未驗收**，完整遠端 tools/list transport capture 與全格式遠端矩陣也未執行。這些新參數已有本機真實 STDIO 驗收，不冒充遠端完成。

## 尚未支援

- OCR／圖片文字／音訊轉錄：缺少受稽核的固定離線模型與推論 runtime，不加入下載或外部程序。
- 影片 frame／嵌入字幕 payload／播放／轉碼：需要額外 codec/demux/時間索引，保持 metadata 與外掛字幕讀取。
- 舊 DOC/XLS/PPT、XLSB、MSG：現有依賴未提供本專案已驗證的有界 OLE/BIFF/MAPI/RTF parser。
- HEIC/HEIF/EPS/SVG 渲染、動畫／多頁、RAR/7Z/ZIP64／加密、遞迴解壓、任意 SQL/WAL 等仍不支援，詳細原因見使用說明。未知正常檔案仍可探索、metadata、hash 與 bounded raw（受既有 root 限制）。

## 工作樹與驗收檔案

起始 branch=main、HEAD=77592b1，已有多筆未提交修改及未追蹤格式模組；本次沿用並擴充，沒有 reset/stash/commit/push，既有圖示等無關工作保留。新增 structured_parser.py、document_extras.py、tests/test_format_expansion.py；共用 reader/worker/media/image、契約／診斷／版本／文件同步更新。

- [機器可讀驗收結果](formats-expanded-acceptance-20261003.json)
- [最終完整 unittest 日誌](formats-expanded-full-unittest-20261003.txt)
- [隔離測試結果](formats-expanded-isolated-results-20261003.json)
- [隔離測試完整日誌](formats-expanded-isolated-tests-20261003.txt)
- [格式專項日誌](formats-expanded-tests-20261003.txt)
- [測試來源 SHA-256 清單](formats-expanded-source-manifest-20261003.json)
