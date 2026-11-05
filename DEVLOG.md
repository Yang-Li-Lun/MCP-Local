# MCP-Local 開發日誌

日期採台北時間，按日期記錄已完成的里程碑，附上可核對的提交或驗收報告。隔離驗證與正式驗收分開記錄。

## 2026-11-05｜.NET 10、桌面查詢更新與成品識別

- WPF 升至 .NET 10 LTS x64，同步建置／部署文件、runtime 缺少提示及部署摘要；沿用 Python、私有 stdin/stdout 與三種模式。摘要按紀錄 revision 重用、狀態未變略過 WPF 刷新，背景收集頻率維持原值。
- 唯讀查詢加入 3 秒期限與最後成功更新時間；操作回覆為 15 秒，送出後逾時保留結果尚未確認，阻擋重送。原回覆不會覆寫或確認較新的未送出草稿。既有診斷區分前端內嵌識別／MVID、Python 實際載入來源、磁碟來源、來源基底 commit、部署摘要與程序啟動 UUID；不能確認者明示未知。
- 最終建置 0 warning／0 error、WPF 自測 100 項及隔離整合 374 項通過；相關 Python、主機 token 定向復測、啟動器與封裝清單驗證通過。最初完整隔離回歸為 477 項、14 failures／48 errors；58 個不同失敗案例改在一般未提升 Windows token、系統 TEMP 下復測全部通過。新增環境檢查，受限 token 或受保護 TEMP 在執行測試前以 `ENVIRONMENT_NOT_READY`、exit 2 停止；不放寬服務安全防護。
- 後續完整隔離回歸執行 480 項，478 通過、2 failures、0 errors、0 skips；兩個 Windows 8.3／完整路徑比較斷言修正後，所屬模組分別 3 項、6 項定向復測通過。以完整結果加定向復測核對全部 480 個案例，目前沒有未解失敗；未重跑條件未變且已通過的全套案例，也未把原始失敗報告改成成功。見[回歸異常處理與逐項證據](reports/validation/regression-closeout-20261105/REPORT.md)。
- 依使用者後續重啟重連授權，部署最終六檔並正常釋放舊版所屬程序，透過既有登入入口重新連線。實際 WPF 載入系統 .NET／Windows Desktop Runtime 10.0.12，唯讀連線診斷回覆契約 13／MCP 1.30.0 且註冊一致。未直接修改正式設定、憑證、共享根或模式；未操作 NCUE、未 Push。部署當時尚未提交，來源基底 `793ebe256305f196c392bbae876a19c972e04b99` 加工作樹修改，以部署 manifest 的來源雜湊辨識；後續本機提交不會追改已部署成品的識別。
- 人工多 DPI、長時間負載、實際 FPS、正式診斷視窗與系統匣互動未驗證。正式設定直接讀取檢查遭自動審查拒絕，取消該項，沒有做位元組前後比對。見[完整證據與限制](reports/validation/desktop-net10-20261105/REPORT.md)及[最終部署摘要](reports/validation/desktop-net10-20261105/deployment.json)。

## 2026-11-04｜請求監控整頁 JSON 與 WPF 原始碼同步

- 正式 runtime 更新請求監控的整頁 JSON。詳情頁使用與列表一致的六欄表頭，可按篩選與排序後的順序切換上一筆／下一筆，窄視窗保留 JSON 閱讀空間。離線建置零警告、零錯誤，最終隔離自測 100 項、整合 331 項通過，部署五檔與候選的 SHA-256 相符。此次正式 GUI 啟動、LIVE 連線、人工拖曳、實際 FPS、多 DPI／多螢幕及長時間負載仍未驗證。見[整頁 JSON 部署](reports/validation/wpf-monitor-full-page-deploy-20261104/REPORT.md)與[最終部署報告](reports/validation/wpf-monitor-table-deploy-20261104/REPORT.md)。
- Git 提交 `793ebe256305f196c392bbae876a19c972e04b99` 收錄 Windows WPF 桌面原始碼、建置入口與桌面驗證工具。正式 runtime 的部署身分見[部署清單](reports/validation/wpf-monitor-table-deploy-20261104/deployment-manifest.json)。

## 2026-11-02｜Fluent WPF 正式部署與短期驗收

- 部署 Fluent 桌面，正式 GUI 啟動與自動連線通過短期驗收。驗收涵蓋唯讀 MCP 診斷與固定檔案讀取、主介面與進階設定，以及嵌入和獨立監控的請求／回應檢視及即時更新。回退、長時間驗收與全庫回歸未執行。見[部署與短期正式驗收報告](reports/validation/wpf-fluent-deploy-20261102/REPORT.md)。

## 2026-11-01｜正式桌面遷移至 WPF

- 正式桌面與部署入口改用 WPF，沿用 Python 後端及設定、安全與生命週期契約。隔離原生驗收涵蓋設定儲存、剪貼簿、資料夾選擇、JSON 匯出及正常退出。使用者確認連續縮放流暢，實際 FPS 未量測。該次原生系統匣元件回歸通過，完整 WPF 視窗縮入系統匣再從圖示還原仍待人工確認。見[WPF 驗收報告](reports/validation/gui-wpf-final-acceptance-20261101/REPORT.md)。
