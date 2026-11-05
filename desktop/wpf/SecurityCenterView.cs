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
    internal readonly ComboBox Risk = new() { ItemsSource = new[] { new Filter("", "全部風險"), new Filter("high", "高風險"), new Filter("medium", "中風險"), new Filter("low", "低風險") }, SelectedIndex = 0 };
    internal readonly ComboBox Kind = new() { ItemsSource = new[] { "", "policy_saved", "authorization_granted", "authorization_revoked", "authorization_expired", "channel_failed", "channel_recovered", "response_masked", "response_blocked", "scan_failed", "command_denied", "session_stopped", "session_stop_failed", "synthetic_test", "audit_cleared", "events_lost" }.Select(code => new Filter(code, code == "" ? "全部事件" : EventName(code))).ToArray(), SelectedIndex = 0 };
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
    internal readonly TextBlock Recent = DesignSystem.Text("載入中…");
    internal readonly TextBlock EventDetail = DesignSystem.Text("選取事件以查看詳情。");
    internal readonly Expander Advanced = new() { Header = "進階設定", Margin = new Thickness(0, 8, 0, 16) };
    internal readonly Dictionary<string, Button> Sections = [];
    private readonly Dictionary<string, StackPanel> sectionPanels = [];
    private readonly TextBlock eventCount = DesignSystem.Text("");
    private readonly SemaphoreSlim eventQuery = new(1, 1), detailQuery = new(1, 1);
    private DateTimeOffset lastEvents;
    private int savedMaximum;
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
        var navigation = new WrapPanel { Margin = new Thickness(0, 0, 0, 16) }; panel.Children.Add(navigation);
        foreach (var (key, label) in new[] { ("overview", "總覽"), ("settings", "設定"), ("events", "安全事件") })
        {
            var button = Ui.Button(label); button.Margin = new Thickness(0, 0, 8, 8);
            DesignSystem.Name(button, label); navigation.Children.Add(button); Sections[key] = button;
            var section = new StackPanel(); sectionPanels[key] = section;
            button.Click += (_, _) => SelectSection(key);
        }
        panel.Children.Add(Draft); panel.Children.Add(Feedback);
        foreach (var section in sectionPanels.Values) panel.Children.Add(section);
        var overview = sectionPanels["overview"];
        var statusCard = DesignSystem.Card(overview, "安全狀態");
        statusCard.Children.Add(Status); statusCard.Children.Add(guardState);
        var hostCard = DesignSystem.Card(overview, "主機授權");
        hostCard.Children.Add(authorization);
        Hint(hostCard, "主機命令具有目前使用者權限，並非憑證沙箱。");
        var hostBar = new WrapPanel(); hostCard.Children.Add(hostBar);
        foreach (int duration in new[] { 5, 10, 15 })
            Action(hostBar, "grant_" + duration, duration + " 分鐘", async () => await Grant(duration));
        Action(hostBar, "revoke", "立即撤銷", async () => { Apply(await backend.Send("security_revoke"), false); Feedback.Text = "已送出撤銷，請查看執行層確認。"; });
        var recentCard = DesignSystem.Card(overview, "最近安全事件"); recentCard.Children.Add(Recent);
        var recentBar = new WrapPanel(); recentCard.Children.Add(recentBar);
        var showEvents = Ui.Button("查看全部"); recentBar.Children.Add(showEvents); showEvents.Click += (_, _) => SelectSection("events");

        var settings = sectionPanels["settings"];
        var policy = DesignSystem.Card(settings, "安全政策");
        policy.Children.Add(Enabled); policy.Children.Add(Host); policy.Children.Add(Detector); policy.Children.Add(Guard);
        DesignSystem.Field(policy, "偵測到敏感內容時", Strategy);
        Hint(policy, "啟用回傳防護後，無法完整檢查的資料會阻擋；執行中 Session 輸出會暫存至結束。命令自行連網不受此防護。");
        var policyBar = new WrapPanel(); policy.Children.Add(policyBar);
        Action(policyBar, "save", "儲存設定", Save);
        Action(policyBar, "defaults", "還原預設", async () => { var value = await backend.Send("security_defaults"); Hydrate(value.GetProperty("policy")); Changed(); Feedback.Text = "已還原草稿，尚未儲存。"; });
        Action(policyBar, "reload", "重新載入", async () => { await Refresh(true); Feedback.Text = "已重新載入已儲存設定。"; });
        settings.Children.Add(Advanced);
        var advanced = new StackPanel(); Advanced.Content = advanced;
        var host = DesignSystem.Card(advanced, "主機授權設定");
        DesignSystem.Field(host, "每次授權上限（分鐘，1–60）", Maximum);
        DesignSystem.Field(host, "到期或撤銷時的 Session 處置", Expiry);
        Hint(host, "通訊失效時會要求終止主機 Session；停止未確認時會顯示異常。");
        DesignSystem.Field(host, "其他授權時間（分鐘）", Minutes);
        var customBar = new WrapPanel(); host.Children.Add(customBar);
        Action(customBar, "grant", "本機授權", async () => await Grant(ParseMinutes(Minutes.Text)));
        var detection = DesignSystem.Card(advanced, "敏感資訊規則"); detection.Children.Add(detectorState);
        rules.ItemsSource = Rules;
        rules.Columns.Add(new DataGridCheckBoxColumn { Header = "啟用", Binding = new Binding("enabled") });
        rules.Columns.Add(new DataGridTextColumn { Header = "識別碼", Binding = new Binding("id"), Width = 130 });
        rules.Columns.Add(new DataGridComboBoxColumn { Header = "類型", ItemsSource = new[] { "builtin", "literal" }, SelectedItemBinding = new Binding("kind"), Width = 90 });
        rules.Columns.Add(new DataGridTextColumn { Header = "規則", Binding = new Binding("pattern"), Width = new DataGridLength(1, DataGridLengthUnitType.Star) });
        rules.Columns.Add(new DataGridComboBoxColumn { Header = "風險", ItemsSource = new[] { "high", "medium", "low" }, SelectedItemBinding = new Binding("risk"), Width = 90 });
        rules.CellEditEnding += (_, _) => Dispatcher.BeginInvoke(Changed);
        DesignSystem.Name(rules, "敏感資訊規則清單"); detection.Children.Add(rules);
        Hint(detection, "自訂規則僅支援固定文字；請勿輸入正式密鑰。");
        var rulesBar = new WrapPanel(); detection.Children.Add(rulesBar);
        Action(rulesBar, "add_rule", "新增規則", () => { if (Rules.Count >= 32) throw new InvalidOperationException("最多 32 筆規則。"); Rules.Add(new() { id = "custom_" + Guid.NewGuid().ToString("N")[..8] }); Changed(); return Task.CompletedTask; });
        Action(rulesBar, "remove_rule", "移除規則", () => { if (rules.SelectedItem is SecurityRule rule) { Rules.Remove(rule); Changed(); } return Task.CompletedTask; });
        Action(rulesBar, "detect", "測試規則", async () => { preview.Text = Pretty(await backend.Send("security_detect", new { policy = Form() })); Feedback.Text = "合成測試完成，草稿尚未儲存。"; });
        DesignSystem.Field(detection, "測試資料", sample);
        var previewBar = new WrapPanel(); detection.Children.Add(previewBar);
        Action(previewBar, "preview", "測試回傳防護", async () => { preview.Text = Pretty(await backend.Send("security_preview", new { policy = Form(), sample = new[] { "credentials", "binary", "ordinary" }[sample.SelectedIndex] })); Feedback.Text = "合成測試完成，草稿尚未儲存。"; });
        var local = DesignSystem.Card(advanced, "診斷與恢復"); local.Children.Add(channel);
        var localBar = new WrapPanel(); local.Children.Add(localBar);
        Action(localBar, "diagnose", "更新診斷", async () => { await Refresh(requireFresh: true); diagnostics.Text = Pretty(state.GetProperty("channel")) + "\n" + Pretty(state.GetProperty("clients")); });
        Action(localBar, "recover", "安全恢復", async () => { Apply(await backend.Send("security_recover"), false); Feedback.Text = "已重新驗證；授權保持撤銷，請查看執行層確認。"; });
        local.Children.Add(diagnostics); local.Children.Add(sessions);
        local.Children.Add(DesignSystem.Text("合成測試結果")); local.Children.Add(preview);
        var rawEvent = DesignSystem.Card(advanced, "事件診斷"); rawEvent.Children.Add(detail);

        var audit = DesignSystem.Card(sectionPanels["events"], "安全事件");
        Hint(audit, "事件不含敏感原文；原始請求監控可能包含敏感內容，匯出前請檢視。");
        DesignSystem.Field(audit, "事件類型", kind); DesignSystem.Field(audit, "風險", risk);
        var auditBar = new WrapPanel(); audit.Children.Add(auditBar);
        Action(auditBar, "events", "篩選", async () => { offset = 0; await LoadEvents(); });
        Action(auditBar, "previous", "上一頁", async () => { offset = Math.Max(0, offset - 100); await LoadEvents(); });
        Action(auditBar, "next", "下一頁", async () => { if (eventSnapshot.ValueKind == JsonValueKind.Object && offset + 100 < eventSnapshot.GetProperty("total").GetInt32()) offset += 100; await LoadEvents(); });
        Action(auditBar, "export", "匯出事件", Export);
        audit.Children.Add(eventCount); audit.Children.Add(events); audit.Children.Add(EventDetail);
        events.SelectionChanged += async (_, _) =>
        {
            detail.Clear(); EventDetail.Text = "選取事件以查看詳情。";
            if (events.SelectedItem is AuditRow selected)
                try
                {
                    await detailQuery.WaitAsync();
                    try
                    {
                        if (events.SelectedItem is not AuditRow current || current.Id != selected.Id) return;
                        var result = await backend.Send("security_detail", new { id = selected.Id });
                        if (events.SelectedItem is not AuditRow latest || latest.Id != selected.Id) return;
                        detail.Text = Pretty(result);
                        EventDetail.Text = result.GetProperty("row").ValueKind == JsonValueKind.Object ? selected.Text + "\n事件識別碼：" + selected.Id : "事件已不存在，請重新載入。";
                    }
                    finally { detailQuery.Release(); }
                }
                catch (Exception error) { EventDetail.Text = "事件詳情未確認：" + error.Message; }
        };
        var clear = DesignSystem.Card(sectionPanels["events"], "清除事件");
        Hint(clear, "清除前請匯出需要保留的事件，並確認最新紀錄。");
        DesignSystem.Field(clear, "輸入「清除安全事件」", clearConfirmation);
        var clearBar = new WrapPanel(); clear.Children.Add(clearBar);
        Action(clearBar, "clear", "清除事件", async () => { if (auditRevision.Length == 0) throw new InvalidOperationException("請先載入事件。"); ApplyEvents(await backend.Send("security_clear", new { expected_revision = auditRevision, confirmation = clearConfirmation.Text })); clearConfirmation.Clear(); Feedback.Text = "已清除安全事件並保留清除紀錄。"; });
        SelectSection("overview");
        foreach (var check in new[] { Enabled, Host, Detector, Guard }) { check.Checked += (_, _) => Changed(); check.Unchecked += (_, _) => Changed(); }
        Maximum.TextChanged += (_, _) => Changed();
        Strategy.SelectionChanged += (_, _) => Changed(); Expiry.SelectionChanged += (_, _) => Changed();
        IsVisibleChanged += (_, _) => { if (IsVisible) { _ = Refresh(); _ = RefreshRecent(); } };
    }

    internal void SelectSection(string name)
    {
        if (Content is ScrollViewer page) page.ScrollToTop();
        foreach (var (key, panel) in sectionPanels) panel.Visibility = key == name ? Visibility.Visible : Visibility.Collapsed;
        foreach (var (key, button) in Sections)
        {
            if (key == name) button.SetResourceReference(StyleProperty, "AccentButtonStyle");
            else button.ClearValue(StyleProperty);
        }
        if (name == "events" && IsVisible) _ = LoadEventsSafely();
    }
    private async Task Grant(int minutes)
    {
        if (savedMaximum == 0 || minutes > savedMaximum) throw new InvalidOperationException("授權時間超過已儲存上限，或上限尚未確認。");
        Apply(await backend.Send("security_grant", new { minutes }), false);
        Feedback.Text = "已送出授權，請查看執行層確認。";
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
        finally { operating = false; UpdateButtons(); _ = RefreshRecent(); }
    }
    private void UpdateButtons()
    {
        foreach (var button in Actions.Values) button.IsEnabled = !operating && !backend.OperationPending;
        foreach (int duration in new[] { 5, 10, 15 })
            Actions["grant_" + duration].IsEnabled = !operating && !backend.OperationPending && savedMaximum >= duration;
        Actions["grant"].IsEnabled = !operating && !backend.OperationPending && savedMaximum > 0;
    }
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
    internal async Task Refresh(bool discardDraft = false, bool requireFresh = false)
    {
        if (refreshing)
        {
            if (discardDraft || requireFresh) throw new InvalidOperationException("狀態更新中，請稍後再試。");
            return;
        }
        refreshing = true;
        long requested = generation;
        try { Apply(await backend.Send("security_state"), discardDraft && requested == generation); lastRefresh = DateTimeOffset.UtcNow; }
        catch (Exception error)
        {
            savedMaximum = 0; Status.Text = "狀態未確認：" + error.Message + "；最後更新 " + lastRefresh.ToLocalTime().ToString("HH:mm:ss");
            guardState.Text = "回傳防護狀態未確認"; authorization.Text = "主機授權狀態未確認";
            if (discardDraft || requireFresh) throw;
        }
        finally { refreshing = false; UpdateButtons(); }
    }
    internal async Task Poll()
    {
        if (!IsVisible) return;
        if (DateTimeOffset.UtcNow - lastRefresh > TimeSpan.FromSeconds(1)) await Refresh();
        if (DateTimeOffset.UtcNow - lastEvents > TimeSpan.FromSeconds(10)) await RefreshRecent();
    }
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
        bool ack = effective == "confirmed" && value.GetProperty("clients").EnumerateArray().Any(row => row.GetProperty("state").GetString() == "ready" && row.GetProperty("mode").GetString() == "host_control" && row.GetProperty("lease_generation").GetInt32() == auth.GetProperty("generation").GetInt32());
        savedMaximum = savedPolicy.GetProperty("max_lease_minutes").GetInt32();
        bool authorized = auth.GetProperty("authorized").GetBoolean();
        authorization.Text = $"已儲存保護：{(savedPolicy.GetProperty("host_authorization").GetBoolean() ? "啟用" : "停用")} · 上限 {savedMaximum} 分鐘\n" +
            (authorized ? $"授權剩餘 {auth.GetProperty("remaining_seconds").GetInt32()} 秒" : "目前未授權") + $" · {(ack ? "執行層已確認" : "執行層尚未確認")}";
        channel.Text = "通訊狀態：" + (effective == "confirmed" ? "已確認" : "未確認");
        sessions.Text = string.Join("\n", value.GetProperty("clients").EnumerateArray().Select(row => $"{row.GetProperty("mode").GetString()} · {row.GetProperty("state").GetString()}\n" + Pretty(row.GetProperty("sessions"))));
        detectorState.Text = $"已儲存：{(savedPolicy.GetProperty("detector_enabled").GetBoolean() ? "啟用" : "停用")} · 啟用規則 {savedPolicy.GetProperty("rules").EnumerateArray().Count(row => row.GetProperty("enabled").GetBoolean())} 筆";
        guardState.Text = $"回傳防護：已儲存{(savedPolicy.GetProperty("response_guard").GetBoolean() ? "啟用" : "停用")} · {(savedPolicy.GetProperty("strategy").GetString() == "mask" ? "遮罩" : "阻擋")} · " +
            (effective == "confirmed" ? "執行層已確認" : "執行層尚未確認") + (enabled ? "" : " · 安全政策已停用");
        UpdateButtons();
    }
    private async Task RefreshRecent()
    {
        try
        {
            await eventQuery.WaitAsync();
            try
            {
                var value = await backend.Send("security_events", new { kind = "", risk = "", offset = 0 });
                var rows = value.GetProperty("rows").EnumerateArray().Take(5).Select(row => FormatEvent(row.GetProperty("event"))).ToArray();
                Recent.Text = rows.Length == 0 ? "目前沒有安全事件。" : string.Join("\n", rows);
                lastEvents = DateTimeOffset.UtcNow;
            }
            finally { eventQuery.Release(); }
        }
        catch (Exception error) { Recent.Text = "安全事件未確認：" + error.Message; lastEvents = DateTimeOffset.UtcNow; }
    }
    private async Task LoadEventsSafely()
    {
        try { await LoadEvents(); }
        catch (Exception error) { eventCount.Text = "安全事件未確認：" + error.Message; auditRevision = ""; }
    }
    private static string EventName(string? kind) => kind switch
        {
            "policy_saved" => "設定已儲存", "authorization_granted" => "已送出授權", "authorization_revoked" => "授權已撤銷",
            "authorization_expired" => "授權已到期", "channel_failed" => "通訊異常", "channel_recovered" => "已恢復通訊",
            "response_masked" => "回傳已遮罩", "response_blocked" => "回傳已阻擋", "scan_failed" => "檢查失敗",
            "command_denied" => "命令已拒絕", "session_stopped" => "Session 已停止", "session_stop_failed" => "Session 停止未確認",
            "synthetic_test" => "合成測試", "audit_cleared" => "事件已清除", "events_lost" => "事件遺失", _ => "其他事件"
        };
    private static string FormatEvent(JsonElement e)
    {
        string risk = e.GetProperty("risk").GetString() switch { "high" => "高風險", "medium" => "中風險", "low" => "低風險", _ => "風險未確認" };
        string? rawTime = e.GetProperty("time").GetString();
        string time = DateTimeOffset.TryParse(rawTime, out var timestamp) ? timestamp.ToLocalTime().ToString("yyyy/MM/dd HH:mm:ss") : "時間未確認";
        return $"{time} · {EventName(e.GetProperty("kind").GetString())} · {risk} · {e.GetProperty("count").GetInt32()}";
    }
    private async Task LoadEvents()
    {
        await eventQuery.WaitAsync();
        try { ApplyEvents(await backend.Send("security_events", new { kind = ((Filter)kind.SelectedItem).Code, risk = ((Filter)risk.SelectedItem).Code, offset })); }
        catch { auditRevision = ""; throw; }
        finally { eventQuery.Release(); }
    }
    private void ApplyEvents(JsonElement value)
    {
        eventSnapshot = value.Clone(); auditRevision = value.GetProperty("revision").GetString()!;
        events.ItemsSource = value.GetProperty("rows").EnumerateArray().Select(row => { var e = row.GetProperty("event"); return new AuditRow(e.GetProperty("id").GetString()!, FormatEvent(e)); }).ToArray();
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
    private sealed record Filter(string Code, string Label) { public override string ToString() => Label; }
    private sealed record AuditRow(string Id, string Text) { public override string ToString() => Text; }
}
