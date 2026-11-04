# WPF 桌面

正式 GUI 為 `desktop/runtime/McpLocal.Desktop.exe`。連線設定、進階讀取設定、外觀、權限模式及請求監控共用一個 WPF 主視窗；獨立監控視窗仍使用相同元件與後端。關閉／最小化主視窗會縮到系統匣，「更多 → 退出程式」停止自己的連線後結束。

需要 Windows、.NET 9 Windows Desktop Runtime 與專案 `.venv`。從來源建置另需 .NET 9 SDK；沒有 NuGet 套件或外部還原來源。

來源介面使用官方 Fluent Theme 與共用設計資源，細節見 [桌面設計系統](DESIGN.md)。外觀頁提供跟隨 Windows／淺色／深色；色彩選擇僅本次程序生效。主視窗、獨立監控及診斷視窗同步，Windows 高對比優先。來源修改須重新建置才會進入指定執行檔；隔離建置不會自動更新正式 runtime。

```powershell
.\desktop\Build-Desktop.ps1
.\.venv\Scripts\pythonw.exe -B local_files_gui.py
```

`MCP-Local.exe`、`開啟連線設定.vbs`、`local_files_gui.py` 與登入入口 `autostart.py` 都進入同一個 WPF 桌面，不提供其他 GUI fallback。未建置或缺少 runtime 時會回報啟動失敗。來源／部署都使用固定 `desktop/runtime/` 位置；建置輸出不提交 Git。

## Python 邊界

WPF 啟動專案 Python 的 `mcp_local.desktop_bridge` 子程序，透過繼承的私有 stdin/stdout 傳遞有界 JSON 訊息，不建立 TCP／HTTP／命名 pipe 服務。命令只包含桌面設定、既有連線操作、監控摘要與選取詳情；沒有任意 Python、shell 或新增 MCP 工具入口。

`desktop_controller` 保留設定 revision／衝突、模式風險確認、失效根修復、單一背景交易、停止取消重連意圖、重試及退出順序。它直接使用現有 `Connection`、設定驗證、DPAPI、Job與原生 Tray。預存金鑰只留在 Python；WPF 密碼欄留白表示保留原值，使用者新輸入只經私有管線傳給 DPAPI 保存。此管線不是相同 Windows 使用者之受信任主機命令的隔離邊界。

既有登入排程 action 與 LeastPrivilege 定義維持相容。啟動器透過 stdin 壽命通知 WPF 父程序已結束；WPF 關閉後端管線後，由 Python 停止其管理的 Job 並撤銷監控。獨立 VM guard 不由前端強制終止。系統匣訊息與背景工作共同喚醒 Python Windows 訊息迴圈。舊電源日誌存在時，啟動前只執行回復；回復失敗保留日誌並停止啟動。

監控容量仍為原本 200 筆／32 MiB 歷史、每個請求／回應約 1 MiB、32 層及 10,000 節點。WPF 週期更新只取得摘要，選取後才取得完整已擷取內容，全文搜尋在 Python 執行。管線解碼、JSON 格式化與語法分析在背景執行；視窗尺寸變動不重新格式化詳情。正式內容不自動寫入檔案；複製、手動匯出保留 `mcp-local-request-monitor-v1` 及結構化截斷註記。

## 隔離驗證與人工縮放

需保留正式 `desktop/runtime/` 時，指定另一個建置與中間輸出目錄，並將同一執行檔交給驗證及互動入口：

```powershell
.\desktop\Build-Desktop.ps1 -OutputDirectory .\reports\validation\my-ui-check\app -IntermediateDirectory .\reports\validation\my-ui-check\obj
.\.venv\Scripts\python.exe -B tools/verification/verify_desktop.py --no-build --executable .\reports\validation\my-ui-check\app\McpLocal.Desktop.exe --output-dir .\reports\validation\my-ui-check\checks
.\desktop\Start-Isolated.ps1 -noBuild -ExecutablePath .\reports\validation\my-ui-check\app\McpLocal.Desktop.exe
```

指定 `--executable`／`-ExecutablePath` 必須搭配停用建置參數，避免意外建置至預設 runtime。整合測試的 `screenshots/` 保存合成資料的五頁深淺色、獨立視窗及窄視窗 WPF 渲染畫面。

