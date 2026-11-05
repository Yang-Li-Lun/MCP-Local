using System.Collections;
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Data;
using System.Windows.Documents;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Threading;
using Microsoft.Win32;

namespace McpLocal.Desktop;

internal sealed class MonitorView : UserControl, IDisposable
{
    private readonly IMonitorSource source;
    internal readonly ObservableCollection<CaptureRow> Rows = [];
    internal readonly DataGrid Listing = new()
    {
        AutoGenerateColumns = false, IsReadOnly = true, CanUserAddRows = false, CanUserDeleteRows = false,
        EnableRowVirtualization = true, EnableColumnVirtualization = true,
        HeadersVisibility = DataGridHeadersVisibility.Column, SelectionMode = DataGridSelectionMode.Single,
        SelectionUnit = DataGridSelectionUnit.FullRow, RowHeight = 40,
        GridLinesVisibility = DataGridGridLinesVisibility.None
    };
    internal readonly TextBox Query = new() { MinWidth = 50, MaxLength = 256, Margin = new Thickness(0, 0, 8, 0) };
    internal readonly ComboBox ToolFilter = new(), StatusFilter = new();
    internal readonly CheckBox Follow = new() { Content = "最新", IsChecked = true, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(16, 0, 0, 0) };
    internal readonly TextBlock SelectedTitle = new(), SelectedSummary = new();
    internal readonly TextBlock SelectedTime = new() { FontFamily = new FontFamily("Segoe UI") };
    internal readonly TextBlock SelectedDuration = new(), ExecutionState = new(), CaptureState = new();
    internal readonly Grid RecordHeader = new() { Width = 664 };
    internal readonly JsonPane Request, Response;
    internal readonly TabControl Tabs = new();
    internal readonly Button CompactSwitch = Ui.Button("");
    internal readonly Button PreviousRecord = Ui.Button("<"), NextRecord = Ui.Button(">");
    internal readonly TextBlock RecordPosition = new() { MinWidth = 48, TextAlignment = TextAlignment.Center, VerticalAlignment = VerticalAlignment.Center };
    private readonly TextBlock pageTitle = DesignSystem.Text("請求監控", "PageTitle");
    private readonly DockPanel pageHeading = new();
    private readonly TextBlock panelIcon = new() { FontFamily = new FontFamily("Segoe Fluent Icons, Segoe MDL2 Assets"), FontSize = 16, Text = "\uE8A0" };
    internal bool IsCompact { get; private set; }
    internal bool DetailsFocused { get; private set; }
    private readonly Grid main = new(), toolbar = new(), filters = new();
    internal Grid LayoutGrid => main;
    private readonly Border listSurface = new();
    private readonly DockPanel details = new();
    private readonly Grid contextBar = new();
    internal readonly DispatcherTimer UpdateTimer = new(DispatcherPriority.Background) { Interval = TimeSpan.FromMilliseconds(100) };
    private readonly DispatcherTimer filterTimer = new() { Interval = TimeSpan.FromMilliseconds(160) };
    private readonly TextBlock status = new() { VerticalAlignment = VerticalAlignment.Center };
    private readonly TextBlock notice = new() { TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(10, 0, 0, 0) };
    private readonly Button pause = Ui.Button("暫停模擬"), diagnostic = Ui.Button("診斷");
    private readonly MenuItem copyRecord, exportRecord;
    private readonly ListCollectionView view;
    private readonly Dictionary<string, CaptureRow> byId = [];
    private CancellationTokenSource? filterCancellation;
    private HashSet<string>? matches;
    private bool selecting, closed, paused;
    private int filterGeneration;
    private string sortKey = "time";
    private bool descending = true;
    private Window? diagnosticWindow;
    internal Task FilterReady { get; private set; } = Task.CompletedTask;
    internal CaptureRow? Selected => Listing.SelectedItem as CaptureRow;
    internal string NoticeText => notice.Text;
    internal MonitorSnapshot Snapshot { get; private set; }
    internal int RefreshTicks { get; private set; }
    internal Action<string> CopySink { get; set; } = text => Clipboard.SetText(text);

