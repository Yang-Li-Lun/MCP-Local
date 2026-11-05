# MCP-Local 開發日誌

日期採台北時間，按日期記錄已完成的里程碑，附上可核對的提交或驗收報告。隔離驗證與正式驗收分開記錄。

## 2026-11-06｜安全中心滾輪與進階讀取名稱

- 修正安全中心巢狀清單及輸出區在無法繼續捲動時攔截頁面滾輪；內部內容仍可捲動時保留自身操作，並容許邊界浮點誤差。導覽及頁面標題由「進階讀取設定」改為「進階讀取」，同步更新介面說明。
- 使用既有 .NET 10 SDK 隔離建置，0 警告／0 錯誤；WPF 自測 100 項及合成 fixture 整合 404 項通過，涵蓋頁面、清單、輸出區與上下邊界的模擬滾輪事件；fixture 程序退出及清理通過。證據：`reports/validation/security-scroll-final-20261106/summary.json`。修正前清單事件攔截及邊界浮點誤差的失敗結果分別保留於 `security-scroll-nested-baseline-20261106/` 與 `security-scroll-diagnostics-20261106/`。
- 新版隔離桌面成品位於 `reports/validation/security-scroll-probe-20261106/app/`；未替換正式 `desktop/runtime/` 或重啟正式 APP／連線。實體滑鼠人工操作、正式 Tunnel 及遠端驗收未執行。

## 2026-11-06｜MCP-Local Security Center

- 完成獨立安全政策 v1、限時主機授權、簽章本機共享記憶體通訊、敏感規則、MCP 最後送出回傳防護與 metadata 安全事件；由 `security_center.py` 協調。WPF 第六頁連接私有 Python 管線，提供草稿／已儲存／執行層確認、規則編輯及合成測試、本機授權與撤銷、診斷與恢復、事件篩選／詳情／匯出／受控清除。
- 舊安裝政策缺檔預設 OFF，不自動寫入或啟用。啟用後主機命令與 stdin 必須有健康通訊及限時授權；到期可終止或保留至原期限，通訊失效要求停止主機 Job，未確認停止維持異常。恢復不授權，也不能接受外部改寫的停用政策。政策 OFF 的通訊診斷失敗不改寫既有工具回傳或阻擋既有命令。三模式 22／34／36 及四份 MCP 契約原件不變，沒有新增遠端授權工具。
- 回傳檢查涵蓋文字 JSON、structuredContent、Session、錯誤與通知；二進位、超限、未知值及不完整 buffer 拒絕。受保護 Session 執行中輸出暫存，結束後先完整檢查再分頁。安全事件只保存分類、風險、時間、ID、計數與清除鏈錨點；原始請求監控保留敏感輸入提示及證據，觀察 guard 處理後的回應。不宣稱同帳戶憑證隔離或完整網路防外洩。
- 完整離線隔離回歸：503 項，500 通過、3 項原生 AppContainer 測試按離線政策 skip，0 failures／errors。後續受影響的來源／封裝／橋接等 44 項定向驗證通過；最終安全中心 23 項全部通過，涵蓋真正 STDIO 三模式、簽章／過期／撤銷／恢復／外部政策降級拒絕、Session 完整輸出、稽核缺口及實際 PowerShell／Python／Git。147 個 Python 檔案語法解析、`pip check` 及入口 `py_compile` 通過。全套與各次定向結果分開保留，不把早期全套結果改寫成後續全套重跑。
- 最終 .NET 10 隔離建置 0 warnings／errors；WPF 監控自測 100 項通過，最終實際控制項／私有管線整合 398 項通過。深色、淺色及高對比截圖檢視發現並修正安全中心 UserControl 前景繼承，加入實際顏色斷言。可靠性驗證的 fixture 計數歸為唯讀查詢，突發監控改為等待有界收集完成後仍核對全部 32 筆及零遺失；早期逾時／收集斷言失敗報告保留。隔離 fixture 退出與清理通過。
- 標準 release STDIO：22 工具、47 次呼叫通過，合成來源內容／mtime／清冊未變；access-mode STDIO：62 次呼叫，三模式 discovery、拒絕路徑、主機父子 Job 結束通過。真實 Windows Sandbox VM 與 AppContainer 原生整合未執行，不能由替身推論通過。
- 封裝白名單與完整來源清單加入模組、UI、測試及文件。新增 `--desktop-artifacts`，核對目前來源識別與成品雜湊後封裝，不更新正式 runtime；修正建置腳本為 UTF-8 BOM，Windows PowerShell 5.1 語法解析通過。最終精簡部署候選 151 檔解包與 SHA-256 核對通過，ZIP SHA-256：`2a0cd585c017fb9b72c1f928e16a54c8f7ed92c0237d82e068b7ad0ea292f605`。不含供應商成品、正式設定或憑證。
- 證據入口：`reports/validation/security-center-final-20261106/REPORT.md`；完整回歸 `security-regression-20261106/engineering-results.json`；最終安全執行層 `security-compatibility-final-20261106/engineering-results.json`；最終 WPF `security-desktop-acceptance-20261106/summary.json`；STDIO 報告位於 `security-enforcement-final-20261106/`。初始受限 token／TEMP 被環境限制拒絕後，改用一般未提升 Windows 帳戶的同一隔離流程，未放寬保護或修改 ACL。
- 來源基底 `b5f31a731e0893f3fdde51af33ad3ca6dcf0b1c6`，本次尚未提交、未 Push。未替換正式 `desktop/runtime/`、未重啟正式 APP／通道、未讀正式憑證、未操作 NCUE、正式共享範圍、排程、防火牆或系統電源。正式 Tunnel、人工多 DPI／多螢幕、鍵盤與輔助技術、真實長時間負載、OS 原生高對比切換及實際 FPS 未驗收。

