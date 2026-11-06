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
    internal readonly ComboBox Expiry = new() { ItemsSource = new[] { "終止既有主機工作", "保留至原期限；禁止互動輸入" }, SelectedIndex = 0 };
    internal readonly TextBox Maximum = new() { Text = "15", MaxLength = 2 };
    internal readonly TextBox Minutes = new() { Text = "5", MaxLength = 2 };
    internal readonly ComboBox Duration = new() { SelectedIndex = 0 };
    internal readonly TextBlock GrantReason = DesignSystem.Text("");
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
    internal readonly Expander Diagnostics = new() { Header = "診斷與恢復" };
    internal readonly Expander EventDiagnostics = new() { Header = "原始診斷資訊" };
    internal readonly Expander ClearEvents = new() { Header = "清除事件", Margin = new Thickness(0, 16, 0, 0) };
    internal readonly Dictionary<string, Button> Sections = [];
    private readonly Dictionary<string, StackPanel> sectionPanels = [];
    private readonly TextBlock eventCount = DesignSystem.Text("");
    private readonly TextBlock dependency = DesignSystem.Text("");
    private readonly StackPanel grantOptions = new(), customDuration = new();
    private readonly StackPanel hostOptions = new(), strategyOptions = new();
    private readonly SemaphoreSlim eventQuery = new(1, 1), detailQuery = new(1, 1);
    private DateTimeOffset lastEvents;
    private int savedMaximum;
    private JsonElement state, eventSnapshot;
    private string savedRevision = "", draftRevision = "", auditRevision = "";
    private bool hydrating, dirty, refreshing, operating, stateKnown, loadingEvents;
    private long stateGeneration, filterGeneration;
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
        var statusBar = new WrapPanel(); statusCard.Children.Add(statusBar);
        Action(statusBar, "open_diagnostics", "查看診斷", () => { OpenDiagnostics(); return Task.CompletedTask; });
        var hostCard = DesignSystem.Card(overview, "主機授權");
        hostCard.Children.Add(authorization);
        authorization.FontSize = 18; authorization.FontWeight = FontWeights.SemiBold;
        hostCard.Children.Add(GrantReason);
        Hint(hostCard, "主機命令具有目前使用者權限，並非憑證沙箱。");
        hostCard.Children.Add(grantOptions);
        foreach (int duration in new[] { 5, 10, 15 }) Duration.Items.Add(new ComboBoxItem { Content = duration + " 分鐘", Tag = duration });
        Duration.Items.Add(new ComboBoxItem { Content = "自訂時間", Tag = 0 }); Duration.SelectedIndex = 0;
        DesignSystem.Field(grantOptions, "授權時間", Duration);
        grantOptions.Children.Add(customDuration); DesignSystem.Field(customDuration, "分鐘（1–60，依已儲存上限）", Minutes);
        var hostBar = new WrapPanel(); hostCard.Children.Add(hostBar);
        Action(hostBar, "grant", "本機授權", async () => await Grant(SelectedMinutes()));
        Action(hostBar, "revoke", "立即撤銷", async () => { Apply(await backend.Send("security_revoke"), false); Feedback.Text = "已送出撤銷，請查看執行層確認。"; });
        var recentCard = DesignSystem.Card(overview, "最近安全事件"); recentCard.Children.Add(Recent);
        var recentBar = new WrapPanel(); recentCard.Children.Add(recentBar);
        var showEvents = Ui.Button("查看全部"); recentBar.Children.Add(showEvents); showEvents.Click += (_, _) => SelectSection("events");

        var settings = sectionPanels["settings"];
        var policy = DesignSystem.Card(settings, "安全政策");
        policy.Children.Add(Enabled); policy.Children.Add(Host); policy.Children.Add(Detector); policy.Children.Add(Guard);
        policy.Children.Add(strategyOptions); DesignSystem.Field(strategyOptions, "偵測到敏感內容時", Strategy);
        policy.Children.Add(dependency);
        Hint(policy, "回傳防護會阻擋無法完整檢查的資料；主機工作輸出暫存至結束。命令自行連網不受此防護。");
        var policyBar = new WrapPanel(); policy.Children.Add(policyBar);
        Action(policyBar, "save", "儲存設定", Save);
        Actions["save"].SetResourceReference(StyleProperty, "AccentButtonStyle");
        settings.Children.Add(Advanced);
        var advanced = new StackPanel(); Advanced.Content = advanced;
        var host = DesignSystem.Card(advanced, "主機授權設定");
        host.Children.Add(hostOptions);
        DesignSystem.Field(hostOptions, "每次授權上限（分鐘，1–60）", Maximum);
        DesignSystem.Field(hostOptions, "到期或撤銷時的工作處置", Expiry);
        Hint(host, "通訊失效時會要求終止主機工作；停止未確認會顯示異常。");
        var detection = DesignSystem.Card(advanced, "敏感資訊規則"); detection.Children.Add(detectorState);
        rules.ItemsSource = Rules;
        rules.Columns.Add(new DataGridCheckBoxColumn { Header = "啟用", Binding = new Binding("enabled") });
        rules.Columns.Add(new DataGridTextColumn { Header = "識別碼", Binding = new Binding("id"), Width = 130 });
        rules.Columns.Add(new DataGridComboBoxColumn { Header = "類型", ItemsSource = new[] { new Filter("builtin", "內建"), new Filter("literal", "固定文字") }, SelectedValuePath = "Code", DisplayMemberPath = "Label", SelectedValueBinding = new Binding("kind"), Width = 100 });
        rules.Columns.Add(new DataGridTextColumn { Header = "規則", Binding = new Binding("pattern"), Width = new DataGridLength(1, DataGridLengthUnitType.Star) });
        rules.Columns.Add(new DataGridComboBoxColumn { Header = "風險", ItemsSource = new[] { new Filter("high", "高"), new Filter("medium", "中"), new Filter("low", "低") }, SelectedValuePath = "Code", DisplayMemberPath = "Label", SelectedValueBinding = new Binding("risk"), Width = 70 });
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
        var draftBar = new WrapPanel(); advanced.Children.Add(draftBar);
        Action(draftBar, "defaults", "還原預設草稿", async () => { if (!ConfirmDiscard()) return; long requested = generation; var value = await backend.Send("security_defaults"); if (requested != generation) { Feedback.Text = "草稿已變更，保留目前編輯。"; return; } Hydrate(value.GetProperty("policy")); Changed(); Feedback.Text = "預設草稿尚未儲存。"; });
        Action(draftBar, "reload", "捨棄草稿", async () => { if (!ConfirmDiscard()) return; await Refresh(true); Feedback.Text = dirty ? "草稿已變更，保留目前編輯。" : "已載入儲存設定。"; });
        settings.Children.Add(Diagnostics);
        var local = new StackPanel(); Diagnostics.Content = local; local.Children.Add(channel);
        var localBar = new WrapPanel(); local.Children.Add(localBar);
        Action(localBar, "diagnose", "更新診斷", async () => { await Refresh(requireFresh: true); diagnostics.Text = Pretty(state.GetProperty("channel")) + "\n" + Pretty(state.GetProperty("clients")); });
        Action(localBar, "recover", "安全恢復", async () => { Apply(await backend.Send("security_recover"), false); Feedback.Text = "已重新驗證；授權保持撤銷，請查看執行層確認。"; });
        local.Children.Add(diagnostics); local.Children.Add(sessions);
        detection.Children.Add(preview);

        var audit = DesignSystem.Card(sectionPanels["events"], "安全事件");
        Hint(audit, "事件不含敏感原文；原始請求監控可能包含敏感內容，匯出前請檢視。");
        DesignSystem.Field(audit, "事件類型", kind); DesignSystem.Field(audit, "風險", risk);
        var auditBar = new WrapPanel(); audit.Children.Add(auditBar);
        Action(auditBar, "events", "更新事件", async () => { offset = 0; await LoadEvents(); });
        Action(auditBar, "previous", "上一頁", async () => { offset = Math.Max(0, offset - 100); await LoadEvents(); });
        Action(auditBar, "next", "下一頁", async () => { if (eventSnapshot.ValueKind == JsonValueKind.Object && offset + 100 < eventSnapshot.GetProperty("total").GetInt32()) offset += 100; await LoadEvents(); });
        Action(auditBar, "export", "匯出全部事件", Export);
        audit.Children.Add(eventCount); audit.Children.Add(events); audit.Children.Add(EventDetail);
        ScrollViewer.SetHorizontalScrollBarVisibility(events, ScrollBarVisibility.Disabled);
        var eventTemplate = new DataTemplate(); var eventText = new FrameworkElementFactory(typeof(TextBlock));
        eventText.SetBinding(TextBlock.TextProperty, new Binding("Text")); eventText.SetValue(TextBlock.TextWrappingProperty, TextWrapping.Wrap);
        eventTemplate.VisualTree = eventText; events.ItemTemplate = eventTemplate;
        audit.Children.Add(EventDiagnostics); EventDiagnostics.Content = detail;
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
                        EventDetail.Text = result.GetProperty("row").ValueKind == JsonValueKind.Object ? selected.Text + "\n" + EventExplanation(result.GetProperty("row").GetProperty("event").GetProperty("kind").GetString()) : "事件已不存在，請重新載入。";
                    }
                    finally { detailQuery.Release(); }
                }
                catch (Exception error) { EventDetail.Text = "事件詳情未確認：" + error.Message; }
        };
        sectionPanels["events"].Children.Add(ClearEvents); var clear = new StackPanel(); ClearEvents.Content = clear;
        Hint(clear, "清除前請匯出需要保留的事件，並確認最新紀錄。");
        DesignSystem.Field(clear, "輸入「清除安全事件」", clearConfirmation);
        var clearBar = new WrapPanel(); clear.Children.Add(clearBar);
        Action(clearBar, "clear", "確認清除全部事件", async () => { if (auditRevision.Length == 0) throw new InvalidOperationException("請先載入事件。"); var value = await backend.Send("security_clear", new { expected_revision = auditRevision, confirmation = clearConfirmation.Text }); kind.SelectedIndex = 0; risk.SelectedIndex = 0; offset = 0; ApplyEvents(value); clearConfirmation.Clear(); Feedback.Text = "已清除安全事件並保留清除紀錄。"; });
        SelectSection("overview");
        foreach (var check in new[] { Enabled, Host, Detector, Guard }) { check.Checked += (_, _) => Changed(); check.Unchecked += (_, _) => Changed(); }
        Maximum.TextChanged += (_, _) => Changed();
        Strategy.SelectionChanged += (_, _) => Changed(); Expiry.SelectionChanged += (_, _) => Changed();
        Duration.SelectionChanged += (_, _) => UpdateButtons(); Minutes.TextChanged += (_, _) => UpdateButtons();
        clearConfirmation.TextChanged += (_, _) => UpdateButtons(); rules.SelectionChanged += (_, _) => UpdateButtons();
        kind.SelectionChanged += (_, _) => FilterChanged(); risk.SelectionChanged += (_, _) => FilterChanged();
        backend.OperationChanged += () => Dispatcher.BeginInvoke(UpdateButtons);
        IsVisibleChanged += (_, _) => { if (IsVisible) { _ = Refresh(); _ = RefreshRecent(); } };
        UpdateButtons();
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
        string reason = GrantUnavailable();
        if (reason.Length > 0) throw new InvalidOperationException(reason);
        if (minutes < 1 || minutes > savedMaximum) throw new InvalidOperationException("授權時間超過已儲存上限，或上限尚未確認。");
        Apply(await backend.Send("security_grant", new { minutes }), false);
        Feedback.Text = "已送出授權，請查看執行層確認。";
    }
    private int SelectedMinutes() => Duration.SelectedItem is ComboBoxItem item && item.Tag is int minutes && minutes > 0 ? minutes : ParseMinutes(Minutes.Text);
    internal bool ConfirmDiscard()
    {
        if (!dirty) return true;
        const string message = "設定有未儲存變更。確定捨棄目前草稿？";
        var owner = Window.GetWindow(this);
        bool accepted = backend.IsFixture && owner is DesktopWindow window && window.ConfirmFixture is not null
            ? window.ConfirmFixture(message)
            : MessageBox.Show(owner, message, "安全中心", MessageBoxButton.YesNo, MessageBoxImage.Warning, MessageBoxResult.No) == MessageBoxResult.Yes;
        if (!accepted) Feedback.Text = "已保留未儲存草稿。";
        return accepted;
    }
    private void OpenDiagnostics()
    {
        SelectSection("settings"); Diagnostics.IsExpanded = true;
        Dispatcher.BeginInvoke(() => Diagnostics.BringIntoView());
    }
    private string GrantUnavailable()
    {
        if (!stateKnown) return "安全狀態未確認，請更新診斷。";
        var policy = state.GetProperty("policy");
        if (!policy.GetProperty("enabled").GetBoolean()) return "請先在設定啟用並儲存安全政策。";
        if (!policy.GetProperty("host_authorization").GetBoolean()) return "已儲存政策未啟用主機限時授權。";
        if (state.GetProperty("effective").GetString() == "error") return "安全狀態異常，請先診斷並安全恢復。";
        if (!LeaseConfirmed()) return "等待主機模式連線確認已儲存政策與授權狀態。";
        if (state.GetProperty("authorization").GetProperty("authorized").GetBoolean()) return "授權有效；若要變更時間，請先撤銷。";
        return "";
    }
    private bool LeaseConfirmed() => stateKnown && state.GetProperty("effective").GetString() == "confirmed" &&
        state.GetProperty("clients").EnumerateArray().Any(row => row.GetProperty("state").GetString() == "ready" &&
            row.GetProperty("mode").GetString() == "host_control" && row.GetProperty("lease_generation").GetInt32() == state.GetProperty("authorization").GetProperty("generation").GetInt32());
    private static TextBox Output(double height) => new() { IsReadOnly = true, AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, VerticalScrollBarVisibility = ScrollBarVisibility.Auto, MinHeight = 60, MaxHeight = height, Margin = new Thickness(0, 8, 0, 8) };
    private static void Hint(Panel panel, string text) { var hint = DesignSystem.Text(text); DesignSystem.Secondary(hint); hint.Margin = new Thickness(0, 8, 0, 12); panel.Children.Add(hint); }
    private static string Pretty(JsonElement value) => JsonSerializer.Serialize(value, JsonData.PrettyOptions);
    private static int ParseMinutes(string value) => int.TryParse(value, out int number) && number is >= 1 and <= 60 ? number : throw new InvalidOperationException("分鐘數必須為 1 至 60。");
    private void Action(Panel panel, string name, string label, Func<Task> action)
    {
        var button = Ui.Button(label); button.MinWidth = 88; button.MinHeight = 32; button.Padding = new Thickness(12, 5, 12, 5);
        panel.Children.Add(button); Actions[name] = button;
        DesignSystem.Name(button, label);
        button.Click += (_, _) => { if (button.IsEnabled && button.Visibility == Visibility.Visible) LastOperation = Operate(action); };
    }
    private async Task Operate(Func<Task> action)
    {
        if (operating || backend.OperationPending) { Feedback.Text = "先前操作尚未確認，未重送。"; return; }
        operating = true; UpdateButtons(); Feedback.SetResourceReference(TextBlock.ForegroundProperty, "TextPrimary"); Feedback.Text = "操作中…";
        try { await action(); }
        catch (Exception error) { ShowError(error); }
        finally { operating = false; UpdateButtons(); _ = RefreshRecent(); }
    }
    private void UpdateButtons()
    {
        bool idle = !operating && !backend.OperationPending;
        foreach (var button in Actions.Values) button.IsEnabled = idle;
        bool active = stateKnown && state.GetProperty("authorization").GetProperty("authorized").GetBoolean();
        string reason = GrantUnavailable();
        foreach (ComboBoxItem item in Duration.Items) item.IsEnabled = item.Tag is int duration && (duration == 0 || savedMaximum >= duration);
        grantOptions.Visibility = active ? Visibility.Collapsed : Visibility.Visible;
        customDuration.Visibility = Duration.SelectedIndex == 3 ? Visibility.Visible : Visibility.Collapsed;
        Duration.IsEnabled = Minutes.IsEnabled = idle && reason.Length == 0;
        bool validDuration = false;
        try { validDuration = SelectedMinutes() <= savedMaximum; } catch (InvalidOperationException) { }
        GrantReason.Text = reason.Length > 0 ? reason : !validDuration ? $"請選擇 1–{savedMaximum} 分鐘。" : $"已儲存上限 {savedMaximum} 分鐘";
        Actions["grant"].Visibility = active ? Visibility.Collapsed : Visibility.Visible;
        Actions["grant"].IsEnabled = idle && reason.Length == 0 && validDuration;
        Actions["grant"].ToolTip = GrantReason.Text;
        Actions["revoke"].Visibility = active || !stateKnown ? Visibility.Visible : Visibility.Collapsed;
        Actions["revoke"].SetResourceReference(StyleProperty, "AccentButtonStyle");
        bool abnormal = !stateKnown || state.GetProperty("effective").GetString() is "error" or "unconfirmed";
        Actions["open_diagnostics"].Visibility = abnormal ? Visibility.Visible : Visibility.Collapsed;
        Actions["recover"].Visibility = abnormal ? Visibility.Visible : Visibility.Collapsed;
        bool policyEnabled = Enabled.IsChecked == true;
        Host.IsEnabled = Detector.IsEnabled = Guard.IsEnabled = policyEnabled;
        // Keep conflicting values editable, but never silently enable a dependency or save an invalid draft.
        Detector.IsEnabled = policyEnabled && (Guard.IsChecked != true || Detector.IsChecked != true);
        Guard.IsEnabled = policyEnabled && (Detector.IsChecked == true || Guard.IsChecked == true);
        hostOptions.IsEnabled = policyEnabled && Host.IsChecked == true;
        strategyOptions.Visibility = policyEnabled && Guard.IsChecked == true ? Visibility.Visible : Visibility.Collapsed;
        bool conflict = policyEnabled && Guard.IsChecked == true && Detector.IsChecked != true;
        dependency.Text = !policyEnabled ? "啟用政策後可調整防護選項。" : conflict ? "回傳防護需要敏感資訊偵測；請啟用偵測或關閉回傳防護。" : Guard.IsChecked == true ? "回傳防護使用敏感資訊偵測。" : "";
        bool validMaximum = int.TryParse(Maximum.Text, out int maximum) && maximum is >= 1 and <= 60;
        if (!validMaximum) dependency.Text = "授權上限必須為 1–60 分鐘。";
        Actions["save"].IsEnabled = idle && dirty && stateKnown && state.GetProperty("effective").GetString() != "error" && !conflict && validMaximum;
        Actions["save"].ToolTip = !stateKnown ? "請先更新診斷確認狀態。" : !dirty ? "沒有未儲存變更。" : conflict || !validMaximum ? dependency.Text : "儲存後需等待執行層確認，現有授權會撤銷。";
        Actions["reload"].IsEnabled = idle && dirty;
        Actions["add_rule"].IsEnabled = idle && Rules.Count < 32;
        Actions["remove_rule"].IsEnabled = idle && Rules.Count > 1 && rules.SelectedItem is SecurityRule;
        Actions["previous"].IsEnabled = idle && !loadingEvents && auditRevision.Length > 0 && offset > 0;
        Actions["next"].IsEnabled = idle && !loadingEvents && auditRevision.Length > 0 && eventSnapshot.ValueKind == JsonValueKind.Object && offset + 100 < eventSnapshot.GetProperty("total").GetInt32();
        Actions["events"].IsEnabled = idle && !loadingEvents;
        Actions["clear"].IsEnabled = idle && !loadingEvents && auditRevision.Length > 0 && clearConfirmation.Text == "清除安全事件";
        Actions["clear"].ToolTip = auditRevision.Length == 0 ? "請先更新並驗證事件。" : "輸入完整確認文字，下一步仍須本機確認。";
        Draft.Text = dirty ? "有未儲存變更" : "";
        Draft.Visibility = dirty ? Visibility.Visible : Visibility.Collapsed;
    }
    private void Changed() { if (hydrating) return; if (!dirty) draftRevision = savedRevision; dirty = true; generation++; UpdateButtons(); }
    private void ShowError(Exception error)
    {
        diagnostics.Text = error.Message;
        string message = error.Message; int separator = message.IndexOf('：');
        Feedback.Text = separator > 0 ? message[(separator + 1)..] : message;
        Feedback.SetResourceReference(TextBlock.ForegroundProperty, "ErrorText");
    }
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
        var value = await backend.Send("security_save", new { policy = form, expected_revision = draftRevision });
        if (submitted == generation) dirty = false;
        else draftRevision = value.GetProperty("saved_revision").GetString()!;
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
        long requested = generation, requestedState = stateGeneration;
        try { var value = await backend.Send("security_state"); if (requestedState == stateGeneration) Apply(value, discardDraft && requested == generation); lastRefresh = DateTimeOffset.UtcNow; }
        catch (Exception error)
        {
            stateKnown = false; savedMaximum = 0; Status.Text = "狀態未確認：" + error.Message + "；最後更新 " + lastRefresh.ToLocalTime().ToString("HH:mm:ss");
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
    internal void Apply(JsonElement value, bool discardDraft)
    {
        state = value.Clone(); stateKnown = true; stateGeneration++; savedRevision = value.GetProperty("saved_revision").GetString()!;
        if (!dirty || discardDraft) { Hydrate(value.GetProperty("policy")); dirty = false; }
        string effective = value.GetProperty("effective").GetString()!;
        bool enabled = value.GetProperty("policy").GetProperty("enabled").GetBoolean();
        var savedPolicy = value.GetProperty("policy");
        Status.Text = (savedRevision.Length == 0 ? "預設關閉 · 尚未儲存" : $"已儲存：{(enabled ? "啟用" : "停用")}") + "\n" + (effective switch { "confirmed" => enabled ? "已生效 · 執行層已確認" : "已停用 · 執行層已確認", "no_client" => "等待確認 · 尚無受管理連線", "error" => "異常 · 請診斷並安全恢復", _ => "等待執行層確認" });
        if (value.GetProperty("error").ValueKind == JsonValueKind.String) Status.Text += "\n" + value.GetProperty("error").GetString();
        var auth = value.GetProperty("authorization");
        bool ack = LeaseConfirmed();
        savedMaximum = savedPolicy.GetProperty("max_lease_minutes").GetInt32();
        bool authorized = auth.GetProperty("authorized").GetBoolean();
        int remaining = auth.GetProperty("remaining_seconds").GetInt32();
        authorization.Text = (authorized ? $"剩餘 {remaining / 60:00}:{remaining % 60:00}" : "目前未授權") + (authorized || enabled && savedPolicy.GetProperty("host_authorization").GetBoolean() ? $" · {(ack ? "執行層已確認" : "等待執行層確認")}" : "");
        channel.Text = "通訊狀態：" + (effective == "confirmed" ? "已確認" : "未確認");
        sessions.Text = string.Join("\n", value.GetProperty("clients").EnumerateArray().Select(row => $"{row.GetProperty("mode").GetString()} · {row.GetProperty("state").GetString()}\n" + Pretty(row.GetProperty("sessions"))));
        detectorState.Text = $"已儲存：{(savedPolicy.GetProperty("detector_enabled").GetBoolean() ? "啟用" : "停用")} · 啟用規則 {savedPolicy.GetProperty("rules").EnumerateArray().Count(row => row.GetProperty("enabled").GetBoolean())} 筆";
        guardState.Text = !enabled ? "回傳防護隨安全政策停用" : $"回傳防護：{(savedPolicy.GetProperty("response_guard").GetBoolean() ? savedPolicy.GetProperty("strategy").GetString() == "mask" ? "遮罩敏感文字" : "阻擋敏感回傳" : "停用")}" + (effective == "confirmed" ? "" : "（已儲存，尚未確認生效）");
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
        catch (Exception error) { eventCount.Text = "安全事件未確認：" + error.Message; auditRevision = ""; UpdateButtons(); }
    }
    private async void FilterChanged()
    {
        long requested = ++filterGeneration; offset = 0; auditRevision = ""; UpdateButtons();
        await Task.Delay(180);
        if (requested == filterGeneration) await LoadEventsSafely();
    }
    private static string EventExplanation(string? kind) => kind switch
    {
        "policy_saved" => "政策已保存；實際生效狀態請查看總覽。",
        "authorization_granted" => "本機已核准限時授權；執行層仍須確認。",
        "authorization_revoked" => "授權已撤銷；既有工作依已儲存策略處置。",
        "authorization_expired" => "限時授權已到期。",
        "channel_failed" => "通訊無法確認，請查看診斷與工作停止結果。",
        "channel_recovered" => "已重新驗證通訊，未自動授權。",
        "response_masked" => "回傳中的敏感文字已遮罩。",
        "response_blocked" => "回傳已由防護規則阻擋。",
        "scan_failed" => "無法完整檢查資料，不能視為安全。",
        "command_denied" => "執行層拒絕命令，未放寬授權。",
        "session_stopped" => "主機工作已停止。",
        "session_stop_failed" => "主機工作停止未確認，請進入診斷。",
        "synthetic_test" => "使用合成資料的本機測試。",
        "audit_cleared" => "保留清除邊界與前一段事件鏈摘要。",
        "events_lost" => "部分事件未送達；請檢視缺口並安全恢復。",
        _ => "請展開原始診斷資訊。"
    };
    private static string EventName(string? kind) => kind switch
        {
            "policy_saved" => "設定已儲存", "authorization_granted" => "已送出授權", "authorization_revoked" => "授權已撤銷",
            "authorization_expired" => "授權已到期", "channel_failed" => "通訊異常", "channel_recovered" => "已恢復通訊",
            "response_masked" => "回傳已遮罩", "response_blocked" => "回傳已阻擋", "scan_failed" => "檢查失敗",
            "command_denied" => "命令已拒絕", "session_stopped" => "主機工作已停止", "session_stop_failed" => "主機工作停止未確認",
            "synthetic_test" => "合成測試", "audit_cleared" => "事件已清除", "events_lost" => "事件遺失", _ => "其他事件"
        };
    private static string FormatEvent(JsonElement e)
    {
        string risk = e.GetProperty("risk").GetString() switch { "high" => "高風險", "medium" => "中風險", "low" => "低風險", _ => "風險未確認" };
        string? rawTime = e.GetProperty("time").GetString();
        string time = DateTimeOffset.TryParse(rawTime, out var timestamp) ? timestamp.ToLocalTime().ToString("yyyy/MM/dd HH:mm:ss") : "時間未確認";
        return $"{time} · {EventName(e.GetProperty("kind").GetString())} · {risk} · {e.GetProperty("count").GetInt32()} 次";
    }
    private async Task LoadEvents()
    {
        long requested = filterGeneration;
        string requestedKind = ((Filter)kind.SelectedItem).Code, requestedRisk = ((Filter)risk.SelectedItem).Code;
        int requestedOffset = offset;
        await eventQuery.WaitAsync();
        try
        {
            if (requested != filterGeneration) return;
            loadingEvents = true; UpdateButtons();
            var value = await backend.Send("security_events", new { kind = requestedKind, risk = requestedRisk, offset = requestedOffset });
            if (requested == filterGeneration) ApplyEvents(value);
        }
        catch { auditRevision = ""; events.ItemsSource = null; throw; }
        finally { loadingEvents = false; eventQuery.Release(); UpdateButtons(); }
    }
    private void ApplyEvents(JsonElement value)
    {
        eventSnapshot = value.Clone(); auditRevision = value.GetProperty("revision").GetString()!;
        events.ItemsSource = value.GetProperty("rows").EnumerateArray().Select(row => { var e = row.GetProperty("event"); return new AuditRow(e.GetProperty("id").GetString()!, FormatEvent(e)); }).ToArray();
        offset = value.GetProperty("offset").GetInt32();
        int total = value.GetProperty("total").GetInt32();
        eventCount.Text = total == 0 ? "沒有符合的事件 · 事件鏈已驗證" : $"第 {offset + 1}–{Math.Min(offset + 100, total)} 筆，共 {total} 筆 · 事件鏈已驗證";
        UpdateButtons();
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
