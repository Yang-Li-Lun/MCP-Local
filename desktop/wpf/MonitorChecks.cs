using System.Windows;
using System.IO;
using System.Text.Json;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;

namespace McpLocal.Desktop;

// Runs the actual WPF controls on their STA dispatcher, with synthetic snapshots
// and an injected copy sink. No system clipboard or native Save dialog in this suite.
internal static class MonitorChecks
{
    public static async Task Run(MonitorView window, string report)
    {
        var host = Window.GetWindow(window);
        window.UpdateTimer.Stop();
        var checks = new List<string>();
        void Check(bool result, string name)
        { if (!result) throw new InvalidOperationException(name); checks.Add(name); }
        await window.Response.Ready;
        Check(window.Rows.Count == 200 && window.Selected?.Id == "199", "initial 200 records and latest selection");
        window.UpdateLayout();
        Check(window.Listing.EnableRowVirtualization && window.Listing.EnableColumnVirtualization &&
            DesignChecks.Children<DataGridRow>(window.Listing).Count() < 40, "200 records retain bounded realized rows and virtualization");
        Check(window.SelectedSummary.Text == window.Selected!.Summary, "selected parameter context matches current record");
        Check(!window.DetailsFocused && !window.Tabs.IsVisible &&
            Math.Abs(window.Listing.ActualWidth - window.LayoutGrid.ActualWidth) < 1,
            "monitor defaults to full-width list with JSON panel collapsed");
        Check(window.CompactSwitch.Content is TextBlock { Text.Length: 1 } && window.CompactSwitch.IsVisible,
            "JSON panel toggle uses an icon without a visible text label");
        Check(window.Listing.Columns.All(c => c.Visibility == Visibility.Visible) && window.Selected!.Time.Length == 8,
            "all six columns remain available and time displays to seconds");
        var selectedRow = (DataGridRow)window.Listing.ItemContainerGenerator.ContainerFromItem(window.Selected);
        var cells = DesignChecks.Children<DataGridCell>(selectedRow).ToArray();
        Check(cells.Length == 6 && cells.All(c =>
        {
            var hit = c.InputHitTest(new Point(2, 2)) as DependencyObject;
            return hit is not null && ItemsControl.ContainerFromElement(window.Listing, hit) is DataGridRow;
        }), "blank cell padding remains a hit target across all six columns");
        var clickedCell = cells[0];
        clickedCell.RaiseEvent(new MouseButtonEventArgs(Mouse.PrimaryDevice, 0, MouseButton.Left)
            { RoutedEvent = Mouse.PreviewMouseDownEvent });
        Check(window.Follow.IsChecked == false, "clicking the already-selected row disables follow immediately");
        for (int i = 0; i < 4; i++) window.Refresh();
        Check(window.Selected?.Id == "199", "refresh preserves manually pinned row selection");
        var errorRow = (DataGridRow)window.Listing.ItemContainerGenerator.ContainerFromItem(window.Rows.First(r => r.Id == "187"));
        var errorCell = DesignChecks.Children<DataGridCell>(errorRow).Single(c => c.Column == window.Listing.Columns[3]);
        Check(DesignChecks.Children<TextBlock>(errorCell).Single().Foreground == window.FindResource("ErrorText"),
            "execution error text uses the red error brush");
        window.FocusDetails(true); window.UpdateLayout();
        Check(!window.Listing.IsVisible && window.Tabs.IsVisible &&
            Math.Abs(window.Tabs.ActualWidth - window.LayoutGrid.ActualWidth) < 1,
            "opening JSON uses the complete content width");
        Check(window.Listing.RowHeight == 40 && DesignChecks.Children<DataGridRow>(window.Listing).All(r => Math.Abs(r.ActualHeight - 40) < 0.1),
            "realized rows use readable 40 DIP height");
        Check(window.SelectedTime.IsVisible && window.SelectedTime.Text == window.Selected!.DisplayTime &&
            window.SelectedSummary.IsVisible && window.ExecutionState.IsVisible && window.CaptureState.IsVisible,
            "full-page header retains current row time, tool, summary, outcome and capture state");
        Check(window.CompactSwitch.TranslatePoint(new Point(0, 0), window).X > window.ActualWidth - 60 &&
            window.CompactSwitch.TranslatePoint(new Point(0, 0), window).Y < 50,
            "JSON toggle is positioned at the top right");
        Check(!window.PreviousRecord.IsEnabled && window.NextRecord.IsEnabled, "first record disables previous navigation");
        window.NextRecord.RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
        await window.Response.Ready;
        Check(window.Selected?.Id == "198" && window.SelectedSummary.Text == window.Selected.Summary &&
            window.Response.ContentText.Contains("demo/a.txt") && window.RecordPosition.Text == "2 / 200",
            "next record updates selected header and JSON together");
        window.PreviousRecord.RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
        Check(window.Selected?.Id == "199", "previous record returns in current list order");
        window.FocusDetails(false); window.UpdateLayout();
        Check(!window.Tabs.IsVisible && window.Listing.IsVisible, "return icon restores complete list width");
        var timeText = DesignChecks.Children<TextBlock>(DesignChecks.Children<DataGridCell>(
            (DataGridRow)window.Listing.ItemContainerGenerator.ContainerFromItem(window.Selected)).First()).Single();
        Check(timeText.FontFamily.Source == "Segoe UI" && timeText.Text.Contains('\u2009') &&
            window.Listing.Columns[0].ActualWidth >= 112,
            "time uses tabular digits with moderate spacing");
        var timeMeasure = new FormattedText(timeText.Text, System.Globalization.CultureInfo.CurrentUICulture,
            FlowDirection.LeftToRight, new Typeface(timeText.FontFamily, timeText.FontStyle, timeText.FontWeight, timeText.FontStretch),
            timeText.FontSize, Brushes.Black, VisualTreeHelper.GetDpi(timeText).PixelsPerDip);
        Check(timeMeasure.WidthIncludingTrailingWhitespace + timeText.Margin.Left + timeText.Margin.Right <= window.Listing.Columns[0].ActualWidth,
            "entire timestamp fits without ellipsis after adding spacing");
        var tightTime = new FormattedText(window.Selected!.Time, System.Globalization.CultureInfo.CurrentUICulture,
            FlowDirection.LeftToRight, new Typeface("Segoe UI"), 14, Brushes.Black, 1);
        var wideTime = new FormattedText(window.Selected.DisplayTime, System.Globalization.CultureInfo.CurrentUICulture,
            FlowDirection.LeftToRight, new Typeface("Consolas"), 14, Brushes.Black, 1);
        Check(timeMeasure.WidthIncludingTrailingWhitespace > tightTime.WidthIncludingTrailingWhitespace &&
            timeMeasure.WidthIncludingTrailingWhitespace < wideTime.WidthIncludingTrailingWhitespace,
            "timestamp spacing falls between original compact and previous wide displays");
        window.CompactSwitch.RaiseEvent(new RoutedEventArgs(Button.ClickEvent)); window.UpdateLayout();
        Check(window.DetailsFocused && !window.Listing.IsVisible, "top-right icon reopens full-page JSON");
        var headerCells = new[] { window.SelectedTime, window.SelectedTitle, window.SelectedSummary,
            window.ExecutionState, window.SelectedDuration, window.CaptureState };
        Check(headerCells.Select((cell, index) => Grid.GetColumn(cell) == index &&
            Math.Abs(cell.TranslatePoint(new Point(0, cell.ActualHeight / 2), window.RecordHeader).Y - 56) < 1).All(value => value),
            "detail information stays in one row in the same column order as the list");
        Check(window.RecordHeader.Children.OfType<System.Windows.Controls.Primitives.DataGridColumnHeader>()
            .Select(h => h.Content).SequenceEqual(window.Listing.Columns.Select(c => c.Header)),
            "JSON detail renders all six list column headings");
        await window.Response.Ready;
        Check(window.Response.ContentText.Length > 1048000, "full large document retained");
        Check(window.Response.Reader.Text.Length <= 4096 && window.Response.PageCount > 200, "bounded display pages");
        int initialLoads = window.Response.DocumentLoads;
        var initialRecord = window.Selected!.Record;
        window.Response.SetPayload(initialRecord.Response, [new string('註', 6000)], "未收集到回應內容。");
        Check(!window.Response.CaptureDetails.IsExpanded && window.Response.CaptureDetails.IsVisible, "capture notes initially collapsed and discoverable");
        window.Response.CaptureDetails.IsExpanded = true;
        window.UpdateLayout();
        Check(window.Response.CaptureNotice.ActualHeight <= 72 && window.Response.CaptureNotice.ScrollableHeight > 0 && window.Response.ColoredReader.ActualHeight >= 48,
            "long capture notes remain scrollable without consuming JSON reading area");
        window.Response.SetPayload(initialRecord.Response, initialRecord.ResponseNotes, "未收集到回應內容。");
        window.Response.CaptureDetails.IsExpanded = false;
        Check(window.Response.DocumentLoads == initialLoads, "capture-note layout does not reload payload");
        window.Response.Mode.SelectedIndex = 0;
        Check(new System.Windows.Documents.TextRange(window.Response.ColoredReader.Document.ContentStart,
            window.Response.ColoredReader.Document.ContentEnd).Text.TrimEnd('\r', '\n') == window.Response.Reader.Text,
            "default colored JSON retains the exact bounded page");
        var document = window.Response.ColoredReader.Document;
        Check(document.FontFamily.Source == "Consolas" && document.FontSize == 14 &&
            document.Blocks.FirstBlock is System.Windows.Documents.Paragraph { LineHeight: 21 },
            "colored JSON uses readable monospace type and line spacing");
        Check(JsonColor.Scan("{\"key\":\"escaped\\\"text\",\"n\":-12.5,\"ok\":true}").Select(s => s.Kind).Distinct().Count() == 4,
            "JSON keys, escaped strings, numbers and literals colored without changing values");
        window.Response.Mode.SelectedIndex = 0;
        window.Response.Turn(1);
        Check(window.Response.PageIndex == 1, "next page");
        window.Response.SetWrap(true);
        Check(window.Response.Reader.Text.Length <= 2048 && window.Response.PageIndex > 0, "wrap preserves reading offset");
        int loads = window.Request.DocumentLoads + window.Response.DocumentLoads;
        int page = window.Response.PageIndex;
        window.FocusDetails(false);
        host.Width = 650; host.Height = 480; await Task.Delay(50);
        Check(window.IsCompact && window.Listing.IsVisible && !window.Tabs.IsVisible,
            "narrow monitor shows full-width records with detail switch");
        window.Listing.RaiseEvent(new KeyEventArgs(Keyboard.PrimaryDevice, PresentationSource.FromVisual(window.Listing), 0, Key.Enter)
            { RoutedEvent = Keyboard.PreviewKeyDownEvent });
        window.UpdateLayout();
        Check(window.Tabs.IsVisible && !window.Listing.IsVisible && Math.Abs(window.Tabs.ActualWidth - window.LayoutGrid.ActualWidth) < 1,
            "Enter opens full-width compact JSON details");
        window.CompactSwitch.RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
        window.UpdateLayout();
        Check(window.Listing.IsVisible && !window.Tabs.IsVisible,
            "compact return switch restores full-width records");
        host.Width = 1120; await Task.Delay(50);
        Check(!window.IsCompact && window.Listing.IsVisible && !window.Tabs.IsVisible,
            "wide short window preserves collapsed JSON panel");
        window.FocusDetails(true); window.UpdateLayout();
        host.Width = 1120; host.Height = 880; await Task.Delay(50);
        Check(window.DetailsFocused && !window.Listing.IsVisible && window.Tabs.IsVisible,
            "resizing preserves full-page JSON mode");
        Check(loads == window.Request.DocumentLoads + window.Response.DocumentLoads && page == window.Response.PageIndex,
            "resize does not reload details or reset page");
        window.Listing.SelectedItem = window.Rows.First(r => r.Id == "198");
        Check(window.Follow.IsChecked == false, "history selection disables follow");
        await window.Response.Ready;
        loads = window.Response.DocumentLoads;
        window.Advance();
        Check(window.Selected?.Id == "198" && window.Response.DocumentLoads == loads, "unselected updates preserve history detail");
        window.Sort("duration");
        var sorted = window.Listing.Items.Cast<CaptureRow>().ToArray();
        Check(sorted[^1].Elapsed is null && sorted[0].Elapsed >= sorted[1].Elapsed, "numeric duration descending, unknown last");
        window.Sort("duration"); sorted = window.Listing.Items.Cast<CaptureRow>().ToArray();
        Check(sorted[^1].Elapsed is null && sorted[0].Elapsed <= sorted[1].Elapsed, "numeric duration ascending, unknown last");
        window.Select(sorted[0], false); window.NavigateRecord(1);
        Check(window.Selected == sorted[1], "record navigation follows numeric sort order");
        window.Select(sorted[^1], false); window.NavigateRecord(1);
        Check(window.Selected == sorted[^1] && !window.NextRecord.IsEnabled,
            "last record disables next navigation without wrapping");
        window.Follow.IsChecked = true;
        Check(window.Selected?.Id == "199", "follow latest independent of sorting");
        window.ToolFilter.SelectedItem = "list_directory"; await window.ApplyFilter();
        Check(window.Listing.Items.Count > 0 && window.Listing.Items.Cast<CaptureRow>().All(r => r.Tool == "list_directory"), "tool filter");
        var filtered = window.Listing.Items.Cast<CaptureRow>().ToArray();
        window.Select(filtered[0], false); window.NavigateRecord(1);
        Check(window.Selected == filtered[1] && window.Selected.Tool == "list_directory" &&
            window.RecordPosition.Text == $"2 / {filtered.Length}", "record navigation stays within filtered results");
        window.Follow.IsChecked = true;
        window.ResetFilters(); await window.FilterReady;
        window.StatusFilter.SelectedItem = "執行中"; await window.ApplyFilter();
        Check(window.Listing.Items.Count == 4, "status filter");
        await window.Response.Ready;
        Check(!window.Response.Available && window.Response.ContentText == "", "missing response not fabricated as null");
        window.ResetFilters(); await window.FilterReady;
        window.Query.Text = "資料/198.txt"; await window.ApplyFilter();
        Check(window.Listing.Items.Count == 1 && window.Selected?.Id == "198", "decoded Unicode request search");
        window.Query.Text = "合成截斷註記"; await window.ApplyFilter();
        Check(window.Listing.Items.Count > 0, "capture notes searchable");
        window.Query.Text = "no-such-synthetic-value"; await window.ApplyFilter();
        Check(window.Listing.Items.Count == 0 && window.Selected is null && !window.Response.Available, "empty filter clears stale details");
        Check(!window.PreviousRecord.IsEnabled && !window.NextRecord.IsEnabled && window.RecordPosition.Text == "0 / 0",
            "empty results disable both record navigation actions");
        Check(window.SelectedSummary.Text.Length == 0, "empty filter also clears parameter context");
        window.Query.Text = "x"; var staleSearch = window.ApplyFilter();
        window.Query.Text = "資料/198.txt"; await window.ApplyFilter(); await staleSearch;
        Check(window.Listing.Items.Count == 1 && window.Selected?.Id == "198", "stale background search discarded");
        window.ResetFilters(); await window.FilterReady;
        window.Listing.SelectedItem = window.Rows.First(r => r.Id == "198");
        window.Follow.IsChecked = false;
        window.Query.Text = "no-such-synthetic-value"; await window.ApplyFilter();
        Check(window.Selected is null && window.NoticeText == "紀錄已被篩選隱藏", "hidden history selection explains missing details");
        window.ResetFilters(); await window.FilterReady;
        window.Listing.SelectedItem = window.Rows.First(r => r.Id == "198");
        Check(window.Selected?.Id == "198" && window.NoticeText == "", "reselected visible record clears obsolete selection notice");
        await window.Response.Ready;
        window.Response.Mode.SelectedIndex = 1;
        var root = (TreeViewItem)window.Response.Fields.Items[0];
        var items = (TreeViewItem)root.Items[0]; items.IsExpanded = true; window.Response.Expand(items);
        Check(items.Items.Count <= 201 && items.Items.Count < 6000, "large field arrays expand in bounded batches");
        var first = (TreeViewItem)items.Items[0]; first.IsSelected = true;
        window.Response.ReadSelectedField(); await window.Response.Ready;
        Check(window.Response.ContentText.Contains("demo/a.txt") && window.Response.ContentText.Length < 200, "read selected field scope");
        window.Response.ResetScope(); await window.Response.Ready;
        Check(window.Response.ContentText.Length > 100000, "return to complete JSON");
        window.Response.Mode.SelectedIndex = 0;
        window.Response.Search.Text = "demo/a.txt";
        int hit1 = window.Response.Find(1), hit2 = window.Response.Find(1), hit3 = window.Response.Find(-1);
        Check(hit2 > hit1 && hit3 == hit1, "forward and backward full document search");
        Check(window.Response.ColoredReader.Selection.Text == "demo/a.txt", "search selects matching text in colored JSON");
        Check(window.Response.Mode.Items.Count == 2 && window.Response.ColoredReader.Visibility == Visibility.Visible,
            "JSON defaults to colored view with field view available");
        window.Response.Search.Text = "not-present";
        Check(window.Response.Find(1) == -1 && window.Response.Feedback.Text == "找不到", "search feedback");
        var unicode = Payload.From(new { value = new string('a', 2040) + "😀台灣" + new string('b', 5000) });
        window.Response.SetPayload(unicode, [], ""); await window.Response.Ready;
        for (int i = 0; i < window.Response.PageCount; i++)
        {
            var text = window.Response.Reader.Text;
            Check(!char.IsHighSurrogate(text[^1]) && !char.IsLowSurrogate(text[0]), $"Unicode page {i + 1} intact");
            window.Response.Turn(1);
        }
        window.Response.SetPayload(Payload.From(new { ok = true }), [], "");
        var firstLoad = window.Response.Ready;
        window.Response.SetPayload(new Payload("null"), [], ""); await window.Response.Ready; await firstLoad;
        Check(window.Response.ContentText == "null" && window.Response.Available, "stale formatting discarded; explicit JSON null retained");
        string? copied = null;
        window.CopySink = text => copied = text;
        window.CopyRecord();
        for (int i = 0; i < 100 && copied is null; i++) await Task.Delay(10);
        Check(copied is not null && JsonDocument.Parse(copied).RootElement.GetProperty("record").GetProperty("id").GetString() == "198", "copy complete selected record via injected sink");
        string exportPath = Path.Combine(Path.GetDirectoryName(report)!, Path.GetFileNameWithoutExtension(report) + "-export.json");
        JsonData.WriteNew(exportPath, copied!);
        bool refused = false;
        try { JsonData.WriteNew(exportPath, "overwrite"); } catch (IOException) { refused = true; }
        Check(refused && File.ReadAllText(exportPath).Contains("synthetic_only"), "export round trip and no overwrite");
        Check(window.DiagnosticsText().Contains("SyntheticSource"), "diagnostic source identified");
        window.Follow.IsChecked = true;
        for (int i = 0; i < 20; i++) window.Advance();
        Check(window.Selected?.Record.Sequence > 199 && window.Rows.Count == 200, "follow incoming records with rolling history");
        Check(window.Snapshot.Diagnostics.EvictedRecords > 0, "eviction diagnostics counted");
        window.Listing.SelectedItem = window.Rows[0];
        string oldest = window.Selected!.Id;
        for (int i = 0; i < 20; i++) window.Advance();
        Check(window.Selected is null && !window.Rows.Any(r => r.Id == oldest), "evicted selection clears details");
        window.ClearRecords();
        Check(window.Rows.Count == 0 && window.Selected is null && !window.Response.Available, "clear synthetic history");
        await CheckSnapshotUpdates(window, Check);
        await CheckJsonScrolling(window, Check, report);
        JsonData.WriteNew(report, JsonSerializer.Serialize(new { state = "PASS", checks, count = checks.Count,
            synthetic_only = true, native_clipboard = "NOT_RUN", native_save_dialog = "NOT_RUN",
            human_resize_acceptance = "NOT_RUN" }, JsonData.PrettyOptions));
    }

