using System.Collections.ObjectModel;
using System.IO;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Data;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Media.Media3D;
using Microsoft.Win32;

namespace McpLocal.Desktop;

internal sealed class SecurityRule
{
    public string id { get; set; } = "custom_rule";
    public string kind { get; set; } = "literal";
    public string pattern { get; set; } = "synthetic_marker";
    public bool enabled { get; set; } = true;
    public string risk { get; set; } = "high";
}

internal sealed class SecurityCenterView : UserControl
{
    private readonly BackendClient backend;
    internal readonly CheckBox Enabled = new() { Content = "啟用安全政策" };
    internal readonly CheckBox Host = new() { Content = "主機命令需要本機限時授權" };
    internal readonly CheckBox Detector = new() { Content = "啟用敏感資訊偵測" };
    internal readonly CheckBox Guard = new() { Content = "檢查實際送出的 MCP 回傳" };
    internal readonly ComboBox Strategy = new() { ItemsSource = new[] { "遮罩敏感文字", "阻擋整筆回傳" }, SelectedIndex = 0 };
    internal readonly ComboBox Expiry = new() { ItemsSource = new[] { "終止既有主機 Session", "保留至原期限；禁止互動輸入" }, SelectedIndex = 0 };
    internal readonly TextBox Maximum = new() { Text = "15", MaxLength = 2 };
    internal readonly TextBox Minutes = new() { Text = "5", MaxLength = 2 };
    internal readonly TextBlock Feedback = DesignSystem.Text("");
    internal readonly TextBlock Status = DesignSystem.Text("");
    internal readonly TextBlock Draft = DesignSystem.Text("尚未載入");
    internal readonly ObservableCollection<SecurityRule> Rules = [];
    internal readonly Dictionary<string, Button> Actions = [];
    private readonly TextBlock authorization = DesignSystem.Text(""), channel = DesignSystem.Text(""), guardState = DesignSystem.Text("");
    private readonly TextBlock detectorState = DesignSystem.Text("");
    private readonly DataGrid rules = new() { AutoGenerateColumns = false, CanUserAddRows = false, CanUserDeleteRows = false, MinHeight = 160, MaxHeight = 300, SelectionMode = DataGridSelectionMode.Single };
    private readonly TextBox sessions = Output(130), preview = Output(180), detail = Output(190), diagnostics = Output(160);
    private readonly ComboBox sample = new() { ItemsSource = new[] { "合成憑證", "無法檢查的二進位", "一般開發文字" }, SelectedIndex = 0 };
    internal readonly ComboBox Risk = new() { ItemsSource = new[] { "全部風險", "high", "medium", "low" }, SelectedIndex = 0 };
    internal readonly ComboBox Kind = new() { ItemsSource = new[] { "全部事件", "policy_saved", "authorization_granted", "authorization_revoked", "authorization_expired", "channel_failed", "channel_recovered", "response_masked", "response_blocked", "scan_failed", "command_denied", "session_stopped", "session_stop_failed", "synthetic_test", "audit_cleared", "events_lost" }, SelectedIndex = 0 };
    private ComboBox risk => Risk;
    private ComboBox kind => Kind;
    internal readonly ListBox Events = new() { Height = 160 };
    private ListBox events => Events;
    internal readonly TextBox ClearConfirmation = new() { MaxLength = 16 };
    private TextBox clearConfirmation => ClearConfirmation;
    internal Func<string, Task>? ExportSink { get; set; }
    internal JsonElement CurrentState => state;
    internal string PreviewText => preview.Text;
    internal string DetailText => detail.Text;
    private readonly TextBlock eventCount = DesignSystem.Text("");
    private JsonElement state, eventSnapshot;
    private string savedRevision = "", auditRevision = "";
    private bool hydrating, dirty, refreshing, operating;
    private long generation;
    private DateTimeOffset lastRefresh;
    private int offset;
    internal Task LastOperation { get; private set; } = Task.CompletedTask;
    internal bool Dirty => dirty;

