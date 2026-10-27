# 文件索引

本文件區分現行使用說明、架構規格、歷史研究、驗證證據與部署產物。文件中的命令預設在專案根目錄執行。
公開產品維持 `read_only`、`developer_control`、`host_control`；第四模式 `restricted_host_control` 維持 **PAUSED**。

## 開始使用與部署

| 文件 | 用途 |
| --- | --- |
| [README](../README.md) | 專案入口、功能與快速開始 |
| [完整繁體中文說明](../README_繁體中文.md) | 多根、容量、游標、設定遷移與錯誤處理 |
| [Windows 部署與封版](../RELEASE.md) | 安裝、固定依賴、封裝及部署驗證 |
| [專案結構與開發入口](PROJECT_LAYOUT.md) | 套件、工具、契約、歷史輸出與相容啟動入口 |
| [LICENSE](../LICENSE)／[NOTICE](../NOTICE) | 授權與供應商聲明 |

## 操作與工具說明

| 文件 | 用途 |
| --- | --- |
| [介面使用說明](guides/介面使用說明.md) | GUI、系統匣、連線與權限選項 |
| [自啟動與優化說明](guides/自啟動與優化說明.md) | 自啟動、重連、保存與效能工具；保留的舊版本段落依文內更新註記閱讀，現行操作以介面說明及 README 為入口 |
| [格式讀取說明](guides/格式讀取說明.md) | 格式能力、工具參數、安全限制與 query_files |
| [圖片讀取說明](guides/圖片讀取說明.md) | 原生圖片回應與靜態解碼邊界 |
| [增量與診斷工具說明](guides/增量與診斷工具說明.md) | 基準生命週期、差異語意及診斷 |

## 架構與契約：architecture/

| 文件 | 用途 |
| --- | --- |
| [權限模式規格](architecture/完整控制模式.md) | 三模式架構、設定切換、VM／受信任主機邊界及驗收限制；保留原檔名 |
| [唯讀工具契約](../contracts/tool-contract.json) | 22 個唯讀工具快照 |
| [開發控制工具契約](../contracts/tool-contract-developer-control.json) | 34 個開發控制工具快照 |
| [主機控制工具契約](../contracts/tool-contract-host-control.json) | 36 個主機控制工具快照 |
| [內部完整控制契約](../contracts/tool-contract-full-control.json) | 保留的 AppContainer 共用／回歸快照，不是第四個公開模式 |

## 歷史設計與交付文件：history/

| 文件 | 分類與閱讀方式 |
| --- | --- |
| [低功耗模式工程設計](history/低功耗模式工程設計.md) | 已被取代的三態歷史設計；目前電源操作見 README／介面說明，不以舊設定版本或 LOW 策略作現行規格 |
| [性能優化交付報告](history/性能優化交付報告.md) | 2026-09-17 歷史量測與當時交付紀錄 |
| [體感效能優化交付報告](history/體感效能優化交付報告.md) | 2026-09-13 歷史實作與驗證紀錄 |

這三份文件受既有歷史引用與封裝清單使用，保留原名、原內容；舊測試與量測不等於目前版本或遠端驗收通過。

## 原始碼與本機工作樹的開發入口

`AGENTS.md`、建置工具與測試隨完整 Git 來源提供；精簡部署 ZIP 不包含它們。`reports/` 的工程、效能與第四模式封存證據只保留於本機，不隨 Git 來源候選或部署 ZIP 提供。下列證據連結僅適用保有歷史資料的本機工作樹。部署使用前述操作文件與契約即可。

| 入口 | 用途 |
| --- | --- |
| [工程統一索引](../reports/engineering/INDEX.md) | 按主題查找工程批次、驗證證據與 legacy-root 歷史輸出 |
| [效能紀錄索引](../reports/performance/INDEX.md) | 效能報告、歷史量測及原始資料位置 |
| [第四模式封存索引](../reports/engineering/restricted-host-archive-20261026/README.md) | PAUSED 封存與八個原地研究目錄；不代表開發或 VM 操作授權 |
| [儲存庫指南](../AGENTS.md) | 模組責任、開發與安全規範 |
| [固定封裝白名單](../tools/build_release.py) | 部署檔案集合與 SHA-256 核對流程 |
| [測試程式](../tests/) | 原始碼回歸；不隨精簡部署包提供 |

## 本機產物與原始路徑

`release/` 是既有部署 ZIP、manifest、驗證紀錄與外部 SHA-256 的位置；`.release-work/` 是過往封裝／驗證工作區。
`archive/backups/` 保存既有備份；`.venv*`、`NVIDIA Corporation/` 等既有環境或用途未確認項目保持原位。官方通道成品集中於 `third_party/tunnel-client/`。
這些本機目錄不一定存在於 clean clone，不因索引收錄而取得清理、重建、覆寫或部署授權。
原根目錄 `engineering-*`、`performance-*` 已分類至 `reports/engineering/legacy-root/` 與 `reports/performance/legacy-root/`，內容不變。歷史清冊保留當時路徑；完整對照見工程統一索引。
