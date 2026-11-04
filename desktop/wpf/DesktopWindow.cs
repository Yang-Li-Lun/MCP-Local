using System.IO;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Threading;
using Microsoft.Win32;

namespace McpLocal.Desktop;

internal sealed class DesktopWindow : Window
{
    private readonly BackendClient backend;
    internal readonly RemoteMonitorSource Source;
    internal readonly MonitorView Monitor;
    internal readonly TabControl Tabs = new();
    internal readonly TextBox Tunnel = new();
    internal readonly ComboBox ThemeChoice = new() { ItemsSource = new[] { "跟隨 Windows", "淺色", "深色" }, SelectedIndex = 0, Width = 220, HorizontalAlignment = HorizontalAlignment.Left };
    private TextBox tunnel => Tunnel;
    private readonly PasswordBox key = new() { MaxLength = 16384 };
    private readonly CheckBox hidden = new() { Content = "下次開啟時直接縮到系統匣" };
    private readonly CheckBox autoStart = new() { Content = "Windows 登入後啟動到系統匣並自動連線" };
    private readonly ListBox folders = new() { MinHeight = 100, MaxHeight = 220, SelectionMode = SelectionMode.Extended };
    private readonly Dictionary<string, TextBox> numbers = [];
    private readonly Dictionary<string, ReaderListEditor> lists = [];
    private readonly Dictionary<string, RadioButton> modes = [], icons = [];
    private readonly Dictionary<string, Button> commands = [];
    private readonly List<FrameworkElement> pageLayouts = [];
    private readonly List<Panel> settingsHosts = [];
    internal readonly Grid GlobalActions = new();
    internal readonly StackPanel SettingsActions = new() { Orientation = Orientation.Horizontal };
    internal readonly StackPanel ConnectionActions = new() { Orientation = Orientation.Horizontal };
    internal readonly TextBlock ConnectionState = new() { FontWeight = FontWeights.SemiBold, VerticalAlignment = VerticalAlignment.Center };
    internal readonly TextBlock DraftState = new() { VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 0, 12, 0), FontSize = 12 };
    private readonly TextBlock status = new() { TextWrapping = TextWrapping.Wrap }, backgroundStatus = new(), accessStatus = new();
    private readonly TextBlock keyStatus = new(), folderStatus = new();
    private readonly DispatcherTimer poll = new() { Interval = TimeSpan.FromMilliseconds(250) };
    private readonly DispatcherTimer editDelay = new() { Interval = TimeSpan.FromMilliseconds(180) };
    private readonly List<Button> folderButtons = [];
    private bool hydrating, localDirty, polling, permitClose, ready, keyEdited;
    private long generation;
    private string iconStyle = "classic", accessMode = "read_only";
    private Window? detachedWindow;
    private MonitorView? detachedMonitor;
    internal JsonElement LastState { get; private set; }
    internal Task Ready { get; private set; } = Task.CompletedTask;
    internal MonitorView? DetachedMonitor => detachedMonitor;
    internal Func<string, bool>? ConfirmFixture { get; set; }

    public DesktopWindow(BackendClient backend)
    {
        this.backend = backend;
        Source = new RemoteMonitorSource(backend); Monitor = new MonitorView(Source);
        Source.Changed += () => { Monitor.Refresh(); detachedMonitor?.Refresh(); };
        Monitor.DetachRequested += OpenMonitor;
        Title = backend.IsFixture ? "MCP-Local｜WPF 隔離整合驗證" : "MCP-Local｜連線與快捷設定";
        if (backend.IsFixture && backend.HasNativeTray) Title += "（原生系統匣）";
        Opacity = 0; // Avoid flashing a settings window for hidden/login startup.
        Width = 1120; Height = 880; MinWidth = 620; MinHeight = 480;
        DesignSystem.Window(this);
        WindowStartupLocation = WindowStartupLocation.CenterScreen; UseLayoutRounding = true;
        var root = new DockPanel(); Content = root;
        var bottomFrame = new Border { Padding = new Thickness(20, 12, 20, 12), BorderThickness = new Thickness(0, 1, 0, 0) };
        bottomFrame.SetResourceReference(Border.BackgroundProperty, "Surface"); bottomFrame.SetResourceReference(Border.BorderBrushProperty, "Divider");
        DockPanel.SetDock(bottomFrame, Dock.Bottom); root.Children.Add(bottomFrame);
        bottomFrame.Child = GlobalActions;
        GlobalActions.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        GlobalActions.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        var connectionInfo = new StackPanel { Margin = new Thickness(0, 0, 16, 0), VerticalAlignment = VerticalAlignment.Center };
        connectionInfo.Children.Add(ConnectionState);
        ConnectionState.SetBinding(ToolTipProperty, new System.Windows.Data.Binding("Text") { Source = status });
        GlobalActions.Children.Add(connectionInfo); Ui.Place(GlobalActions, ConnectionActions, 0, 1);
        ConnectionActions.VerticalAlignment = VerticalAlignment.Center;
        DesignSystem.Name(SettingsActions, "設定操作"); DesignSystem.Name(ConnectionActions, "連線操作");
        DesignSystem.Secondary(DraftState); SettingsActions.Children.Add(DraftState);
        foreach (var (name, label) in new[] { ("save", "儲存設定"), ("start", "啟動連線"), ("restart", "儲存並重連"), ("stop", "停止連線") })
        {
            var button = Ui.Button(label); commands[name] = button;
            (name is "save" or "restart" ? SettingsActions : ConnectionActions).Children.Add(button);
            button.Click += async (_, _) => await Run(name, includeForm: name is "save" or "start" or "restart");
        }
        var more = Ui.Button("更多"); more.Margin = new Thickness(0); ConnectionActions.Children.Add(more);
        var menu = new ContextMenu(); Ui.Menu(menu, "獨立監控視窗", OpenMonitor);
        Ui.Menu(menu, "縮到系統匣", HideToTray); Ui.Menu(menu, "退出程式", () => _ = Run("quit")); Ui.Attach(more, menu);
        commands["start"].SetResourceReference(StyleProperty, "AccentButtonStyle");
        status.Margin = new Thickness(0, 4, 0, 0); status.FontSize = 12; DesignSystem.Secondary(status);
        status.TextWrapping = TextWrapping.NoWrap; status.TextTrimming = TextTrimming.CharacterEllipsis;
        status.SetBinding(ToolTipProperty, new System.Windows.Data.Binding("Text") { Source = status });
        System.Windows.Automation.AutomationProperties.SetLiveSetting(ConnectionState, System.Windows.Automation.AutomationLiveSetting.Polite);
        System.Windows.Automation.AutomationProperties.SetLiveSetting(status, System.Windows.Automation.AutomationLiveSetting.Polite);
        Tabs.SetResourceReference(StyleProperty, "Navigation"); root.Children.Add(Tabs);
        Tabs.SelectionChanged += (_, e) => { if (ReferenceEquals(e.Source, Tabs)) UpdateSettingsActions(); };
        SizeChanged += (_, _) => UpdatePageLayout();
        var connection = Page("連線設定");
        var folderCard = DesignSystem.Card(connection, "授權資料夾");
        DesignSystem.Name(folders, "授權資料夾"); folderCard.Children.Add(folders);
        var folderBar = new WrapPanel { Margin = new Thickness(0, 8, 0, 8) }; folderCard.Children.Add(folderBar);
        var credentials = DesignSystem.Card(connection, "連線");
        DesignSystem.Field(credentials, "通道識別碼", tunnel);
        DesignSystem.Field(credentials, "API 金鑰", key); credentials.Children.Add(keyStatus);
        var startupCard = DesignSystem.Card(connection, "啟動與背景執行");
        hidden.Margin = new Thickness(0, 0, 0, 8); startupCard.Children.Add(hidden);
        startupCard.Children.Add(autoStart); backgroundStatus.Margin = new Thickness(0, 8, 0, 8); startupCard.Children.Add(backgroundStatus);
        var check = Ui.Button("更新自啟動狀態"); check.HorizontalAlignment = HorizontalAlignment.Left;
        commands["check_background"] = check; startupCard.Children.Add(check);
        check.Click += async (_, _) => await Run("check_background");
        foreach (var text in new[] { keyStatus, folderStatus, backgroundStatus, accessStatus }) DesignSystem.Secondary(text);
        keyStatus.Margin = new Thickness(0, 8, 0, 0);
        foreach (var (label, change) in new[] { ("加入資料夾…", false), ("變更選取…", true) })
        {
            var button = Ui.Button(label); folderButtons.Add(button); folderBar.Children.Add(button);
            button.Click += async (_, _) =>
            {
                int? index = change && folders.SelectedItems.Count == 1 ? folders.SelectedIndex : null;
                if (change && index is null) return;
                var picker = new OpenFolderDialog { Title = "選擇授權資料夾", Multiselect = false };
                if (picker.ShowDialog(this) == true)
                { await FlushDraft(); await Run("folder", new { path = picker.FolderName, index }, hydrate: true); }
            };
        }
        var remove = Ui.Button("移除選取"); folderButtons.Add(remove); folderBar.Children.Add(remove);
        remove.Click += async (_, _) =>
        {
            var indexes = folders.SelectedItems.Cast<object>().Select(item => folders.Items.IndexOf(item)).ToArray();
            await FlushDraft(); await Run("remove_folders", new { indexes }, hydrate: true);
        };
        folderCard.Children.Add(folderStatus);
        tunnel.TextChanged += (_, _) => Dirty(); key.PasswordChanged += (_, _) => { if (!hydrating) keyEdited = true; Dirty(); };
        hidden.Checked += (_, _) => Dirty(); hidden.Unchecked += (_, _) => Dirty();
        autoStart.Checked += (_, _) => Dirty(); autoStart.Unchecked += (_, _) => Dirty();
        Loaded += (_, _) => { backend.Start(); Ready = Initialize(); };
        Closing += (_, e) =>
        {
            if (!permitClose)
            {
                e.Cancel = true;
                if (!backend.HasNativeTray) _ = Run("quit"); else HideToTray();
            }
        };
        StateChanged += (_, _) => { if (WindowState == WindowState.Minimized && backend.HasNativeTray) HideToTray(); };
        Closed += (_, _) => { poll.Stop(); editDelay.Stop(); Monitor.Dispose(); detachedMonitor?.Dispose(); detachedWindow?.Close(); Source.Dispose(); backend.Dispose(); };
        PreviewKeyDown += async (_, e) =>
        {
            if (e.Key == Key.S && Keyboard.Modifiers.HasFlag(ModifierKeys.Control)) { e.Handled = true; await Run("save", includeForm: true); }
            if (Keyboard.Modifiers == ModifierKeys.Alt && e.SystemKey >= Key.D1 && e.SystemKey <= Key.D5)
            { Tabs.SelectedIndex = e.SystemKey - Key.D1; ((TabItem)Tabs.SelectedItem).Focus(); e.Handled = true; }
        };
        backend.Event += message => Dispatcher.BeginInvoke(() => HandleEvent(message));
        backend.Disconnected += () => Dispatcher.BeginInvoke(() =>
        {
            if (permitClose) return;
            poll.Stop(); status.Text = "桌面後端已結束；連線清理與電源回復結果請查閱既有診斷。";
            ConnectionState.Text = "● 後端已結束";
            foreach (var button in commands.Values) button.IsEnabled = false;
            permitClose = true; Show();
        });
        poll.Tick += async (_, _) => await Poll();
        editDelay.Tick += async (_, _) => { editDelay.Stop(); await FlushDraft(); };
    }
    private StackPanel Page(string name)
    {
        var panel = new StackPanel();
        var layout = new DockPanel(); pageLayouts.Add(layout);
        var heading = new DockPanel { LastChildFill = false, Margin = new Thickness(0, 0, 0, 20) };
        var title = DesignSystem.Text(name, "PageTitle"); title.Margin = new Thickness(0, 0, 16, 0); title.VerticalAlignment = VerticalAlignment.Center;
        heading.Children.Add(title);
        var actions = new StackPanel { Orientation = Orientation.Horizontal, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center };
        DockPanel.SetDock(actions, Dock.Right); heading.Children.Add(actions); settingsHosts.Add(actions);
        DockPanel.SetDock(heading, Dock.Top); layout.Children.Add(heading);
        layout.Children.Add(new ScrollViewer { Content = panel, VerticalScrollBarVisibility = ScrollBarVisibility.Auto, HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled, Focusable = false });
        AddPage(name, layout); UpdatePageLayout();
        return panel;
    }
    private void UpdateSettingsActions()
    {
        if (SettingsActions.Parent is Panel previous) previous.Children.Remove(SettingsActions);
        if (Tabs.SelectedIndex >= 0 && Tabs.SelectedIndex < settingsHosts.Count)
            settingsHosts[Tabs.SelectedIndex].Children.Add(SettingsActions);
    }
    private void UpdatePageLayout()
    {
        bool compact = ActualWidth > 0 && ActualWidth < 920;
        Tabs.Tag = compact ? "Compact" : "Expanded";
        foreach (var layout in pageLayouts)
        {
            layout.Margin = new Thickness(compact ? 16 : 28, compact ? 16 : 24, compact ? 16 : 28, 16);
            if (layout is DockPanel dock && dock.Children.Count > 0 && dock.Children[0] is FrameworkElement heading && heading is not MonitorView)
            {
                heading.Margin = new Thickness(0, 0, 0, dock.Children.Contains(Monitor) ? 8 : compact ? 12 : 20);
                var title = heading as TextBlock ?? (heading is Panel host ? host.Children.OfType<TextBlock>().FirstOrDefault() : null);
                if (title is not null) title.FontSize = compact ? 24 : 28;
            }
        }
        DraftState.Visibility = ActualWidth < 760 ? Visibility.Collapsed : Visibility.Visible;
    }
    private void AddPage(string name, UIElement content)
    {
        string glyph = name switch { "連線設定" => "\uE839", "進階讀取設定" => "\uE713", "外觀" => "\uE790", "權限模式" => "\uE72E", _ => "\uE9D9" };
        Tabs.Items.Add(new TabItem { Header = name, Tag = glyph, Content = content });
    }
    private static void Label(Panel panel, string label)
    {
        var hint = DesignSystem.Text(label); DesignSystem.Secondary(hint);
        hint.Margin = new Thickness(0, 16, 0, 8); panel.Children.Add(hint);
    }
    private async Task Initialize()
    {
        try
        {
            var hello = await backend.Send("hello");
            if (hello.GetProperty("protocol").GetInt32() != 1) throw new InvalidOperationException("桌面協定版本不符。");
            var advanced = Page("進階讀取設定");
            var capacity = DesignSystem.Card(advanced, "讀取與掃描上限");
            var numericGrid = new System.Windows.Controls.Primitives.UniformGrid { Columns = 2, Margin = new Thickness(0, 0, -16, 0) }; capacity.Children.Add(numericGrid);
            foreach (var field in hello.GetProperty("numbers").EnumerateObject())
            {
                bool bytes = field.Name.EndsWith("_bytes");
                var fieldPanel = new StackPanel { Margin = new Thickness(0, 0, 16, 16) }; numericGrid.Children.Add(fieldPanel);
                var editor = new TextBox { MaxLength = 40 };
                DesignSystem.Field(fieldPanel, field.Value[0].GetString() + (bytes ? "（MiB）" : "（個）"), editor);
                editor.ToolTip = $"最高 {field.Value[2].GetInt64() / (bytes ? 1048576 : 1):N0}";
                System.Windows.Automation.AutomationProperties.SetHelpText(editor, editor.ToolTip.ToString());
                numbers[field.Name] = editor; editor.TextChanged += (_, _) => Dirty();
            }
            var readerBar = new WrapPanel(); capacity.Children.Add(readerBar);
            foreach (var (command, label) in new[] { ("reader_maximums", "最大值"), ("reader_defaults", "還原進階預設值") })
            { var button = Ui.Button(label); readerBar.Children.Add(button); button.Click += async (_, _) => { await FlushDraft(); await Run(command, hydrate: true); }; }
            var filters = DesignSystem.Card(advanced, "檔案類型與排除");
            foreach (var (name, label) in new[] { ("extensions", "文字 reader 副檔名"), ("text_names", "文字 reader 完整檔名"), ("excluded_names", "額外排除名稱") })
            {
                if (lists.Count > 0)
                {
                    var divider = new Border { Height = 1, Margin = new Thickness(0, 16, 0, 0) };
                    divider.SetResourceReference(Border.BackgroundProperty, "Divider"); filters.Children.Add(divider);
                }
                var listEditor = new ReaderListEditor(label);
                lists[name] = listEditor; DesignSystem.Field(filters, label, listEditor); listEditor.LogicalChanged += (_, _) => Dirty();
            }
            Label(filters, "不接受路徑或萬用字元。文字清單只限制 UTF-8 reader；固定安全排除、隱藏路徑與連結防護持續生效。");
            var appearance = Page("外觀");
            var themeCard = DesignSystem.Card(appearance, "色彩模式");
            DesignSystem.Name(ThemeChoice, "色彩模式"); themeCard.Children.Add(ThemeChoice);
            var themeHint = DesignSystem.Text("立即套用至所有視窗，本次開啟期間生效。高對比優先使用 Windows 色彩。");
            themeHint.Margin = new Thickness(0, 12, 0, 0); DesignSystem.Secondary(themeHint); themeCard.Children.Add(themeHint);
            ThemeChoice.SelectionChanged += (_, _) => DesignSystem.Apply(new[] { "System", "Light", "Dark" }[ThemeChoice.SelectedIndex]);
            var iconCard = DesignSystem.Card(appearance, "應用程式圖示");
            var iconOptions = new WrapPanel(); iconCard.Children.Add(iconOptions);
            foreach (var (value, label, filename) in new[] { ("classic", "經典資料夾", "mcp-local-classic.png"), ("folder_link", "連結資料夾", "mcp-local-v2.png") })
            {
                var content = new Image { Source = new BitmapImage(new Uri(Path.Combine(backend.Root, "assets", "icons", filename))), Width = 64, Height = 64, Margin = new Thickness(12, 4, 12, 4) };
                var radio = new RadioButton { GroupName = "icons", Content = content, VerticalContentAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 8, 16, 8), Padding = new Thickness(8) };
                radio.ToolTip = label + "｜儲存後套用至視窗與系統匣";
                DesignSystem.Name(radio, label); icons[value] = radio; iconOptions.Children.Add(radio); radio.Checked += (_, _) => { iconStyle = value; Dirty(); };
            }
            var access = Page("權限模式");
            foreach (var property in hello.GetProperty("modes").EnumerateObject())
            {
                var value = property.Name;
                var modeCard = DesignSystem.Card(access, "");
                var detail = new StackPanel();
                var modeName = DesignSystem.Text(property.Value.GetString()!, "SectionTitle"); modeName.Margin = new Thickness(0, 0, 0, 6); detail.Children.Add(modeName);
                var description = DesignSystem.Text(value switch
                {
                    "read_only" => "22 個工具 · 僅讀取授權資料夾。",
                    "developer_control" => "34 個工具 · Windows Sandbox VM 可直接修改授權資料夾；工具鏈唯讀，VM 不可用時拒絕命令。",
                    _ => "36 個工具 · 以目前 Windows 使用者執行受信任命令，具主機存取權限；不是沙箱。"
                });
                DesignSystem.Secondary(description); detail.Children.Add(description);
                var radio = new RadioButton { Content = detail, GroupName = "access", HorizontalContentAlignment = HorizontalAlignment.Stretch };
                DesignSystem.Name(radio, property.Value.GetString()!);
                modes[value] = radio; modeCard.Children.Add(radio); radio.Checked += (_, _) => { accessMode = value; Dirty(); };
            }
            access.Children.Add(accessStatus);
            var monitorPage = new DockPanel(); pageLayouts.Add(monitorPage); UpdatePageLayout();
            monitorPage.Children.Add(Monitor); AddPage("請求監控", monitorPage);
            ready = true; ApplyState(hello.GetProperty("state"), true);
            UpdateSettingsActions();
            ApplyIcon(iconStyle);
            if (hello.GetProperty("state").GetProperty("key_load_error").ValueKind == JsonValueKind.String)
                MessageBox.Show(this, "無法讀取已保存的金鑰；原檔保留，請重新輸入。", "金鑰讀取失敗");
            if (!LastState.GetProperty("visible").GetBoolean()) HideToTray();
            Opacity = 1;
            poll.Start(); await Poll();
        }
        catch (Exception error)
        {
            if (permitClose) return;
            status.Text = error.Message; permitClose = true; Opacity = 1; Show(); throw;
        }
    }
    private void Dirty()
    {
        if (hydrating || !ready) return;
        localDirty = true; generation++; editDelay.Stop(); editDelay.Start();
        DraftState.Text = "有未儲存變更";
        if (commands.TryGetValue("save", out var save)) save.IsEnabled = true;
    }
    private Dictionary<string, object?> Form()
    {
        var form = new Dictionary<string, object?> { ["tunnel"] = tunnel.Text, ["start_hidden"] = hidden.IsChecked == true,
            ["auto_start"] = autoStart.IsChecked == true, ["icon_style"] = iconStyle, ["access_mode"] = accessMode,
            ["numbers"] = numbers.ToDictionary(x => x.Key, x => x.Value.Text), ["lists"] = lists.ToDictionary(x => x.Key, x => x.Value.Text) };
        if (keyEdited) form["key"] = key.Password;
        return form;
    }
    private async Task FlushDraft()
    {
        if (!ready || !localDirty) return;
        await Run("edit", new { form = Form() });
    }
    internal async Task Run(string command, object? args = null, bool includeForm = false, bool hydrate = false)
    {
        if (!ready) return;
        long version = generation;
        try
        {
            editDelay.Stop();
            var state = await backend.Send(command, includeForm ? new { form = Form() } : args);
            if (generation == version && (includeForm || command == "edit")) localDirty = false;
            ApplyState(state, hydrate && generation == version);
        }
        catch (Exception error) { status.Text = error.Message; }
    }
    internal async Task Poll()
    {
        if (polling || !ready || permitClose) return;
        polling = true;
        try
        {
            ApplyState(await backend.Send("state"), false);
            Source.Update(await backend.Send("monitor"));
        }
        catch (Exception error) { status.Text = error.Message; }
        finally { polling = false; }
    }
    private void ApplyState(JsonElement state, bool hydrate)
    {
        LastState = state.Clone();
        status.Text = state.GetProperty("status").GetString(); backgroundStatus.Text = state.GetProperty("background_status").GetString();
        accessStatus.Text = state.GetProperty("access_status").GetString();
        keyStatus.Text = state.GetProperty("key_available").GetBoolean() ? "已保存金鑰；輸入新值可替換，留白保留原值。" : "儲存時以 Windows DPAPI 加密；不寫入一般設定。";
        var controls = state.GetProperty("controls");
        string connectionState = state.GetProperty("state").GetString()!;
        ConnectionState.Text = "● " + (connectionState switch
        {
            "RUNNING" => "連線已啟動", "STARTING" => "正在連線", "STOPPING" => "正在停止",
            "RESTARTING" => "正在重連", "QUITTING" => "正在退出",
            "REPAIR" => "需要處理", "SAVING" => "正在儲存", "IDLE" => "未連線", _ => connectionState
        });
        ConnectionState.SetResourceReference(TextBlock.ForegroundProperty, connectionState == "RUNNING" ? "Accent" : connectionState == "REPAIR" ? "Warning" : "TextPrimary");
        DraftState.Text = localDirty || state.GetProperty("dirty").GetBoolean() ? "有未儲存變更" : "設定已儲存";
        bool stopping = state.GetProperty("running").GetBoolean() || controls.GetProperty("stop").GetBoolean() || connectionState is "STARTING" or "STOPPING" or "RESTARTING" or "QUITTING";
        commands["start"].Visibility = stopping ? Visibility.Collapsed : Visibility.Visible;
        commands["stop"].Visibility = stopping ? Visibility.Visible : Visibility.Collapsed;
        commands["restart"].Visibility = state.GetProperty("running").GetBoolean() ? Visibility.Visible : Visibility.Collapsed;
        foreach (var (name, button) in commands)
        {
            string field = name == "restart" ? "apply" : name == "check_background" ? "background_check" : name;
            button.IsEnabled = controls.GetProperty(field).GetBoolean() || name == "save" && localDirty && !state.GetProperty("busy").GetBoolean();
        }
        hydrating = true;
        try
        {
            foreach (var button in folderButtons) button.IsEnabled = !state.GetProperty("busy").GetBoolean() && !state.GetProperty("quitting").GetBoolean();
            if (hydrate || !localDirty && !state.GetProperty("dirty").GetBoolean() && !state.GetProperty("busy").GetBoolean())
            {
                var form = state.GetProperty("form"); tunnel.Text = form.GetProperty("tunnel").GetString()!;
                hidden.IsChecked = form.GetProperty("start_hidden").GetBoolean(); autoStart.IsChecked = form.GetProperty("auto_start").GetBoolean();
                iconStyle = form.GetProperty("icon_style").GetString()!; accessMode = form.GetProperty("access_mode").GetString()!;
                icons[iconStyle].IsChecked = true; modes[accessMode].IsChecked = true;
                foreach (var (name, editor) in numbers) editor.Text = form.GetProperty("numbers").GetProperty(name).GetString()!;
                foreach (var (name, editor) in lists) editor.Text = form.GetProperty("lists").GetProperty(name).GetString()!;
                var invalid = form.GetProperty("invalid_root_ids").EnumerateArray().Select(x => x.GetString()).ToHashSet();
                var paths = form.GetProperty("roots").EnumerateArray().Select(x => x.GetProperty("path").GetString() + (invalid.Contains(x.GetProperty("id").GetString()) ? " （無法使用）" : "")).ToArray();
                if (!folders.Items.Cast<string>().SequenceEqual(paths)) folders.ItemsSource = paths;
                folderStatus.Text = $"已加入 {paths.Length} 個資料夾（最多 8 個）";
            }
        }
        finally { hydrating = false; }
        if (state.GetProperty("finished").GetBoolean()) { permitClose = true; Close(); }
    }
    private async void HandleEvent(JsonElement message)
    {
        switch (message.GetProperty("event").GetString())
        {
            case "visibility":
                if (message.GetProperty("visible").GetBoolean()) { Show(); WindowState = WindowState.Normal; Activate(); }
                else Hide();
                break;
            case "icon": ApplyIcon(message.GetProperty("style").GetString()!); break;
            case "dialog":
                bool confirm = message.GetProperty("kind").GetString() == "confirm";
                if (backend.IsFixture && ConfirmFixture is not null)
                {
                    if (confirm) await backend.Answer(message.GetProperty("token").GetString()!, ConfirmFixture(message.GetProperty("text").GetString()!));
                    break;
                }
                Opacity = 1; Show();
                var result = MessageBox.Show(this, message.GetProperty("text").GetString(), message.GetProperty("title").GetString(),
                    confirm ? MessageBoxButton.YesNo : MessageBoxButton.OK, confirm ? MessageBoxImage.Warning : MessageBoxImage.Information,
                    confirm ? MessageBoxResult.No : MessageBoxResult.OK);
                if (confirm) { try { await backend.Answer(message.GetProperty("token").GetString()!, result == MessageBoxResult.Yes); } catch (IOException) { } }
                break;
            case "fatal": status.Text = message.GetProperty("text").GetString(); permitClose = true; Opacity = 1; Show(); break;
            case "closed": permitClose = true; Close(); break;
        }
    }
    private void ApplyIcon(string style)
    {
        var filename = style == "folder_link" ? "mcp-local-v2.ico" : "mcp-local.ico";
        try { Icon = BitmapFrame.Create(new Uri(Path.Combine(backend.Root, "assets", "icons", filename))); }
        catch (IOException) { status.Text = "圖示無法載入。"; }
    }
    private void HideToTray()
    {
        if (!backend.HasNativeTray) { WindowState = WindowState.Minimized; return; }
        Hide(); if (ready) _ = Run("visibility", new { visible = false });
    }
    internal void CloseFromOwnerExit() { permitClose = true; Close(); }
    internal async void OpenMonitor()
    {
        if (detachedWindow is not null) { detachedWindow.Show(); detachedWindow.Activate(); return; }
        detachedMonitor = new MonitorView(Source, allowDetach: false);
        var layout = new DockPanel { Margin = new Thickness(28, 24, 28, 16) };
        layout.Children.Add(detachedMonitor);
        detachedWindow = new Window { Title = "MCP-Local｜請求監控", Content = layout, Width = 1120, Height = 880, MinWidth = 620, MinHeight = 480, Icon = Icon };
        DesignSystem.Window(detachedWindow);
        detachedWindow.SizeChanged += (_, _) =>
        {
            bool compact = detachedWindow.ActualWidth < 920;
            layout.Margin = new Thickness(compact ? 16 : 28, compact ? 16 : 24, compact ? 16 : 28, 16);
        };
        detachedWindow.Closed += (_, _) => { detachedMonitor?.Dispose(); detachedMonitor = null; detachedWindow = null; };
        detachedWindow.Show();
        await detachedMonitor.RestoreView(Monitor);
    }
}