    internal SecurityCenterView(BackendClient backend)
    {
        this.backend = backend;
        SetResourceReference(ForegroundProperty, "TextPrimary");
        var panel = new StackPanel();
        var page = new ScrollViewer { Content = panel, VerticalScrollBarVisibility = ScrollBarVisibility.Auto, HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled, Focusable = false };
        Content = page;
        page.PreviewMouseWheel += (_, e) =>
        {
            DependencyObject? current = e.OriginalSource as DependencyObject;
            bool nested = false;
            while (current is not null && !ReferenceEquals(current, page))
            {
                if (current is ScrollViewer inner)
                {
                    nested = true;
                    if (e.Delta < 0 && inner.VerticalOffset < inner.ScrollableHeight - 0.01 ||
                        e.Delta > 0 && inner.VerticalOffset > 0.01) return;
                }
                current = current is Visual or Visual3D ? VisualTreeHelper.GetParent(current) : null;
            }
            if (nested && ReferenceEquals(current, page))
            {
                page.ScrollToVerticalOffset(page.VerticalOffset - e.Delta / 3.0);
                e.Handled = true;
            }
        };
        panel.Children.Add(DesignSystem.Text("安全中心", "PageTitle"));
        Status.Margin = new Thickness(0, 12, 0, 4); panel.Children.Add(Status);
        panel.Children.Add(Draft); panel.Children.Add(Feedback);
        var policy = DesignSystem.Card(panel, "安全政策");
        policy.Children.Add(Enabled);
        Hint(policy, "首次升級預設關閉。安全政策獨立儲存；儲存後須收到 MCP 執行層確認才顯示生效。");
        var policyBar = new WrapPanel(); policy.Children.Add(policyBar);
        Action(policyBar, "save", "儲存安全政策", Save);
        Action(policyBar, "defaults", "還原預設草稿", async () => { var value = await backend.Send("security_defaults"); Hydrate(value.GetProperty("policy")); Changed(); Feedback.Text = "預設值已載入草稿；尚未儲存。"; });
        Action(policyBar, "reload", "重新載入已儲存設定", async () => { await Refresh(true); Feedback.Text = "已重新載入已儲存設定。"; });

        var host = DesignSystem.Card(panel, "主機限時授權");
        host.Children.Add(Host);
        DesignSystem.Field(host, "每次授權上限（分鐘，1–60）", Maximum);
        DesignSystem.Field(host, "授權到期／撤銷時的 Session 處置", Expiry);
        Hint(host, "通訊失效時終止既有主機 Session；無法確認停止會顯示異常。此功能不是同 Windows 帳戶的憑證沙箱。");
        host.Children.Add(authorization);
        DesignSystem.Field(host, "本次授權時間（分鐘）", Minutes);
        var hostBar = new WrapPanel(); host.Children.Add(hostBar);
        Action(hostBar, "grant", "本機授權", async () => { int minutes = ParseMinutes(Minutes.Text); Apply(await backend.Send("security_grant", new { minutes }), false); Feedback.Text = "授權已簽發；請查看執行層確認狀態。"; });
        Action(hostBar, "revoke", "立即撤銷", async () => { Apply(await backend.Send("security_revoke"), false); Feedback.Text = "撤銷已送出；Session 停止結果以即時回報為準。"; });
        host.Children.Add(sessions);

        var local = DesignSystem.Card(panel, "本機授權通訊"); local.Children.Add(channel);
        var localBar = new WrapPanel(); local.Children.Add(localBar);
        Action(localBar, "diagnose", "診斷", async () => { await Refresh(); diagnostics.Text = Pretty(state.GetProperty("channel")) + "\n" + Pretty(state.GetProperty("clients")); });
        Action(localBar, "recover", "安全恢復", async () => { Apply(await backend.Send("security_recover"), false); Feedback.Text = "已重新驗證；授權保持撤銷，等待執行層確認。"; });
        local.Children.Add(diagnostics);

        var detection = DesignSystem.Card(panel, "敏感資訊規則"); detection.Children.Add(Detector); detection.Children.Add(detectorState);
        rules.ItemsSource = Rules;
        rules.Columns.Add(new DataGridCheckBoxColumn { Header = "啟用", Binding = new Binding("enabled") });
        rules.Columns.Add(new DataGridTextColumn { Header = "識別碼", Binding = new Binding("id"), Width = 130 });
        rules.Columns.Add(new DataGridComboBoxColumn { Header = "類型", ItemsSource = new[] { "builtin", "literal" }, SelectedItemBinding = new Binding("kind"), Width = 90 });
        rules.Columns.Add(new DataGridTextColumn { Header = "規則", Binding = new Binding("pattern"), Width = new DataGridLength(1, DataGridLengthUnitType.Star) });
        rules.Columns.Add(new DataGridComboBoxColumn { Header = "風險", ItemsSource = new[] { "high", "medium", "low" }, SelectedItemBinding = new Binding("risk"), Width = 90 });
        rules.CellEditEnding += (_, _) => Dispatcher.BeginInvoke(Changed);
        DesignSystem.Name(rules, "敏感資訊規則清單"); detection.Children.Add(rules);
        Hint(detection, "builtin 使用 private_key／api_key／bearer／password／email；literal 為固定文字，不執行自訂正規表示式。請勿在規則中輸入正式密鑰。");
        var rulesBar = new WrapPanel(); detection.Children.Add(rulesBar);
        Action(rulesBar, "add_rule", "新增固定文字規則", () => { if (Rules.Count >= 32) throw new InvalidOperationException("最多 32 筆規則。"); Rules.Add(new() { id = "custom_" + Guid.NewGuid().ToString("N")[..8] }); Changed(); return Task.CompletedTask; });
        Action(rulesBar, "remove_rule", "移除所選規則", () => { if (rules.SelectedItem is SecurityRule rule) { Rules.Remove(rule); Changed(); } return Task.CompletedTask; });
        Action(rulesBar, "detect", "合成偵測測試", async () => { preview.Text = Pretty(await backend.Send("security_detect", new { policy = Form() })); Feedback.Text = "已用目前草稿測試合成資料。"; });

        var response = DesignSystem.Card(panel, "工具回傳防護"); response.Children.Add(Guard);
        DesignSystem.Field(response, "偵測到敏感內容時", Strategy);
        Hint(response, "檢查實際送出的文字、結構化結果及錯誤。二進位、超限或不完整資料阻擋；執行中 Session 輸出暫存至結束後整段檢查。此功能不涵蓋命令自行連網。");
        response.Children.Add(guardState);
        DesignSystem.Field(response, "預覽資料", sample);
        var previewBar = new WrapPanel(); response.Children.Add(previewBar);
        Action(previewBar, "preview", "預覽處理結果", async () => { preview.Text = Pretty(await backend.Send("security_preview", new { policy = Form(), sample = new[] { "credentials", "binary", "ordinary" }[sample.SelectedIndex] })); Feedback.Text = "合成預覽完成；草稿尚未因此儲存。"; });
        response.Children.Add(preview);

        var audit = DesignSystem.Card(panel, "安全事件");
        Hint(audit, "只保存分類、時間與計數，使用 SHA-256 事件鏈檢查完整性。原始請求監控獨立保留，可能含敏感內容；請審慎檢視及匯出。");
        DesignSystem.Field(audit, "事件類型", kind); DesignSystem.Field(audit, "風險", risk);
        var auditBar = new WrapPanel(); audit.Children.Add(auditBar);
        Action(auditBar, "events", "查詢", async () => { offset = 0; await LoadEvents(); });
        Action(auditBar, "previous", "上一頁", async () => { offset = Math.Max(0, offset - 100); await LoadEvents(); });
        Action(auditBar, "next", "下一頁", async () => { if (eventSnapshot.ValueKind == JsonValueKind.Object && offset + 100 < eventSnapshot.GetProperty("total").GetInt32()) offset += 100; await LoadEvents(); });
        Action(auditBar, "export", "匯出已驗證事件", Export);
        audit.Children.Add(eventCount); audit.Children.Add(events); audit.Children.Add(detail);
        events.SelectionChanged += async (_, _) =>
        {
            if (events.SelectedItem is AuditRow selected)
                try { detail.Text = Pretty(await backend.Send("security_detail", new { id = selected.Id })); }
                catch (Exception error) { Feedback.Text = error.Message; }
        };
        DesignSystem.Field(audit, "清除確認（輸入「清除安全事件」）", clearConfirmation);
        var clearBar = new WrapPanel(); audit.Children.Add(clearBar);
        Action(clearBar, "clear", "受控清除", async () => { if (auditRevision.Length == 0) throw new InvalidOperationException("請先查詢事件。"); ApplyEvents(await backend.Send("security_clear", new { expected_revision = auditRevision, confirmation = clearConfirmation.Text })); clearConfirmation.Clear(); Feedback.Text = "已清除所確認的安全事件，並留下清除邊界事件。"; });

        foreach (var check in new[] { Enabled, Host, Detector, Guard }) { check.Checked += (_, _) => Changed(); check.Unchecked += (_, _) => Changed(); }
        Maximum.TextChanged += (_, _) => Changed();
        Strategy.SelectionChanged += (_, _) => Changed(); Expiry.SelectionChanged += (_, _) => Changed();
        IsVisibleChanged += (_, _) => { if (IsVisible) _ = Refresh(); };
    }