```powershell
# 建置、WPF 控制項與真實 Python 管線／合成 Job 整合檢查
.\.venv\Scripts\python.exe -B tools/verification/verify_desktop.py --output-dir <新的報告目錄>
# 加入整個桌面視窗的程式化縮放量測
.\.venv\Scripts\python.exe -B tools/verification/verify_desktop.py --benchmark --output-dir <新的報告目錄>
# 可直接操作全部頁面的合成環境
.\desktop\Start-Isolated.ps1 -noBuild
# 200 筆、約 2,652 萬回應字元；四筆／100 ms 更新，可直接拖曳邊框
.\desktop\Start-Isolated.ps1 -noBuild -load
```

隔離環境只使用新建的專用暫存目錄、合成 DPAPI 金鑰與受控 Python 子程序，排程、電源及 Tray 使用替身；正式狀態、網路連線、VM 及非 Python 子程序由 fixture 防護拒絕。未加 `-load` 時，可按「啟動連線」操作合成連線與監控。隔離視窗關閉或「更多 → 退出程式」會結束程序，啟動腳本隨後清理其新建 fixture；縮小則留在工作列。原生系統匣另由 `tests/test_windows_desktop.py` 使用自己建立的圖示驗證。

負載模式選取最後一筆長 JSON 並開啟換行；`F9` 開始、`F10` 結束並手動保存量測。取消存檔可再次按 F10。報告包含心跳、渲染回呼、尺寸事件、詳情重建與 WPF CPU 時間；不是實際呈現 FPS、輸入延遲或人工流暢度驗收。使用者須另外判定連續拖曳、閃爍／殘影、閱讀位置、不同 DPI 與多螢幕的表現。

正式模式使用原生系統匣，關閉／最小化主視窗保留程式與連線。

`--fixture-native-tray` 必須搭配新建空目錄的 `--fixture`，會建立提示文字帶有 `WPF 隔離測試` 與目錄尾碼的獨立原生圖示，並讓隔離視窗沿用正式的隱藏／還原／退出路徑。設定、金鑰、連線、排程、電源及 VM 防護仍由 fixture 管理，只有系統匣使用原生實作。fixture 內的 `native-tray-events.json` 僅保存最多 64 筆圖示生命週期／滑鼠事件，不保存正式監控內容。一般 fixture 仍使用系統匣替身。

## 原始碼與測試

`DesktopWindow` 管理五個頁面；`BackendClient` 與 `RemoteMonitorSource` 管理私有通訊及監控 DTO；`MonitorView`／`JsonPane` 提供篩選、歷史、分頁、欄位、尋找、複製與匯出。「JSON（上色）」保留有界語法上色閱讀，預設 JSON 使用已驗證的文字閱讀路徑。

`MonitorChecks`、`DesktopChecks` 是正式 WPF 回歸，只有明確測試參數才啟動；Python 回歸包含控制器、啟動父程序、管線、共享根、真實原生系統匣與 Job。正式設定、Credential、排程變更、系統電源變更、LIVE 遠端連線、實際 VM 與登入／登出驗收需各自授權，不能以合成結果代替。

本機整合後的連續邊框縮放已由使用者人工確認流暢。原生剪貼簿、資料夾選擇、JSON 存檔對話框、設定儲存及正常退出的隔離操作已完成。原生系統匣同組程序的隱藏、真實點擊還原與正常退出已確認，原始事件與雜湊見本機 `reports/validation/gui-wpf-native-tray-20261101/CLOSEOUT-20261102.md`；原輪部分通過紀錄保留。隔離桌面已在目前 100%／175% 顯示設定完成跨螢幕基本操作與注音組字驗證；現行視窗為 system-DPI-aware，不能將系統縮放當成原生 per-monitor DPI 重繪通過。持續負載的實測期間、資源觀察與尚未驗證項目見 `reports/validation/gui-wpf-desktop-closeout-20261102/REPORT.md`。正式環境、其他縮放比例、螢幕熱插拔、數小時／跨日流量仍需獨立驗收。受阻清理紀錄保留於 `reports/validation/gui-wpf-final-acceptance-20261101/`，不以 UI 心跳或渲染回呼替代實際 FPS。