    private sealed class SnapshotSource(CaptureRecord initial) : IMonitorSource
    {
        public CaptureRecord Record = initial;
        public MonitorSnapshot Snapshot => new([Record], new(0, 0, "合成快照測試"));
        public void Advance() { }
        public void Clear() { }
        public void Dispose() { }
    }

    private static async Task CheckJsonScrolling(MonitorView window, Action<bool, string> check, string report)
    {
        window.FocusDetails(true); window.Tabs.SelectedIndex = 1;
        var pane = window.Response;
        pane.SetWrap(false); pane.SetPayload(Payload.From(new { ok = true }), [], ""); await pane.Ready;
        window.UpdateLayout(); await Task.Delay(50);
        check(pane.ColoredReader.ExtentWidth <= pane.ColoredReader.ViewportWidth + 1,
            "short JSON has no artificial horizontal scroll range");
        pane.SetPayload(Payload.From(new { text = new string('x', 250) + "台灣😀" }), [], ""); await pane.Ready;
        window.UpdateLayout(); await Task.Delay(50);
        check(pane.ColoredReader.ExtentWidth > pane.ColoredReader.ViewportWidth + 100 && pane.ColoredReader.ExtentWidth < 3000,
            "long JSON line scroll range follows measured text width");
        var horizontal = DesignChecks.Children<System.Windows.Controls.Primitives.ScrollBar>(pane.ColoredReader)
            .Single(bar => bar.Orientation == Orientation.Horizontal && bar.IsVisible);
        double bottom = horizontal.TranslatePoint(new Point(0, horizontal.ActualHeight), pane.ColoredReader).Y;
        check(Math.Abs(bottom - pane.ColoredReader.ActualHeight) < 2,
            "horizontal scrollbar stays at bottom of full reading viewport for short content");
        DesignChecks.Capture(Window.GetWindow(window), Path.Combine(Path.GetDirectoryName(report)!, "json-horizontal-bottom.png"));
        check(pane.ColoredReader.CaretBrush is SolidColorBrush { Color.A: 0 } && pane.ColoredReader.FocusVisualStyle is null &&
            !pane.ColoredReader.IsReadOnlyCaretVisible && !DesignChecks.Children<Border>(pane.ColoredReader).Any(b => b.Name == "AccentBorder"),
            "read-only JSON has no caret or accent focus underline");
        pane.ColoredReader.ScrollToHorizontalOffset(double.MaxValue); window.UpdateLayout();
        check(pane.ColoredReader.HorizontalOffset > 100 && pane.ColoredReader.HorizontalOffset <=
            pane.ColoredReader.ExtentWidth - pane.ColoredReader.ViewportWidth + 1,
            "horizontal scrolling stops at content boundary");
        pane.SetWrap(true); window.UpdateLayout(); await Task.Delay(50);
        check(pane.ColoredReader.ExtentWidth <= pane.ColoredReader.ViewportWidth + 1,
            "wrapped JSON removes horizontal overflow");
        pane.SetWrap(false); pane.SetPayload(Payload.From(new { ok = true }), [], ""); await pane.Ready;
        window.UpdateLayout(); await Task.Delay(50);
        check(pane.ColoredReader.HorizontalOffset == 0 && pane.ColoredReader.ExtentWidth <= pane.ColoredReader.ViewportWidth + 1,
            "switching from long to short JSON clears stale horizontal scroll range");
    }

