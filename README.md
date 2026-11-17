# MCP-Local Rust

Windows x64 原生桌面與 STDIO MCP 服務，使用 Rust、官方 Rust MCP SDK 與 Slint Fluent。

## 建置與啟動

使用 Rust 1.95 以上及對應的 Windows C/C++ 工具鏈。請在一般 Windows 終端機執行測試，格式解析測試需要建立 AppContainer。將 Cargo 輸出指向專案外的專用目錄。

```powershell
$env:CARGO_TARGET_DIR = Join-Path ([IO.Path]::GetTempPath()) 'mcp-local-rust-build'
cargo build --locked --release
cargo test --locked
```

無參數啟動桌面。指定 `--root` 啟動 STDIO，或使用原版 Base64 格式的 `--workspace-settings` 與 `--reader-settings`。桌面使用 `%LOCALAPPDATA%\MCP-Local-Rust`。`--import-settings <快照副本>` 從原版 v0–v8 的相容一般設定載入草稿。v0–v6 與已退場的 `full_control` 會載入為唯讀。匯入保留 Rust 桌面的既有 DPAPI 密文，儲存時依相同的模式確認流程處理。

桌面儲存時比對設定檔版本。磁碟版本有變更時，可重新載入一般設定，已輸入的 API 金鑰仍留在表單。重連前會再核對磁碟版本與表單。若有新的編輯或磁碟更新，請確認設定後重新啟動連線。

已保存自動連線設定時，手動開啟桌面與登入啟動都會經過相同的連線檢查後自動連線；匯入設定快照不會觸發連線。登入自啟動設定預設關閉。啟用需有可用的 DPAPI 金鑰，並在目前使用者的 Windows 本機設定目錄執行。桌面以最低權限註冊自己的 Windows 工作排程，工作只指定原生執行檔、工作目錄與固定登入參數。程式會保留設定不符的同名工作。登入後可從系統匣或再次啟動桌面開啟同一視窗，使用原有啟停與監控。匯入快照只載入草稿。正式登入及取消排程的實機驗收仍待完成。

登入背景連線遇到已分類的通道啟動、診斷或程序中斷時，會在完成資源與安全連線清理後依 5、15、30、60、300 秒基準自動重試。各延遲有 ±10% 浮動。可按「停止連線」取消，設定有未儲存的編輯或已從外部更新時會暫停。供應商的原始輸出不進入診斷。

登入背景程式在本機重新啟動連線或儲存並重連時，若登入自啟動與自動連線仍開啟，會重新啟用重試。取消登入自啟動只影響後續登入與重試，不停止正在執行的連線；停止目前連線請使用「停止連線」。通道啟動前會依隨附供應商版本檢查識別碼：`tunnel_` 後須為 32 個小寫英文字母或數字。

## Windows 部署

以預設功能建置 x64 Release，再將成品打包到已有的專案外目錄。MSVC x64 目標預設靜態連結 C 執行階段。打包會檢查 x64 PE 標頭，拒絕含 CLR 執行階段標頭的執行檔。部署包包含原生執行檔、專案授權、`THIRD-PARTY-LICENSES.txt` 與 `SHA256SUMS.json`。授權彙整以 Windows x64 GNU／MSVC 的預設相依圖為準，保留上游授權文字與聲明。變更 `Cargo.toml` 或 `Cargo.lock` 後須更新對應聲明及以 LF 正規化後的 SHA-256。打包會拒絕版本不符的授權文件。

