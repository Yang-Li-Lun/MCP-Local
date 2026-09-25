using System;
using System.Diagnostics;
using System.IO;
using System.Windows.Forms;

internal static class McpLocalLauncher
{
    [STAThread]
    private static int Main()
    {
        string root = AppDomain.CurrentDomain.BaseDirectory;
        string python = Path.Combine(root, ".venv", "Scripts", "pythonw.exe");
        string script = Path.Combine(root, "local_files_gui.py");
        if (!File.Exists(python) || !File.Exists(script))
        {
            MessageBox.Show("找不到 MCP-Local 的 Python 環境或設定介面。", "MCP-Local",
                            MessageBoxButtons.OK, MessageBoxIcon.Error);
            return 1;
        }
        try
        {
            Process.Start(new ProcessStartInfo(python)
            {
                Arguments = "-B \"" + script + "\"",
                WorkingDirectory = root,
                UseShellExecute = false,
                CreateNoWindow = true
            });
            return 0;
        }
        catch (Exception error)
        {
            MessageBox.Show("無法啟動 MCP-Local：" + error.Message, "MCP-Local",
                            MessageBoxButtons.OK, MessageBoxIcon.Error);
            return 1;
        }
    }
}