    public MonitorView(IMonitorSource source, bool allowDetach = true)
    {
        this.source = source;
        Snapshot = source.Snapshot;
        SetResourceReference(FontFamilyProperty, "AppFont"); FontSize = 14;
        SetResourceReference(ForegroundProperty, "TextPrimary");
        UseLayoutRounding = true;
        foreach (var control in new Control[] { Query, ToolFilter, StatusFilter, pause, diagnostic }) Ui.Outline(control);
        var root = new Grid();
        foreach (var height in new[] { GridLength.Auto, GridLength.Auto, GridLength.Auto, new GridLength(1, GridUnitType.Star), GridLength.Auto })
            root.RowDefinitions.Add(new() { Height = height });
        Content = root;
        SizeChanged += (_, _) => UpdateLayoutMode();
        var heading = pageHeading; heading.Margin = new Thickness(0, 0, 0, 16);
        CompactSwitch.Content = panelIcon; CompactSwitch.Width = 36; CompactSwitch.Padding = new Thickness(0);
        CompactSwitch.Margin = new Thickness(12, 0, 0, 0); CompactSwitch.VerticalAlignment = VerticalAlignment.Center;
        Ui.Outline(CompactSwitch); DockPanel.SetDock(CompactSwitch, Dock.Right); heading.Children.Add(CompactSwitch);
        pageTitle.Margin = new Thickness(0); heading.Children.Add(pageTitle); root.Children.Add(heading);
        toolbar.Margin = new Thickness(0, 0, 0, 8);
        foreach (var width in new[] { GridLength.Auto, new GridLength(1, GridUnitType.Star), GridLength.Auto, GridLength.Auto })
            toolbar.ColumnDefinitions.Add(new() { Width = width });
        toolbar.Children.Add(new Label { Content = "搜尋", Target = Query, VerticalAlignment = VerticalAlignment.Center, Padding = new Thickness(0, 0, 8, 0) });
        DesignSystem.Name(Query, "搜尋監控紀錄"); DesignSystem.Name(ToolFilter, "工具篩選"); DesignSystem.Name(StatusFilter, "狀態篩選");
        DesignSystem.Name(Listing, "請求紀錄");
        Ui.Place(toolbar, Query, 0, 1); Ui.Place(toolbar, pause, 0, 2);
        var more = Ui.Button("更多"); more.Margin = new Thickness(0); Ui.Outline(more); Ui.Place(toolbar, more, 0, 3); Ui.Place(root, toolbar, 1);
        Query.ToolTip = "搜尋工具、參數、已擷取內容與註記";
        var menu = new ContextMenu();
        Ui.Menu(menu, "清除紀錄", ClearRecords); Ui.Menu(menu, "重設篩選", ResetFilters);
        menu.Items.Add(new Separator());
        copyRecord = Ui.Menu(menu, "複製完整紀錄", CopyRecord);
        exportRecord = Ui.Menu(menu, "匯出選取紀錄…", ExportRecord);
        menu.Items.Add(new Separator());
        if (allowDetach) Ui.Menu(menu, "獨立視窗", () => DetachRequested?.Invoke());
        Ui.Attach(more, menu);
        filters.Margin = new Thickness(0, 0, 0, 8);
        filters.ColumnDefinitions.Add(new() { Width = new GridLength(3, GridUnitType.Star) });
        filters.ColumnDefinitions.Add(new() { Width = new GridLength(2, GridUnitType.Star) });
        filters.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        ToolFilter.Margin = new Thickness(0, 0, 8, 0);
        ToolFilter.ItemsSource = new[] { "全部工具" }.Concat(Snapshot.Records.Select(r => r.Tool).Distinct().Order()).ToArray();
        ToolFilter.SelectedIndex = 0;
        StatusFilter.ItemsSource = new[] { "全部狀態" }.Concat(JsonData.StatusNames.Values).ToArray(); StatusFilter.SelectedIndex = 0;
        filters.Children.Add(ToolFilter); Ui.Place(filters, StatusFilter, 0, 1); Ui.Place(filters, Follow, 0, 2); Ui.Place(root, filters, 2);
        Ui.Place(root, main, 3);
        CompactSwitch.Click += (_, _) => FocusDetails(!DetailsFocused);
        Listing.SetResourceReference(StyleProperty, "MonitorGrid");
        Listing.Columns.Add(new DataGridTextColumn { Header = "時間", Binding = new Binding(nameof(CaptureRow.DisplayTime)), SortMemberPath = "time", Width = 112, MinWidth = 112 });
        Listing.Columns.Add(new DataGridTextColumn { Header = "工具", Binding = new Binding("Tool"), CanUserSort = false, Width = 140 });
        Listing.Columns.Add(new DataGridTextColumn { Header = "參數摘要", Binding = new Binding("Summary"), CanUserSort = false, Width = new DataGridLength(1, DataGridLengthUnitType.Star), MinWidth = 112 });
        Listing.Columns.Add(new DataGridTextColumn { Header = "執行結果", Binding = new Binding("Outcome"), CanUserSort = false, Width = 100 });
        Listing.Columns.Add(new DataGridTextColumn { Header = "耗時", Binding = new Binding("Duration"), SortMemberPath = "duration", Width = 96 });
        Listing.Columns.Add(new DataGridTextColumn { Header = "資料擷取", Binding = new Binding("Capture"), CanUserSort = false, Width = 104 });
        var cellText = new Style(typeof(TextBlock));
        cellText.Setters.Add(new Setter(VerticalAlignmentProperty, VerticalAlignment.Center));
        cellText.Setters.Add(new Setter(MarginProperty, new Thickness(8, 0, 8, 0)));
        cellText.Setters.Add(new Setter(TextBlock.TextTrimmingProperty, TextTrimming.CharacterEllipsis));
        cellText.Setters.Add(new Setter(ToolTipProperty, new Binding("Text") { RelativeSource = new RelativeSource(RelativeSourceMode.Self) }));
        foreach (DataGridTextColumn column in Listing.Columns) column.ElementStyle = cellText;
        var timeText = new Style(typeof(TextBlock), cellText);
        timeText.Setters.Add(new Setter(TextBlock.FontFamilyProperty, new FontFamily("Segoe UI")));
        timeText.Setters.Add(new Setter(Typography.NumeralAlignmentProperty, FontNumeralAlignment.Tabular));
        timeText.Setters.Add(new Setter(MarginProperty, new Thickness(12, 0, 12, 0)));
        ((DataGridTextColumn)Listing.Columns[0]).ElementStyle = timeText;
        var outcomeText = new Style(typeof(TextBlock), cellText);
        var error = new DataTrigger { Binding = new Binding(nameof(CaptureRow.IsError)), Value = true };
        error.Setters.Add(new Setter(TextBlock.ForegroundProperty, new DynamicResourceExtension("ErrorText")));
        outcomeText.Triggers.Add(error); ((DataGridTextColumn)Listing.Columns[3]).ElementStyle = outcomeText;
        var captureText = new Style(typeof(TextBlock), cellText);
        var limited = new DataTrigger { Binding = new Binding(nameof(CaptureRow.HasCaptureWarning)), Value = true };
        limited.Setters.Add(new Setter(TextBlock.ForegroundProperty, new DynamicResourceExtension("Warning")));
        captureText.Triggers.Add(limited); ((DataGridTextColumn)Listing.Columns[5]).ElementStyle = captureText;
        view = (ListCollectionView)CollectionViewSource.GetDefaultView(Rows);
        Listing.ItemsSource = view;
        listSurface.Child = Listing; listSurface.Padding = new Thickness(0); listSurface.Margin = new Thickness(0);
        listSurface.SetResourceReference(StyleProperty, "Card");
        main.Children.Add(listSurface);
        Request = new JsonPane(Copy); Response = new JsonPane(Copy);
        Tabs.SetResourceReference(StyleProperty, "MonitorTabs");
        var requestTab = new TabItem { Header = "請求", Content = Request };
        var responseTab = new TabItem { Header = "回應", Content = Response };
        requestTab.SetResourceReference(StyleProperty, "MonitorTab"); responseTab.SetResourceReference(StyleProperty, "MonitorTab");
        Tabs.Items.Add(requestTab); Tabs.Items.Add(responseTab);
        Tabs.SelectedIndex = 1;
        var headerCells = new[] { SelectedTime, SelectedTitle, SelectedSummary, ExecutionState, SelectedDuration, CaptureState };
        RecordHeader.SetResourceReference(BackgroundProperty, "Surface");
        RecordHeader.RowDefinitions.Add(new() { Height = new GridLength(36) });
        RecordHeader.RowDefinitions.Add(new() { Height = new GridLength(Listing.RowHeight) });
        for (int i = 0; i < headerCells.Length; i++)
        {
            var column = (DataGridTextColumn)Listing.Columns[i];
            RecordHeader.ColumnDefinitions.Add(new() { Width = new GridLength(column.Width.Value, column.Width.UnitType == DataGridLengthUnitType.Star ? GridUnitType.Star : GridUnitType.Pixel), MinWidth = column.MinWidth });
            var label = new DataGridColumnHeader { Content = column.Header, Focusable = false, IsHitTestVisible = false };
            label.SetBinding(StyleProperty, new Binding(nameof(DataGrid.ColumnHeaderStyle)) { Source = Listing });
            Ui.Place(RecordHeader, label, 0, i);
            var cell = headerCells[i]; cell.FontSize = 14; cell.FontWeight = FontWeights.Normal;
            cell.TextWrapping = TextWrapping.NoWrap; cell.TextTrimming = TextTrimming.CharacterEllipsis;
            cell.VerticalAlignment = VerticalAlignment.Center; cell.Margin = new Thickness(i == 0 ? 12 : 8, 0, i == 0 ? 12 : 8, 0);
            cell.SetBinding(ToolTipProperty, new Binding(nameof(TextBlock.Text)) { Source = cell });
            Ui.Place(RecordHeader, cell, 1, i);
        }
        Typography.SetNumeralAlignment(SelectedTime, FontNumeralAlignment.Tabular);
        var headerViewport = new ScrollViewer { HorizontalScrollBarVisibility = ScrollBarVisibility.Auto,
            VerticalScrollBarVisibility = ScrollBarVisibility.Disabled, Content = RecordHeader, Margin = new Thickness(0, 0, 12, 0) };
        DesignSystem.Name(headerViewport, "當前紀錄資訊");
        headerViewport.SizeChanged += (_, _) => RecordHeader.Width = Math.Max(664, headerViewport.ActualWidth);
        contextBar.Margin = new Thickness(0, 0, 0, 12);
        contextBar.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        contextBar.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        contextBar.Children.Add(headerViewport);
        var navigation = new StackPanel { Orientation = Orientation.Horizontal, VerticalAlignment = VerticalAlignment.Center };
        foreach (var button in new[] { PreviousRecord, NextRecord }) { Ui.Outline(button); button.Width = 36; button.Padding = new Thickness(0); button.Margin = new Thickness(0); }
        DesignSystem.Name(PreviousRecord, "上一筆紀錄"); PreviousRecord.ToolTip = "上一筆紀錄";
        DesignSystem.Name(NextRecord, "下一筆紀錄"); NextRecord.ToolTip = "下一筆紀錄";
        PreviousRecord.Click += (_, _) => NavigateRecord(-1); NextRecord.Click += (_, _) => NavigateRecord(1);
        navigation.Children.Add(PreviousRecord); navigation.Children.Add(RecordPosition); navigation.Children.Add(NextRecord);
        Ui.Place(contextBar, navigation, 0, 1);
        DockPanel.SetDock(contextBar, Dock.Top); details.Children.Add(contextBar); details.Children.Add(Tabs); main.Children.Add(details);
        var footer = new DockPanel { Margin = new Thickness(0, 6, 0, 0) };
        DockPanel.SetDock(diagnostic, Dock.Right); footer.Children.Add(diagnostic);
        DockPanel.SetDock(status, Dock.Left); footer.Children.Add(status); footer.Children.Add(notice); Ui.Place(root, footer, 3);
        notice.SetBinding(ToolTipProperty, new Binding("Text") { Source = notice });
        var footerStack = new StackPanel(); root.Children.Remove(footer); footerStack.Children.Add(footer);
        Ui.Place(root, footerStack, 4);
        Listing.MouseDoubleClick += (_, e) =>
        {
            if (Selected is not null && e.OriginalSource is DependencyObject target && ItemsControl.ContainerFromElement(Listing, target) is DataGridRow)
                FocusDetails(true);
        };
        Listing.PreviewMouseLeftButtonDown += (_, e) =>
        {
            if (e.OriginalSource is DependencyObject target && ItemsControl.ContainerFromElement(Listing, target) is DataGridRow row)
            {
                Follow.IsChecked = false;
                Listing.SelectedItem = row.Item;
            }
        };
        Listing.PreviewKeyDown += (_, e) =>
        {
            if (e.Key is Key.Up or Key.Down or Key.Home or Key.End or Key.PageUp or Key.PageDown) Follow.IsChecked = false;
            if (e.Key == Key.Enter && Selected is not null) { FocusDetails(true); e.Handled = true; }
        };
        PreviewKeyDown += (_, e) => { if (DetailsFocused && e.SystemKey == Key.Left && Keyboard.Modifiers == ModifierKeys.Alt) { FocusDetails(false); e.Handled = true; } };
        Query.TextChanged += (_, _) => ScheduleFilter();
        ToolFilter.SelectionChanged += (_, _) => ScheduleFilter(); StatusFilter.SelectionChanged += (_, _) => ScheduleFilter();
        filterTimer.Tick += (_, _) => { filterTimer.Stop(); FilterReady = ApplyFilter(); };
        Listing.SelectionChanged += (_, _) =>
        {
            if (!selecting) Follow.IsChecked = false;
            ShowDetails();
        };
        Listing.Sorting += (_, e) => { e.Handled = true; Sort(e.Column.SortMemberPath); };
        Follow.Checked += (_, _) => SelectLatest();
        pause.Click += async (_, _) => {
            if (source is RemoteMonitorSource remote) { try { await remote.Toggle(); } catch (Exception error) { notice.Text = error.Message; } }
            else { paused = !paused; UpdateStatus(); }
        };
        diagnostic.Click += (_, _) => ShowDiagnostics();
        UpdateTimer.Tick += (_, _) => Advance();
        Loaded += (_, _) => { Refresh(); if (source is SyntheticSource) UpdateTimer.Start(); };
        if (source is RemoteMonitorSource live) live.HealthChanged += UpdateHealth;
    }
    private void UpdateHealth()
    {
        if (source is RemoteMonitorSource live)
        {
            notice.Text = live.HealthText; notice.ToolTip = live.HealthText;
            UpdateStatus();
        }
    }
    internal void FocusDetails(bool value)
    {
        if (value) Follow.IsChecked = false;
        DetailsFocused = value; UpdateLayoutMode();
        UpdateLayout();
        if (value) (Tabs.SelectedIndex == 0 ? Request : Response).FocusContent();
        else { if (Selected is not null) Listing.ScrollIntoView(Selected); Listing.Focus(); }
    }
    private void UpdateLayoutMode()
    {
        IsCompact = ActualWidth < 800;
        bool focused = DetailsFocused;
        pageTitle.FontSize = IsCompact ? 24 : 28;
        pageHeading.Margin = new Thickness(0, 0, 0, IsCompact ? focused ? 0 : 8 : 16);
        contextBar.Margin = new Thickness(0, 0, 0, IsCompact ? 0 : 12);
        panelIcon.Text = DetailsFocused ? "\uE8FD" : "\uE943";
        string toggleName = DetailsFocused ? "返回完整列表" : "以整頁開啟 JSON";
        CompactSwitch.ToolTip = toggleName; DesignSystem.Name(CompactSwitch, toggleName);
        CompactSwitch.IsEnabled = focused || Selected is not null;
        toolbar.Visibility = filters.Visibility = focused ? Visibility.Collapsed : Visibility.Visible;
        listSurface.Visibility = focused ? Visibility.Collapsed : Visibility.Visible;
        details.Visibility = DetailsFocused ? Visibility.Visible : Visibility.Collapsed;
    }
    internal void NavigateRecord(int direction)
    {
        int index = Selected is null ? -1 : view.IndexOf(Selected);
        int next = index + Math.Sign(direction);
        if (index < 0 || direction == 0 || next < 0 || next >= view.Count) return;
        Follow.IsChecked = false; Select((CaptureRow)view.GetItemAt(next), false);
    }
    internal event Action? DetachRequested;
    internal async Task RestoreView(MonitorView previous)
    {
        Refresh(); Follow.IsChecked = false;
        Query.Text = previous.Query.Text; ToolFilter.SelectedItem = previous.ToolFilter.SelectedItem;
        StatusFilter.SelectedItem = previous.StatusFilter.SelectedItem;
        sortKey = previous.sortKey; descending = previous.descending;
        view.CustomSort = new RowComparer(sortKey, descending);
        await ApplyFilter();
        if (previous.Selected is { } selected && byId.TryGetValue(selected.Id, out var row)) Select(row, true);
        Follow.IsChecked = previous.Follow.IsChecked;
    }
    public void Dispose()
    {
        if (closed) return;
        closed = true; UpdateTimer.Stop(); filterTimer.Stop(); filterCancellation?.Cancel();
        filterCancellation?.Dispose(); Request.Dispose(); Response.Dispose(); diagnosticWindow?.Close();
        if (source is SyntheticSource) source.Dispose();
        if (source is RemoteMonitorSource live) live.HealthChanged -= UpdateHealth;
    }