    private static async Task CheckSnapshotUpdates(MonitorView owner, Action<bool, string> check)
    {
        var request = Payload.From(new { @params = new { arguments = new { path = "before.txt", text = new string('a', 9000) } } });
        var record = new CaptureRecord("fixture", 1, DateTimeOffset.UnixEpoch, "read_file", "running", null, request, null, [], []);
        var source = new SnapshotSource(record);
        var window = new MonitorView(source);
        var host = new Window { Owner = Window.GetWindow(owner), ShowInTaskbar = false, Content = window };
        try
        {
            host.Show(); window.UpdateTimer.Stop();
            await window.Request.Ready;
            window.Request.Turn(1);
            int loads = window.Request.DocumentLoads;
            window.Request.SetPayload(request, ["capture note updated"], "loading revision");
            check(window.Request.DocumentLoads == loads && window.Request.PageIndex == 1,
                "same payload with status or notes changes retains reading page");
            source.Record = record with { Status = "success", Response = Payload.From(new { ok = true }) };
            window.Refresh(); await window.Response.Ready;
            check(window.Selected?.Status == "完成" && window.Selected.Capture == "—", "capture without known limits displays dash without claiming verified completeness");
            check(window.Request.DocumentLoads == loads && window.Request.PageIndex == 1 && window.Response.Available,
                "response arrival preserves request reading state");
            window.Query.Text = "later-note"; await window.ApplyFilter();
            check(window.Listing.Items.Count == 0, "note search initially empty");
            source.Record = source.Record with { ResponseNotes = ["later-note"] };
            window.Refresh(); await window.FilterReady;
            check(window.Listing.Items.Count == 1, "notes-only update refreshes active search");
            check(window.Selected?.Status == "完成" && window.Selected.Capture == "部分內容", "capture limit does not turn successful execution into an error");
            window.UpdateLayout();
            var selectedRow = (DataGridRow)window.Listing.ItemContainerGenerator.ContainerFromItem(window.Selected);
            var captureCell = DesignChecks.Children<DataGridCell>(selectedRow).Single(c => c.Column == window.Listing.Columns[5]);
            check(DesignChecks.Children<TextBlock>(captureCell).Single().Foreground == window.FindResource("Warning"),
                "partial capture text uses warning brush");
            var missing = new CaptureRow(record with { Status = "success", Request = null, Response = null });
            check(missing.Capture == "內容缺失" && missing.HasCaptureWarning, "missing capture remains explicit and discoverable");
            var unknown = new CaptureRow(record with { Status = "incomplete", Partial = true });
            check(unknown.Outcome.Contains("未確認") && unknown.Capture == "部分內容", "incomplete record never invents successful execution");
            var error = new CaptureRow(record with { Status = "error" });
            int errorNotifications = 0;
            error.PropertyChanged += (_, e) => { if (e.PropertyName == nameof(CaptureRow.IsError)) errorNotifications++; };
            check(error.IsError, "execution error is marked for red text");
            error.Update(record with { Status = "success" });
            check(!error.IsError && errorNotifications == 1, "status updates remove stale error color");
            check(new CaptureRow(record with { Status = "transport_error" }).IsError && !unknown.IsError,
                "transport errors use error color without treating incomplete outcomes as errors");
            source.Record = source.Record with { Request = Payload.From(new { @params = new { arguments = new { path = "after.txt" } } }) };
            window.Refresh(); await window.FilterReady; await window.Request.Ready;
            check(window.Selected?.Summary.Contains("after.txt") == true, "request update refreshes argument summary");
            check(window.Selected?.Elapsed is null, "snapshot unknown duration remains unknown");
            window.Sort("duration");
            loads = window.Response.DocumentLoads;
            for (int i = 0; i < 4; i++) { source.Record = source.Record with { ElapsedMs = i }; window.Refresh(); }
            check(window.Response.DocumentLoads == loads, "sorted duration updates retain response document");
        }
        finally { window.Dispose(); host.Close(); }
    }

    public static async Task Benchmark(MonitorView window, string report)
    {
        await window.Response.Ready;
        window.Follow.IsChecked = false; window.Response.SetWrap(true);
        await Task.Delay(800);
        var host = Window.GetWindow(window);
        using var measurement = new ResizeMeasurement(host, window);
        measurement.Start("PROGRAMMATIC_RESIZE_80_STEPS");
        for (int i = 0; i < 80; i++)
        {
            int distance = i < 40 ? i : 79 - i;
            host.Width = 900 + distance * 8; host.Height = 820 + distance * 4;
            await Task.Delay(16);
        }
        await Task.Delay(350);
        JsonData.WriteNew(report, measurement.Stop());
    }
}