加上 `-VendorArchive` 可納入已取得的 [OpenAI 官方 Windows x64 v0.0.14](https://github.com/openai/tunnel-client/releases/tag/v0.0.14) ZIP。打包全程離線，核對官方 ZIP 及其每個檔案的 SHA-256，並在完成前重讀輸出驗證。輸出 ZIP 已存在時會停止。

```powershell
.\Package-Windows.ps1 -Executable "$env:CARGO_TARGET_DIR\release\mcp-local-rust.exe" `
    -OutputZip 'C:\deploy\MCP-Local-Rust.zip'
```

解壓後直接執行 `mcp-local-rust.exe`。含通道的封包會保留 `third_party\tunnel-client` 子目錄及官方授權、NOTICE、SPDX。桌面連線會驗證 `tunnel-client.exe`、`cloudflared.exe` 及其 manifest，並在執行通道前套用 Windows Job Object。可由 PowerShell 執行 `(Start-Process .\mcp-local-rust.exe -ArgumentList '--verify-vendor' -Wait -PassThru).ExitCode` 檢查部署目錄。退出碼 `0` 代表三個通道檔案通過驗證，此命令不執行通道。唯讀 STDIO 可使用一般部署包。

## 功能狀態

唯讀服務註冊原契約的 22 個工具。`workspace_info` 列出可用格式與限制，`server_diagnostics` 回報工具註冊與模式狀態。圖片、文件、封存檔及影音解析程序在無網路能力、禁止子程序的 Windows 低權限 AppContainer 中執行。父程序確認容器權限與 Job Object 後，才經私有管線送入有上限的位元組快照。解析結束時清除當次容器，隔離建立或清理失敗時回報 `FORMAT_ISOLATION`。

桌面請求監控從 STDIO 傳輸擷取工具呼叫及回應，近期紀錄保留於記憶體。主視窗和獨立監控都顯示 STDIO 接入狀態，以及目前保留、成功回應與失敗的紀錄數。篩選只影響列表顯示。進階檢視顯示目前的累計計數、已接入服務的模式與版本，以及選取紀錄的擷取註記。可搜尋、暫停、清除，並檢視、複製或匯出選取的紀錄。匯出保留該筆紀錄擷取時的權限模式。獨立監控和監控診斷沿用相同主題。

系統匣提供開啟主視窗、請求監控、啟動／停止連線及三種模式切換。模式切換沿用本機確認、儲存與清理後重連；清理失敗不重連。主視窗最小化或關閉時可縮到系統匣。系統匣未就緒時，視窗保留在工作列，收到 Windows 的工作列建立通知後會重新註冊圖示。

監控診斷顯示桌面與已接入 STDIO 程序各自的啟動識別、時間、PID、編譯器及目標平台。來源 SHA-256 在建置時從 Cargo 設定、build.rs、src、ui、assets、contracts 與本機 vendor 內容計算並嵌入；不以磁碟版本推定正在執行的版本。未接入或無法讀取後端識別時顯示空值。這些識別不代表執行檔簽章或遠端認證。

Windows 高對比優先於手動淺色／深色，並同步套用至主視窗、獨立監控及關於視窗。高對比使用 Windows 系統色和高對比選取色。JSON 保留文字、選取與搜尋，語法字色改用系統文字色。退出高對比時恢復工作階段中的主題選擇。控制項沿用鎖定版本的 Slint Fluent 元件。

SQLite 需要 Windows 內建元件提供受限的記憶體快照 API。虛擬資料表模組全程停用。含 FTS、R-Tree 或 Geopoly 的資料庫仍可讀取一般表，排除虛擬表及其內部表。無法辨識虛擬表的內部表規則時會拒絕快照。

AVIF 支援有界單張解碼，包括直接由 AV1 圖塊組成的格狀影像、整張透明度影像和逐塊透明度。色彩和透明度格各最多 64 塊，圖塊在裁切前的像素總數也受原圖像素上限約束。多幀和不支援的衍生結構會被拒絕。

安全中心管理本機安全政策 v1、合成預覽及安全事件。啟用回傳防護後，STDIO 會依政策遮罩或阻擋送出的資料。桌面啟動的連線使用 Ed25519 簽署的本機共享記憶體傳遞政策，並以 HMAC 驗證執行層回報。桌面區分已儲存政策和執行層確認，安全事件保留固定欄位及 SHA-256 事件鏈。連線中斷會撤銷授權，復原需要本機操作。

桌面預設唯讀。從「權限模式」切換至開發控制時，儲存前會顯示 VM 可直接讀寫的資料夾並要求確認。已連線時儲存新模式會先停止舊連線及工作階段，再重連。開發控制以簽署的本機安全連線啟動 34 個工具，通道的 Windows Job 外另有 VM 守護程序。服務與守護程序認證後比對同一份共享範圍及讀取設定。通道停止時須確認 Job 和 VM 均已清理。

主機控制服務註冊 36 工具契約、檔案操作、PowerShell 工作階段、stdin、輸出分頁、終止、關閉及檔案匯出。主機程序以目前未提升權限的使用者執行，沒有 VM 隔離。啟動時僅繼承明列的一般 Windows 環境（包含 PATH、PATHEXT、使用者與安裝位置、PSModulePath），不傳入任意 API 金鑰或工具注入變數；固定 COMSPEC，TEMP／TMP 指向工作目錄。以 Windows 內建 PowerShell 的 `-NoProfile -NonInteractive -EncodedCommand` 執行；程序先暫停並納入 Job，複核簽署的政策與限時授權後才恢復。Job 提供 32 個程序及 4 GiB 合計記憶體上限，禁止 breakaway，關閉時終止其所有程序。每個連線同時最多一個主機工作階段，最多保留 16 筆記錄，已停止記錄於 15 分鐘後可回收。單次 stdin 上限 4096 bytes，輸出上限 1 MiB，存活期限 1–3600 秒；stdin 阻塞、取消、逾時及無法確認清理時失敗關閉。

主機命令與 stdin 在實際使用前複核授權，檔案變更在提交前複核同一份授權快照。到期依既有政策終止或允許既有命令完成，新的命令及 stdin 仍須有效授權；失去安全通訊或關閉連線時終止工作階段。回傳防護先檢查完整輸出，再依 Unicode 字元分頁，送出前再次核對政策版本。終止與關閉工具不要求仍有效的命令 lease，但安全連線必須可確認。

三種模式均提供桌面、CLI 與通道入口。主機控制仍必須接入簽署的本機安全通道；啟用主機授權政策時，命令及檔案變更另須有效的限時授權。桌面從其他模式切換為主機控制會要求本機確認。`mode_ready` 反映本機後端前置檢查；它不代表正式遠端或完整實機驗收已通過。開發模式診斷會檢查 Windows Sandbox CLI 的可信安裝位置，後端缺失時回報不可用，沒有主機 fallback。

Windows 工作排程器另有離線測試 `startup::tests::task_scheduler_validates_xml_without_registering_a_task`，只以 `TASK_VALIDATE_ONLY` 驗證 XML 與 Windows 正規化後的所有權比對，並確認隨機測試名稱前後均不存在，不建立或刪除工作。測試需在一般 Windows 使用者環境執行；VM 守護程序的合成測試還要求測試程序不在禁止 breakaway 的外層 Job 中。測試資料目錄須在專案外，且不可位於 `.codex` 等既有路徑封鎖範圍。

開發 VM 從 Windows 安裝位置偵測唯讀工具鏈，驗證映射範圍與檔案別名。`start_process` 在背景啟動 VM 前即可回報已驗證的映射。VM 內的固定轉接程式使用 Windows 內建 PowerShell。主機端由 Rust 驗證路徑，並透過授權映射根交換有上限的封包。本機 HMAC 管線以工作階段 ID 驗證輸入、輸出和停止請求。服務端以 UTF-8 串流解碼輸出，依 Unicode 字元位置分頁。回傳防護啟用時會先檢查完整輸出，再取分頁。確認停止後可讀取已保存的輸出與結束狀態，也可匯出工作目錄內的 UTF-8 檔案。

Slint 採用 Royalty-free Desktop、Mobile and Web Applications License v2.0，桌面程式的「說明 → 關於 MCP-Local」提供官方 `AboutSlint` 標示。 [Slint 授權條款](https://github.com/slint-ui/slint/blob/v1.19.0/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md)

## 本機 CLI 與一般設定回復

`--connect [--access-mode read_only|developer_control|host_control]` 使用 Rust 自己保存的設定啟動前景連線，模式覆寫只影響此次連線。沒有已保存金鑰時，可從本機主控台隱藏輸入，僅用於此次連線。輸入 `stop` 或 Ctrl+C 會停止連線並清理；`authorize <分鐘>` 僅接受本機互動式主控台，依安全政策授予限時主機權限，`revoke` 撤銷授權。CLI 不接受命令列明文金鑰。

`--set-access-mode <模式>` 保存 Rust 本機模式。已有 Rust 桌面時，請求交給該桌面，依相同確認及清理後重連流程處理；未完成的表單編輯不會被覆蓋。CLI 只在確認設定已保存後回報成功，遠端可用性仍需另外確認。

設定變更前，Rust 在自己的 `settings-backups` 目錄保留最多五份一般設定快照，不包含金鑰或 DPAPI 密文。快照可透過 `--import-settings` 載入草稿；保存時仍保留現有 Rust 金鑰。無法辨識或已被修改的備份檔保留，不納入自動輪替。

舊版電源模式不提供啟用入口。若發現既有 `power-runtime.json`，通道啟動會停止，直到本機使用者明確執行 `--recover-legacy-power <日誌路徑>`。此命令只回復已結束程序留下的有效交易：保留使用者後來選取的電源設定，失敗時保留日誌；會修改系統電源及刪除已回復的交易日誌，不能作為一般無副作用診斷。一般設定匯入不執行電源回復、不讀取舊版憑證，也不啟用已廢止的第四模式。

`tests/compare_legacy.py` 是可選的新舊程式差異驗證工具，僅使用暫存合成資料及獨立設定目錄。舊版 Python 僅供測試比較，Rust 正式執行不依賴它。比較仍須區分隨機識別碼、檔案存取時間及解析器／圖片編碼差異。
