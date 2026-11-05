using System.IO;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;

namespace McpLocal.Desktop;

internal static class DesktopChecks
{
    internal static async Task PrepareResize(DesktopWindow window)
    {
        await window.Ready;
        window.Tabs.SelectedIndex = 4; window.Monitor.Refresh(); window.Monitor.Follow.IsChecked = false;
        window.Monitor.Select(window.Monitor.Rows.First(r => r.Id == "199"), true);
        for (int i = 0; i < 100 && window.Monitor.Selected?.Record.DetailsLoaded != true; i++) await Task.Delay(50);
        await window.Monitor.Response.Ready;
        window.Monitor.Response.SetWrap(true);
        if (window.Monitor.Response.ContentText.Length < 1048000) throw new InvalidOperationException("縮放負載未完整載入。");
    }
    internal static async Task Benchmark(DesktopWindow window, string report)
    {
        await PrepareResize(window); await Task.Delay(800);
        using var measurement = new ResizeMeasurement(window, window.Monitor);
        measurement.Start("INTEGRATED_DESKTOP_PROGRAMMATIC_RESIZE_80_STEPS");
        for (int i = 0; i < 80; i++)
        {
            int distance = i < 40 ? i : 79 - i;
            window.Width = 900 + distance * 8; window.Height = 820 + distance * 4;
            await Task.Delay(16);
        }
        await Task.Delay(350);
        using var sample = JsonDocument.Parse(measurement.Stop());
        JsonData.WriteNew(report, JsonSerializer.Serialize(new { measurement = sample.RootElement,
            backend_retained_bytes = window.Source.CapturedBytes, synthetic_response_chars = 26520000,
            backend_update_interval_ms = 100, updated_records_per_tick = 4, frontend_poll_ms = 250,
            cpu_scope = "WPF process only; Python backend excluded", actual_fps = "NOT_MEASURED" }, JsonData.PrettyOptions));
    }
    private static IEnumerable<T> Children<T>(DependencyObject root) where T : DependencyObject
    {
        if (root is T value) yield return value;
        for (int i = 0; i < VisualTreeHelper.GetChildrenCount(root); i++)
            foreach (var child in Children<T>(VisualTreeHelper.GetChild(root, i))) yield return child;
    }
    internal static async Task Run(DesktopWindow window, BackendClient backend, string fixture, string report)
    {
        if (!backend.IsFixture) throw new InvalidOperationException("整合測試只允許隔離 fixture。");
        var checks = new List<string>();
        void Check(bool ok, string label) { if (!ok) throw new InvalidOperationException(label); checks.Add(label); }
        async Task Until(Func<bool> condition)
        {
            var end = DateTime.UtcNow.AddSeconds(12);
            while (!condition())
            {
                if (DateTime.UtcNow > end) throw new TimeoutException("桌面狀態未完成：" + window.LastState);
                await Task.Delay(60); await window.Poll();
            }
        }
        await window.Ready;
        await Until(() => !window.LastState.GetProperty("busy").GetBoolean());
        Check(window.Tabs.Items.Cast<TabItem>().Select(t => t.Header.ToString()).SequenceEqual(
            new[] { "連線設定", "進階讀取", "外觀", "權限模式", "請求監控", "安全中心" }), "six complete desktop pages");
        Check(!window.LastState.GetRawText().Contains("synthetic-fixture-key"), "saved DPAPI secret is not sent to WPF");
        window.Tunnel.Text = "tunnel_wpf_changed";
        await window.Run("save", includeForm: true);
        await Until(() => !window.LastState.GetProperty("busy").GetBoolean() && !window.LastState.GetProperty("dirty").GetBoolean());
        using (var disk = JsonDocument.Parse(File.ReadAllText(Path.Combine(fixture, "settings.json"))))
            Check(disk.RootElement.GetProperty("tunnel").GetString() == "tunnel_wpf_changed", "WPF editor saves through Python transaction");

        window.Tabs.SelectedIndex = 1; window.UpdateLayout();
        var numeric = Children<TextBox>(window.Tabs).Where(t => t.MaxLength == 40).ToArray();
        Check(numeric.Length == 4, "four advanced numeric editors");
        numeric[1].Text = "42";
        await window.Run("save", includeForm: true);
        await Until(() => !window.LastState.GetProperty("busy").GetBoolean() && !window.LastState.GetProperty("dirty").GetBoolean());
        Check(window.LastState.GetProperty("form").GetProperty("numbers").EnumerateObject().Any(p => p.Value.GetString() == "42"), "advanced setting committed");
        await window.Run("reader_maximums", hydrate: true);
        Check(window.LastState.GetProperty("form").GetProperty("numbers").GetProperty("max_scan_files").GetString() == "100000", "reader maximums preserve backend capacity definitions");
        await window.Run("reader_defaults", hydrate: true);

        var listEditors = Children<ReaderListEditor>(window.Tabs).ToArray();
        Check(listEditors.Length == 3, "three individual-item editors remain available");
        foreach (var editor in listEditors)
        {
            string original = editor.Text;
            var first = System.Text.RegularExpressions.Regex.Match(original, @"[^,;，；\n]+" );
            Check(first.Success && editor.Apply(first.Index, first.Length, "synthetic-name"), "edit one item");
            Check(editor.VisibleEntries.Contains("synthetic-name"), "edited item is represented separately");
            editor.Text = original;
            Check(editor.Apply(null, 0, "synthetic-added") && editor.VisibleEntries[^1] == "synthetic-added", "add one item");
            editor.Remove(editor.Text.LastIndexOf("synthetic-added", StringComparison.Ordinal), "synthetic-added".Length);
            Check(ReaderListEditor.Entries(editor.Text).SequenceEqual(ReaderListEditor.Entries(original)), "remove item preserves all other entries");
            editor.Undo(); Check(editor.VisibleEntries[^1] == "synthetic-added", "undo restores deleted item");
            editor.Redo(); Check(ReaderListEditor.Entries(editor.Text).SequenceEqual(ReaderListEditor.Entries(original)), "redo removes the same item");
            editor.Text = original;
            Check(!editor.Apply(null, 0, "one,two") && editor.Text == original, "separator in a single item is rejected without modifying the draft");
        }
        Check(ReaderListEditor.Entries(".txt，.md；../private?\n.a/b").SequenceEqual(new[] { ".txt", ".md", "../private?", ".a/b" }),
            "list parsing retains invalid paths and wildcards for backend rejection");
        var mixed = listEditors[0]; string beforeMixed = mixed.Text;
        const string rawMixed = " .txt ; .js，  ../private?；\n.a/b, .txt ";
        mixed.Text = rawMixed;
        Check(mixed.Text == rawMixed && mixed.VisibleEntries.SequenceEqual(new[] { ".txt", ".js", "../private?", ".a/b", ".txt" }),
            "item view preserves raw separators, invalid entries and duplicates without silent repair");
        int badStart = rawMixed.IndexOf("  ../private?", StringComparison.Ordinal);
        Check(mixed.Apply(badStart, "  ../private?".Length, "../still-invalid?"), "invalid item remains editable for backend validation");
        Check(mixed.Text == rawMixed.Remove(badStart, "  ../private?".Length).Insert(badStart, "../still-invalid?"),
            "editing replaces only the selected raw entry");
        mixed.Text = beforeMixed;
        var previousSettings = File.ReadAllBytes(Path.Combine(fixture, "settings.json"));
        string validExtensions = listEditors[0].Text;
        window.ConfirmFixture = _ => false;
        listEditors[0].Text = ".txt;../escape";
        await window.Run("save", includeForm: true);
        await Until(() => !window.LastState.GetProperty("busy").GetBoolean());
        Check(File.ReadAllBytes(Path.Combine(fixture, "settings.json")).SequenceEqual(previousSettings) && listEditors[0].Text.Contains("../escape"),
            "invalid reader draft is rejected without changing settings or repairing input");
        listEditors[0].Text = validExtensions;
        await window.Run("edit", includeForm: true);
        window.ConfirmFixture = null;

        string originalId = window.LastState.GetProperty("form").GetProperty("roots")[0].GetProperty("id").GetString()!;
        var replacement = Directory.CreateDirectory(Path.Combine(fixture, "replacement")).FullName;
        await window.Run("folder", new { path = replacement, index = 0 }, hydrate: true);
        var root = window.LastState.GetProperty("form").GetProperty("roots")[0];
        Check(root.GetProperty("id").GetString() == originalId && root.GetProperty("excluded_names")[0].GetString() == "private", "root replacement retains identity and private exclusions");
        var added = Directory.CreateDirectory(Path.Combine(fixture, "additional")).FullName;
        await window.Run("folder", new { path = added }, hydrate: true);
        Check(window.LastState.GetProperty("form").GetProperty("roots").GetArrayLength() == 2, "additional shared root");
        await window.Run("remove_folders", new { indexes = new[] { 1 } }, hydrate: true);
        await window.Run("save", includeForm: true);
        await Until(() => !window.LastState.GetProperty("busy").GetBoolean() && !window.LastState.GetProperty("dirty").GetBoolean());
        Check(window.LastState.GetProperty("form").GetProperty("roots").GetArrayLength() == 1, "remove root and commit");

        window.Tabs.SelectedIndex = 2; window.UpdateLayout();
        var styles = Children<RadioButton>(window.Tabs).ToArray();
        Check(styles.Length == 2, "both appearance choices rendered");
        styles[1].IsChecked = true;
        await window.Run("save", includeForm: true);
        await Until(() => !window.LastState.GetProperty("busy").GetBoolean() && !window.LastState.GetProperty("dirty").GetBoolean());
        Check(window.LastState.GetProperty("form").GetProperty("icon_style").GetString() == "folder_link" && window.Icon is not null, "appearance saved and window icon applied");

        window.Tabs.SelectedIndex = 3; window.UpdateLayout();
        var modes = Children<RadioButton>(window.Tabs).ToArray();
        Check(modes.Length == 3, "three permission modes rendered");
        bool accepted = false; int confirmations = 0;
        window.ConfirmFixture = _ => { confirmations++; return accepted; };
        modes[2].IsChecked = true;
        await window.Run("save", includeForm: true); await window.Poll();
        using (var disk = JsonDocument.Parse(File.ReadAllText(Path.Combine(fixture, "settings.json"))))
            Check(disk.RootElement.GetProperty("access_mode").GetString() == "read_only" && confirmations == 1, "declining control-mode consent leaves disk unchanged");
        accepted = true;
        await window.Run("save", includeForm: true);
        await Until(() => !window.LastState.GetProperty("busy").GetBoolean() && !window.LastState.GetProperty("dirty").GetBoolean());
        Check(window.LastState.GetProperty("form").GetProperty("access_mode").GetString() == "host_control" && confirmations == 2, "explicit mode consent reaches existing transaction");
        modes[0].IsChecked = true; await window.Run("save", includeForm: true);
        await Until(() => !window.LastState.GetProperty("busy").GetBoolean() && !window.LastState.GetProperty("dirty").GetBoolean());
        Check(!window.LastState.TryGetProperty("power_mode", out _) &&
            !window.LastState.TryGetProperty("power_status", out _), "power mode absent from desktop state");

        await window.Run("start");
        await Until(() => window.LastState.GetProperty("state").GetString() == "RUNNING" && window.Source.Snapshot.Records.Count > 0);
        window.Tabs.SelectedIndex = 4; window.UpdateLayout(); window.Monitor.Refresh();
        await Until(() => window.Monitor.Selected?.Record.DetailsLoaded == true);
        await window.Monitor.Response.Ready;
        Check(window.Monitor.Response.ContentText.Contains("合成整合資料"), "WPF reads real private-pipe capture from synthetic Connection Job");
        Check(window.Monitor.ToolFilter.Items.Cast<string>().Contains("read_file"), "tools discovered after initial empty monitor");
        Check(window.Monitor.Selected!.Capture == "部分內容" && !window.Monitor.Selected.Status.Contains("部分內容") &&
            window.Monitor.CaptureState.Text == "部分內容", "execution outcome and capture completeness presented separately");
        window.Monitor.Query.Text = "合成整合"; await window.Monitor.ApplyFilter();
        Check(window.Monitor.Listing.Items.Count == 1, "remote full-content search");
        window.Monitor.Follow.IsChecked = false;
        window.OpenMonitor();
        await Until(() => window.DetachedMonitor?.Query.Text == "合成整合");
        Check(window.DetachedMonitor!.Follow.IsChecked == false, "detached WPF monitor retains filtering and history mode");
        string? copied = null; window.Monitor.CopySink = value => copied = value; window.Monitor.CopyRecord();
        await Until(() => copied is not null);
        using (var exported = JsonDocument.Parse(copied!))
            Check(exported.RootElement.GetProperty("record").GetProperty("response_notes")[0].GetProperty("retained_chars").GetInt32() == 6,
                "copy preserves structured capture notes and export schema");
        await DesignChecks.Run(window, report, Check);
        int loads = window.Monitor.Request.DocumentLoads + window.Monitor.Response.DocumentLoads;
        for (int i = 0; i < 8; i++) { window.Width = i % 2 == 0 ? 660 : 1100; window.Height = i % 2 == 0 ? 510 : 900; await Task.Delay(40); }
        Check(loads == window.Monitor.Request.DocumentLoads + window.Monitor.Response.DocumentLoads, "whole desktop resize does not reload selected JSON");
        await window.Source.Toggle(); await Until(() => !window.Source.Enabled);
        Check(!window.Source.Enabled, "pause via private pipe");
        await window.Source.Toggle(); await Until(() => window.Source.Enabled);
        Check(window.Source.Enabled, "resume via private pipe");
        await window.Run("stop"); await Until(() => !window.LastState.GetProperty("running").GetBoolean());
        Check(!window.Source.Connected, "connection stop revokes monitoring");
        window.Source.Clear(); await Until(() => window.Source.Snapshot.Records.Count == 0);
        Check(window.Monitor.Selected is null, "clear removes selection and payload");
        await SecurityChecks.Run(window, backend, fixture, report, Check);
        await DesktopReliabilityChecks.Run(window, backend, Check);
        JsonData.WriteNew(report, JsonSerializer.Serialize(new { state = "PASS", checks, fixture_only = true,
            real_dpapi_and_job = true, production_credentials = "NOT_READ", scheduler = "MOCKED", power = "MOCKED",
            manual_resize = "NOT_ASSESSED", actual_fps = "NOT_MEASURED" }, JsonData.PrettyOptions));
    }
}
