# 原生圖片工具驗收紀錄 — 2026-09-29

## 實作

服務 `2026.09.29.2`、契約 6、設定版本仍為 5；MCP SDK 1.30.0、Pillow 12.3.0。新增 `read_image(path, root_id?)`，保留既有 15 工具。開始工作時已存在未提交的增量／診斷功能；本次基於該狀態增加圖片能力，未提交或推送。

主要改動：`image_reader.py`、共用 `FileReader.open_checked` 的圖片模式與結束時檔案身分核對、`WorkspaceReader.read_image`、MCP 工具註冊、`operation_budget.py` 原生圖片輸出預算、診斷 image_limits、版本／契約／依賴／AGENTS 與使用文件。

支援經實際解碼驗證的 PNG/JPEG/WebP，副檔名必須吻合，拒絕動畫及多幀。來源上限為 16 MiB 與既有 root.max_file_bytes 兩者較小值，來源每邊最多 16,384、總像素最多 25,000,000。縮圖完全在記憶體中進行，不放大，回傳每邊最多 2,048 的 PNG，移除 metadata、保留 alpha、套用 EXIF 方向。

回應為 `CallToolResult.content=[TextContent(metadata), ImageContent(type="image", mimeType="image/png", data=Base64)]`，`structuredContent=null`；Base64 不包在一般文字或 JSON 回應中。PNG 最多 3 MiB、Base64 最多 4 MiB、圖片回應最多 4 MiB + 64 KiB；原有文字 JSON 2 MiB 上限不變。

共享根、Hidden、點號檔、符號／硬連結、reparse point、排除名稱、狀態／設定／金鑰目錄、讀取前後檔案身分與變更檢查皆沿用。原有文字列舉功能仍只列文字檔；第一版須提供已知圖片相對路徑。合作式取消不能強制中斷正在執行的原生解碼。

## 驗證

- `python -m py_compile local_files_mcp.py workspace_reader.py image_reader.py operation_budget.py capability_diagnostics.py`：PASS。
- `python -B verify_isolated.py`：完整 unittest discover、語法、pip check；最終統計見 `image-tests-20260929.json`，明細見 `image-tests-20260929.txt`。
- 新增 20 項圖片測試：PNG/JPEG/WebP、損毀／CRC／JPEG 缺尾、格式偽裝、來源容量／root 設定上限、尺寸／像素上限、縮圖／傳輸超限、2 MiB 以上原生回應、metadata／alpha／EXIF、動畫、越界、Hidden（含 Windows 屬性與父目錄）、link、排除名稱／狀態目錄、檔案身分／讀取中變更、取消、嚴格參數、來源 SHA256／mtime 不變及真實 STDIO。
- 真實 STDIO `tools/list`：16 工具、schema 與唯讀 annotations 符合 tool-contract.json。
- 真實 STDIO `tools/call read_image`：`content` 含 `type:image`；MIME 為 image/png，Base64 嚴格解碼成功，Pillow 完整 load 成功；獨立大圖案例超過舊 2 MiB JSON 限制仍成功。
- 正式非敏感驗收圖：`shared/image-acceptance.png`，1000 × 650；無答案 metadata，未保存純文字答案。來源 SHA256 與回傳結果詳見 `image-stdio-20260929.json`。
- 第一輪沙箱內 STDIO 子程序建立在 Windows pipe CreateFile 發生 WinError 5；以非管理員正常使用者執行完整隔離驗證後通過。未因此修改系统權限、TEMP/TMP 或安全邊界。

## 重連與用戶端

已停止並啟動既有非提升權限 MCP-Local 排程一次。旧 Tunnel PID 50596，重連後 PID 50120；healthz=200、readyz=200、排程 Running。設定檔 SHA256、Credential 長度／修改時間及排程定義均保持一致；未讀出金鑰、變更共享範圍、Tunnel 配置、Credential 或權限。詳見 `image-reconnect-20260929.json`。

透過本對話既有 MCP connector 成功呼叫 `server_diagnostics`，實際收到 `2026.09.29.2`／契約 6，registered_tools 含 read_image，實際 registry 與契約 digest 相同、consistent=true。詳見 `image-client-20260929.json`。這僅證明既有連線已到新版服務。

**ChatGPT 原生視覺端到端：UNRESOLVED。** 本對話可呼叫工具清單尚無 read_image，模型未透過既有 connector 收到驗收圖。因此未宣稱模型已識別唯一文字、形狀或顏色。需要用戶端刷新工具後，再呼叫 read_image 取得原生圖片並由模型描述其內容；若仍沒有 image content，繼續保留 UNRESOLVED。

用戶端刷新後可直接提供：

```text
請呼叫 read_image，root_id="root_d1db27716b44"，path="MCP-Local/shared/image-acceptance.png"。
請只根據收到的原生圖片，回報圖片上的完整文字，以及由左至右的圖形與顏色。
若工具不存在或沒有收到 type:image，請回報 UNRESOLVED。
```

本次未透過 OCR、讀生成腳本或本機看圖來替代 connector 視覺驗收。尚待完成項目只有用戶端工具刷新與原生看圖驗收；目前工具集合沒有可呼叫的新 read_image，也沒有可用的刷新連線操作。