    internal void Advance()
    {
        if (paused || closed) return;
        source.Advance(); Refresh();
    }
    internal void Refresh()
    {
        RefreshTicks++;
        Snapshot = source.Snapshot;
        bool changed = false;
        var wanted = Snapshot.Records.Select(r => r.Id).ToHashSet();
        var selectedId = Selected?.Id;
        selecting = true;
        // Collection changes must reach ListCollectionView immediately. DeferRefresh
        // is for view-property changes, not ObservableCollection mutations.
        {
            foreach (var stale in Rows.Where(r => !wanted.Contains(r.Id)).ToArray())
            { Rows.Remove(stale); byId.Remove(stale.Id); changed = true; }
            foreach (var record in Snapshot.Records)
            {
                if (byId.TryGetValue(record.Id, out var existing))
                {
                    changed |= !ReferenceEquals(existing.Record.Request, record.Request)
                        || !ReferenceEquals(existing.Record.Response, record.Response) || existing.Record.Status != record.Status
                        || !existing.Record.RequestNotes.SequenceEqual(record.RequestNotes)
                        || !existing.Record.ResponseNotes.SequenceEqual(record.ResponseNotes) || existing.Record.Revision != record.Revision;
                    existing.Update(record);
                }
                else { var row = new CaptureRow(record); byId.Add(record.Id, row); Rows.Add(row); changed = true; }
            }
            if (view.CustomSort is null || sortKey == "duration") view.CustomSort = new RowComparer(sortKey, descending);
        }
        selecting = false;
        var tools = new[] { "全部工具" }.Concat(Snapshot.Records.Select(r => r.Tool).Distinct().Order()).ToArray();
        if (!ToolFilter.Items.Cast<string>().SequenceEqual(tools))
        {
            var previous = ToolFilter.SelectedItem as string;
            ToolFilter.ItemsSource = tools;
            ToolFilter.SelectedItem = previous is not null && tools.Contains(previous) ? previous : tools[0];
        }
        if (changed && HasFilter) { FilterReady = ApplyFilter(); }
        else RestoreSelection(selectedId);
        UpdateStatus();
    }
    private bool HasFilter => Query.Text.Trim().Length > 0 || ToolFilter.SelectedIndex > 0 || StatusFilter.SelectedIndex > 0;
    private void ScheduleFilter() { filterTimer.Stop(); filterTimer.Start(); }
    internal async Task ApplyFilter()
    {
        filterTimer.Stop(); filterCancellation?.Cancel(); filterCancellation?.Dispose();
        filterCancellation = new CancellationTokenSource(); var token = filterCancellation.Token;
        int version = ++filterGeneration;
        var query = Query.Text.Trim();
        var tool = ToolFilter.SelectedIndex > 0 ? (string)ToolFilter.SelectedItem : "";
        var state = StatusFilter.SelectedIndex > 0 ? JsonData.StatusNames.First(x => x.Value == (string)StatusFilter.SelectedItem).Key : "";
        var snapshot = Snapshot.Records.ToArray();
        try
        {
            var accepted = source is RemoteMonitorSource remote ? await remote.Search(query, tool, state, token) : query.Length == 0
                ? snapshot.Where(r => JsonData.Matches(r, query, tool, state, token)).Select(r => r.Id).ToHashSet()
                : await Task.Run(() => snapshot.Where(r => JsonData.Matches(r, query, tool, state, token)).Select(r => r.Id).ToHashSet(), token);
            if (closed || token.IsCancellationRequested || version != filterGeneration) return;
            var selected = Selected?.Id;
            matches = accepted;
            selecting = true;
            view.Filter = query.Length > 0 || tool.Length > 0 || state.Length > 0
                ? value => value is CaptureRow row && matches.Contains(row.Id) : null;
            selecting = false;
            RestoreSelection(selected); UpdateStatus();
        }
        catch (OperationCanceledException) { }
        catch (Exception error) { if (!closed && version == filterGeneration) notice.Text = error.Message; }
    }
    private void RestoreSelection(string? selectedId)
    {
        if (Follow.IsChecked == true) SelectLatest();
        else if (selectedId is not null && byId.TryGetValue(selectedId, out var row) && view.Contains(row)) Select(row, false);
        else { Select(null, false); if (selectedId is not null) notice.Text = byId.ContainsKey(selectedId) ? "紀錄已被篩選隱藏" : "紀錄已淘汰或清除"; }
        ShowDetails();
    }
    internal void Select(CaptureRow? row, bool scroll)
    {
        selecting = true; Listing.SelectedItem = row; selecting = false;
        if (scroll && row is not null) Listing.ScrollIntoView(row);
        ShowDetails();
    }
    internal void SelectLatest()
    {
        var latest = view.Cast<CaptureRow>().MaxBy(r => r.Record.Sequence);
        // Unchanged latest rows do not trigger native scrolling on every timer tick.
        Select(latest, !ReferenceEquals(latest, Selected));
    }
    internal void Sort(string key)
    {
        descending = sortKey == key ? !descending : true; sortKey = key;
        var selected = Selected?.Id;
        selecting = true; view.CustomSort = new RowComparer(sortKey, descending); selecting = false;
        foreach (var column in Listing.Columns)
            column.SortDirection = column.SortMemberPath == key ? descending ? ListSortDirection.Descending : ListSortDirection.Ascending : null;
        RestoreSelection(selected);
    }
    internal void ResetFilters() { Query.Clear(); ToolFilter.SelectedIndex = 0; StatusFilter.SelectedIndex = 0; FilterReady = ApplyFilter(); }
    internal void ClearRecords() { source.Clear(); Refresh(); notice.Text = "已送出清除請求"; }
    private void ShowDetails()
    {
        var row = Selected?.Record;
        SelectedTitle.Text = row?.Tool ?? "";
        SelectedTime.Text = Selected?.DisplayTime ?? "";
        SelectedDuration.Text = Selected?.Duration ?? "";
        SelectedSummary.Text = Selected?.Summary ?? "";
        ExecutionState.Text = Selected?.Outcome ?? "";
        ExecutionState.SetResourceReference(TextBlock.ForegroundProperty, Selected?.IsError == true ? "ErrorText" : "TextSecondary");
        CaptureState.Text = Selected?.Capture ?? "";
        CaptureState.SetResourceReference(TextBlock.ForegroundProperty, Selected?.Capture is "部分內容" or "內容缺失" ? "Warning" : "TextSecondary");
        int index = Selected is null ? -1 : view.IndexOf(Selected);
        RecordPosition.Text = $"{index + 1} / {view.Count}";
        PreviousRecord.IsEnabled = index > 0; NextRecord.IsEnabled = index >= 0 && index + 1 < view.Count;
        UpdateLayoutMode();
        if (row is not null && notice.Text is "紀錄已被篩選隱藏" or "紀錄已淘汰或清除")
            notice.Text = "";
        if (row is not null && !row.DetailsLoaded && source is RemoteMonitorSource remote)
        {
            Request.SetPayload(row.Request, row.RequestNotes, "正在載入…"); Response.SetPayload(row.Response, row.ResponseNotes, "正在載入…");
            copyRecord.IsEnabled = exportRecord.IsEnabled = false;
            LoadRemoteDetail(remote, row.Id, row.Revision);
            return;
        }
        Request.SetPayload(row?.Request, row?.RequestNotes ?? [], row is null ? "" : "未收集到請求內容。");
        Response.SetPayload(row?.Response, row?.ResponseNotes ?? [], row is null ? "" : row.Status == "running" ? "等待執行結果。" : "未收集到回應內容。");
        copyRecord.IsEnabled = exportRecord.IsEnabled = row is not null;
    }
    private void UpdateStatus()
    {
        status.Text = $"{(paused ? "已暫停" : Snapshot.Diagnostics.State)} · {view.Count} / {Rows.Count} 筆";
        pause.Content = source is RemoteMonitorSource remote ? remote.Enabled ? "暫停監控" : "恢復監控" : paused ? "恢復模擬" : "暫停模擬";
        pause.IsEnabled = source is not RemoteMonitorSource live || live.Connected && !live.Failed;
        diagnostic.Content = Snapshot.Diagnostics.State == "監控異常" || Snapshot.Diagnostics.EvictedRecords > 0 || Snapshot.Diagnostics.LostEvents > 0
            || Rows.Any(r => r.Record.Partial || r.Record.RequestNotes.Length + r.Record.ResponseNotes.Length > 0) ? "診斷 !" : "診斷";
    }
    private void Copy(string text)
    {
        try { CopySink(text); notice.Text = "已複製"; }
        catch (Exception) { notice.Text = "複製失敗；請稍後重試。"; }
    }
    internal async void CopyRecord()
    {
        var row = Selected?.Record; var diagnostics = Snapshot.Diagnostics;
        if (row is null) return;
        try
        {
            var text = await Task.Run(() => JsonData.Export(row, diagnostics));
            if (!closed) Copy(text);
        }
        catch (Exception) { if (!closed) notice.Text = "複製未完成；請重新選取紀錄後再試。"; }
    }
    private async void ExportRecord()
    {
        // Capture selection before asynchronous formatting or the native modal loop.
        var row = Selected?.Record; var diagnostics = Snapshot.Diagnostics;
        if (row is null) return;
        try
        {
            var text = await Task.Run(() => JsonData.Export(row, diagnostics));
            if (closed) return;
            var dialog = NewSaveDialog("匯出選取紀錄（含已擷取原始內容）", "mcp-request.json");
            if (dialog.ShowDialog(Window.GetWindow(this)) != true) return;
            JsonData.WriteNew(dialog.FileName, text); notice.Text = "已匯出選取紀錄";
        }
        catch (Exception) { notice.Text = "匯出未完成；不覆寫既有檔案。若寫入中斷請檢查新檔。"; }
    }
    internal static SaveFileDialog NewSaveDialog(string title, string name) => new()
    { Title = title, FileName = name, Filter = "JSON (*.json)|*.json", DefaultExt = ".json", AddExtension = true, OverwritePrompt = true };
    internal string DiagnosticsText() => JsonSerializer.Serialize(new
    {
        synthetic_only = source is SyntheticSource, source = source.GetType().Name, connected = source is RemoteMonitorSource live && live.Connected,
        records = Rows.Count, visible = view.Count, selected_id = Selected?.Id,
        lost_events = Snapshot.Diagnostics.LostEvents, evicted_records = Snapshot.Diagnostics.EvictedRecords,
        partial_records = Rows.Count(r => r.Record.Partial || r.Record.RequestNotes.Length + r.Record.ResponseNotes.Length > 0),
        scope = "監控歷程累計，非選取紀錄專屬。", elapsed_unknown = Rows.Count(r => r.Elapsed is null),
        request_notes = Selected?.Record.RequestNotes, response_notes = Selected?.Record.ResponseNotes,
        capture = "未提供、截斷與省略的內容無法還原。",
        framework = System.Runtime.InteropServices.RuntimeInformation.FrameworkDescription,
        frontend = BuildIdentity.Current,
        backend = source is RemoteMonitorSource remote && remote.BackendIdentity.ValueKind == JsonValueKind.Object ? (object)remote.BackendIdentity : "未知",
        monitor_last_success = source is RemoteMonitorSource health ? health.LastSuccess?.ToString("O") ?? "尚無" : "不適用",
        backend_query_error = source is RemoteMonitorSource query ? query.QueryError : null,
        dpi_scale = VisualTreeHelper.GetDpi(this).DpiScaleX,
        capture_source = "Python 記憶體監控；沒有擷取到的內容不能還原。"
    }, JsonData.PrettyOptions);
    private async void ShowDiagnostics()
    {
        if (diagnosticWindow is not null) { diagnosticWindow.Activate(); return; }
        if (source is RemoteMonitorSource remote)
        {
            try { await remote.RefreshIdentity(); }
            catch (Exception error) { remote.SetHealth(error.Message); }
        }
        if (closed || diagnosticWindow is not null) return;
        var pane = new TextBox { Text = DiagnosticsText(), IsReadOnly = true, TextWrapping = TextWrapping.Wrap,
            VerticalScrollBarVisibility = ScrollBarVisibility.Auto, Margin = new Thickness(10) };
        diagnosticWindow = new Window { Owner = Window.GetWindow(this), Title = "監控診斷", Width = 610, Height = 560, Content = pane };
        DesignSystem.Window(diagnosticWindow);
        diagnosticWindow.Closed += (_, _) => diagnosticWindow = null;
        diagnosticWindow.Show();
    }
    private async void LoadRemoteDetail(RemoteMonitorSource remote, string id, long revision)
    {
        try { if (await remote.EnsureDetails(id, revision) && !closed && Selected?.Id == id) Refresh(); }
        catch (Exception error) { if (!closed) notice.Text = error.Message; }
    }

    private sealed class RowComparer(string key, bool reverse) : IComparer
    {
        public int Compare(object? x, object? y)
        {
            var a = ((CaptureRow)x!).Record; var b = ((CaptureRow)y!).Record;
            if (key == "duration" && (a.ElapsedMs is null || b.ElapsedMs is null))
                return a.ElapsedMs is null ? b.ElapsedMs is null ? a.Sequence.CompareTo(b.Sequence) : 1 : -1;
            int result = key == "duration" ? Nullable.Compare(a.ElapsedMs, b.ElapsedMs) : a.Time.CompareTo(b.Time);
            if (result == 0) result = a.Sequence.CompareTo(b.Sequence);
            return reverse ? -result : result;
        }
    }
}