## 2026-11-05｜系統匣權限模式切換

- 後續依使用者要求，系統匣切換略過權限模式確認，正常切換保持主介面隱藏；設定頁仍保留原確認，錯誤處理保持原有提示。此決策取代下列初版系統匣權限確認行為。相同三模組隔離復測 58 項全數通過，新增斷言確認系統匣切換不呼叫確認視窗且主介面保持隱藏；139 個 Python 檔案語法解析及 pip check 通過。證據：`reports/validation/tray-silent-switch-20261105/engineering-results.json`。未重啟正式 APP，未執行人工／LIVE 驗收。
- 移除右鍵選單的「開啟快捷設定」，保留左鍵開啟視窗；右鍵直接列出唯讀、開發控制及主機控制，勾選已儲存模式。
- 選擇其他模式沿用既有權限確認、設定儲存與連線生命週期：儲存成功後停止舊連線及 sessions，確認清理完成才重連；未連線時儲存後啟動。忙碌、修復及清理失敗時停用切換；取消確認還原原模式草稿與 dirty 狀態，儲存失敗不停止現有連線。
- 執行 `.venv/Scripts/python.exe -B tools/verification/verify_isolated.py --offline --tests test_desktop_controller test_desktop_bridge test_windows_desktop --output-dir reports/validation/tray-modes-20261105-normal`：58 項通過、0 failures／errors／skips；139 個 Python 檔案語法解析及 pip check 通過。原生選單選取使用 Windows API 替身，非人工點選驗收。見 `reports/validation/tray-modes-20261105-normal/engineering-results.json`。
- 初次受限 token 執行被前置檢查拒絕（0 項執行），改在一般未提升 Windows 使用者環境完成同一隔離驗證；保留原始環境拒絕紀錄 `reports/validation/tray-modes-20261105/engineering-results.json`。未操作正式連線、設定、憑證或系統電源，未執行完整回歸、真實 VM／遠端重連及人工系統匣驗收。Python 後端變更需重開 APP 載入，未重啟使用中的正式 APP。

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