    private static TextBox Output(double height) => new() { IsReadOnly = true, AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, VerticalScrollBarVisibility = ScrollBarVisibility.Auto, MinHeight = 60, MaxHeight = height, Margin = new Thickness(0, 8, 0, 8) };
    private static void Hint(Panel panel, string text) { var hint = DesignSystem.Text(text); DesignSystem.Secondary(hint); hint.Margin = new Thickness(0, 8, 0, 12); panel.Children.Add(hint); }
    private static string Pretty(JsonElement value) => JsonSerializer.Serialize(value, JsonData.PrettyOptions);
    private static int ParseMinutes(string value) => int.TryParse(value, out int number) && number is >= 1 and <= 60 ? number : throw new InvalidOperationException("分鐘數必須為 1 至 60。");
    private void Action(Panel panel, string name, string label, Func<Task> action)
    {
        var button = Ui.Button(label); panel.Children.Add(button); Actions[name] = button;
        button.Click += (_, _) => LastOperation = Operate(action);
    }
    private async Task Operate(Func<Task> action)
    {
        if (operating || backend.OperationPending) { Feedback.Text = "先前操作尚未確認，未重送。"; return; }
        operating = true; UpdateButtons(); Feedback.Text = "操作中…";
        try { await action(); }
        catch (Exception error) { Feedback.Text = error.Message; }
        finally { operating = false; UpdateButtons(); }
    }
    private void UpdateButtons() { foreach (var button in Actions.Values) button.IsEnabled = !operating && !backend.OperationPending; }
    private void Changed() { if (hydrating) return; dirty = true; generation++; Draft.Text = "設定草稿有未儲存變更"; }
    internal object Form()
    {
        rules.CommitEdit(DataGridEditingUnit.Cell, true); rules.CommitEdit(DataGridEditingUnit.Row, true);
        return new { version = 1, enabled = Enabled.IsChecked == true, host_authorization = Host.IsChecked == true,
            detector_enabled = Detector.IsChecked == true, response_guard = Guard.IsChecked == true,
            strategy = Strategy.SelectedIndex == 0 ? "mask" : "block", session_expiry = Expiry.SelectedIndex == 0 ? "terminate" : "finish",
            max_lease_minutes = ParseMinutes(Maximum.Text), rules = Rules.ToArray() };
    }
    private void Hydrate(JsonElement policy)
    {
        hydrating = true;
        try
        {
            Enabled.IsChecked = policy.GetProperty("enabled").GetBoolean(); Host.IsChecked = policy.GetProperty("host_authorization").GetBoolean();
            Detector.IsChecked = policy.GetProperty("detector_enabled").GetBoolean(); Guard.IsChecked = policy.GetProperty("response_guard").GetBoolean();
            Maximum.Text = policy.GetProperty("max_lease_minutes").GetInt32().ToString();
            Strategy.SelectedIndex = policy.GetProperty("strategy").GetString() == "mask" ? 0 : 1;
            Expiry.SelectedIndex = policy.GetProperty("session_expiry").GetString() == "terminate" ? 0 : 1;
            Rules.Clear(); foreach (var row in policy.GetProperty("rules").EnumerateArray()) Rules.Add(JsonSerializer.Deserialize<SecurityRule>(row.GetRawText())!);
        }
        finally { hydrating = false; }
    }
    private async Task Save()
    {
        var form = Form(); long submitted = generation;
        var value = await backend.Send("security_save", new { policy = form, expected_revision = savedRevision });
        if (submitted == generation) dirty = false;
        Apply(value, false); Feedback.Text = "安全政策已儲存；實際生效狀態請查看執行層確認。";
    }
    internal async Task Refresh(bool discardDraft = false)
    {
        if (refreshing) return;
        refreshing = true;
        long requested = generation;
        try { Apply(await backend.Send("security_state"), discardDraft && requested == generation); lastRefresh = DateTimeOffset.UtcNow; }
        catch (Exception error) { Status.Text = "狀態未確認：" + error.Message + "；最後更新 " + lastRefresh.ToLocalTime().ToString("HH:mm:ss"); }
        finally { refreshing = false; UpdateButtons(); }
    }
    internal async Task Poll() { if (IsVisible && DateTimeOffset.UtcNow - lastRefresh > TimeSpan.FromSeconds(1)) await Refresh(); }
    private void Apply(JsonElement value, bool discardDraft)
    {
        state = value.Clone(); savedRevision = value.GetProperty("saved_revision").GetString()!;
        if (!dirty || discardDraft) { Hydrate(value.GetProperty("policy")); dirty = false; }
        Draft.Text = dirty ? "設定草稿有未儲存變更" : savedRevision.Length == 0 ? "升級預設：關閉；尚未建立獨立政策檔" : "設定草稿與已儲存政策一致";
        string effective = value.GetProperty("effective").GetString()!;
        bool enabled = value.GetProperty("policy").GetProperty("enabled").GetBoolean();
        var savedPolicy = value.GetProperty("policy");
        Status.Text = $"已儲存：{(enabled ? "啟用" : "停用")} · 實際狀態：" + (effective switch { "confirmed" => "執行層已確認", "no_client" => "尚無受管理 MCP 用戶端", "error" => "異常", _ => "尚未確認" });
        if (value.GetProperty("error").ValueKind == JsonValueKind.String) Status.Text += "\n" + value.GetProperty("error").GetString();
        var auth = value.GetProperty("authorization");
        bool ack = value.GetProperty("clients").EnumerateArray().Any(row => row.GetProperty("state").GetString() == "ready" && row.GetProperty("mode").GetString() == "host_control" && row.GetProperty("lease_generation").GetInt32() == auth.GetProperty("generation").GetInt32());
        authorization.Text = $"已儲存保護：{(savedPolicy.GetProperty("host_authorization").GetBoolean() ? "啟用" : "停用")} · 上限 {savedPolicy.GetProperty("max_lease_minutes").GetInt32()} 分鐘\n剩餘 {auth.GetProperty("remaining_seconds").GetInt32()} 秒 · {auth.GetProperty("reason").GetString()} · {(ack ? "執行層已確認" : "執行層尚未確認")}";
        channel.Text = "本機簽章共享記憶體 · 有效心跳 3 秒 · " + effective;
        sessions.Text = string.Join("\n", value.GetProperty("clients").EnumerateArray().Select(row => $"{row.GetProperty("mode").GetString()} · {row.GetProperty("state").GetString()}\n" + Pretty(row.GetProperty("sessions"))));
        detectorState.Text = $"已儲存：{(savedPolicy.GetProperty("detector_enabled").GetBoolean() ? "啟用" : "停用")} · 啟用規則 {savedPolicy.GetProperty("rules").EnumerateArray().Count(row => row.GetProperty("enabled").GetBoolean())} 筆";
        guardState.Text = $"已儲存：{(savedPolicy.GetProperty("response_guard").GetBoolean() ? "啟用" : "停用")} · {(savedPolicy.GetProperty("strategy").GetString() == "mask" ? "遮罩" : "阻擋")} · {(enabled ? "依頁首執行層狀態確認" : "總政策關閉，防護未生效")}\n實際攔截計數：" + string.Join(" · ", value.GetProperty("clients").EnumerateArray().Select(row => row.GetProperty("counters").GetRawText()));
    }
    private async Task LoadEvents() => ApplyEvents(await backend.Send("security_events", new { kind = kind.SelectedIndex == 0 ? "" : (string)kind.SelectedItem, risk = risk.SelectedIndex == 0 ? "" : (string)risk.SelectedItem, offset }));
    private void ApplyEvents(JsonElement value)
    {
        eventSnapshot = value.Clone(); auditRevision = value.GetProperty("revision").GetString()!;
        events.ItemsSource = value.GetProperty("rows").EnumerateArray().Select(row => { var e = row.GetProperty("event"); return new AuditRow(e.GetProperty("id").GetString()!, $"{e.GetProperty("time").GetString()} · {e.GetProperty("kind").GetString()} · {e.GetProperty("risk").GetString()} · {e.GetProperty("count").GetInt32()}"); }).ToArray();
        eventCount.Text = $"符合 {value.GetProperty("total").GetInt32()} 筆 · 起點 {value.GetProperty("offset").GetInt32()} · 事件鏈已驗證";
    }
    private async Task Export()
    {
        var value = await backend.Send("security_export");
        if (backend.IsFixture && ExportSink is not null) { await ExportSink(Pretty(value)); Feedback.Text = "已匯出隔離事件。"; return; }
        var dialog = new SaveFileDialog { FileName = "mcp-security-events.json", Filter = "JSON|*.json", OverwritePrompt = true };
        if (dialog.ShowDialog(Window.GetWindow(this)) != true) { Feedback.Text = "已取消匯出。"; return; }
        await File.WriteAllTextAsync(dialog.FileName, Pretty(value)); Feedback.Text = "已匯出經驗證的安全事件。";
    }
    private sealed record AuditRow(string Id, string Text) { public override string ToString() => Text; }
}
