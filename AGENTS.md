# MCP-Local Rust 專案規範

## Windows 執行檔交付

- 每次修改程式後，重新編譯 Windows x64 Release，完成對應驗證後，更新專案根目錄的 `mcp-local-rust.exe`。
- 正式 EXE 固定放在專案根目錄，不部署到 Codex 內部目錄。使用者直接從專案根目錄開啟程式。
- 更新時保留使用者設定，完成後不啟動程式。
