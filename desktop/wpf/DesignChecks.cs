using System.IO;
using System.Windows;
using System.Windows.Automation;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Threading;

namespace McpLocal.Desktop;

// Invoked only inside DesktopChecks after its mandatory fixture guard.
internal static class DesignChecks
{
    internal static IEnumerable<T> Children<T>(DependencyObject root) where T : DependencyObject
    {
        if (root is T found) yield return found;
        for (int i = 0; i < VisualTreeHelper.GetChildrenCount(root); i++)
            foreach (var child in Children<T>(VisualTreeHelper.GetChild(root, i))) yield return child;
    }
    private static async Task Draw(Window window)
    {
        window.UpdateLayout();
        await window.Dispatcher.InvokeAsync(() => { }, DispatcherPriority.ContextIdle);
    }
    internal static void Capture(Window window, string path)
    {
        // Capture the complete WPF client root, including the content's margins.
        // Rendering Content alone loses Window.Background and clips offset children.
        var root = PresentationSource.FromVisual(window)?.RootVisual
            ?? throw new InvalidOperationException("截圖需要已呈現的 WPF 視窗。");
        var view = (FrameworkElement)VisualTreeHelper.GetChild(root, 0);
        var bounds = new Rect(view.RenderSize);
        var dpi = VisualTreeHelper.GetDpi(view);
        var bitmap = new RenderTargetBitmap((int)Math.Ceiling(bounds.Width * dpi.DpiScaleX),
            (int)Math.Ceiling(bounds.Height * dpi.DpiScaleY), dpi.PixelsPerInchX, dpi.PixelsPerInchY, PixelFormats.Pbgra32);
        var background = new DrawingVisual();
        using (var drawing = background.RenderOpen()) drawing.DrawRectangle(window.Background, null, bounds);
        bitmap.Render(background);
        bitmap.Render(view);
        // The background is already composited; encode RGB to avoid residual alpha
        // from WPF's premultiplied brush rounding in downstream image viewers.
        var opaque = new FormatConvertedBitmap(bitmap, PixelFormats.Bgr24, null, 0);
        var png = new PngBitmapEncoder(); png.Frames.Add(BitmapFrame.Create(opaque));
        using var file = new FileStream(path, FileMode.CreateNew); png.Save(file);
    }
    private static void CheckExtensions(DesktopWindow window, Action<bool, string> check, string label)
    {
        var editor = Children<ReaderListEditor>(window.Tabs).Single(t => AutomationProperties.GetName(t) == "文字 reader 副檔名");
        string expected = window.LastState.GetProperty("form").GetProperty("lists").GetProperty("extensions").GetString()!;
        check(editor.Text == expected, label + " item editor preserves exact backend text");
        check(editor.VisibleEntries.SequenceEqual(ReaderListEditor.Entries(expected)), label + " each entry is represented individually");
        var controls = Children<Button>(editor).Where(b => !string.IsNullOrEmpty(AutomationProperties.GetName(b))).ToArray();
        check(controls.Length == editor.VisibleEntries.Length + 1, label + " named item buttons and add control are present");
    }
    private static void CheckActions(DesktopWindow window, Action<bool, string> check, string label)
    {
        var settings = window.SettingsActions;
        var connection = window.ConnectionActions;
        bool monitor = window.Tabs.SelectedIndex == 4;
        check(monitor ? settings.Parent is null : settings.IsVisible && settings.TranslatePoint(new Point(0, 0), window).Y < connection.TranslatePoint(new Point(0, 0), window).Y,
            label + " settings commands belong to settings pages only");
        check(connection.TranslatePoint(new Point(connection.ActualWidth, 0), window).X <= window.ActualWidth,
            label + " all global connection actions fit within window");
        check(settings.Children.Count == 3 && connection.Children.Count == 3 &&
            Children<Button>(window.GlobalActions).Where(b => b.IsVisible).All(b => b.ActualHeight >= 32), label + " visible global actions retain usable targets");
        if (!monitor)
            check(settings.ActualWidth > 100 && settings.TranslatePoint(new Point(settings.ActualWidth, 0), window).X <= window.ActualWidth - 16,
                label + " settings actions fit page heading: " + settings.ActualWidth + " / " + settings.TranslatePoint(new Point(0, 0), window));
    }
    private static async Task CheckListDialog(DesktopWindow window, string output, string palette, Action<bool, string> check)
    {
        const string label = "文字 reader 副檔名";
        var editor = Children<ReaderListEditor>(window.Tabs).Single(t => AutomationProperties.GetName(t) == label);
        string original = editor.Text, first = editor.VisibleEntries[0];
        var open = Children<Button>(editor).Single(b => AutomationProperties.GetName(b) == label + "：" + first);
        Exception? failure = null;
        _ = window.Dispatcher.BeginInvoke(DispatcherPriority.ContextIdle, new Action(() =>
        {
            var dialog = Application.Current.Windows.OfType<Window>().Single(w => w.Owner == window && w.Title == label);
            try
            {
                dialog.UpdateLayout();
                var input = Children<TextBox>(dialog).Single();
                check(input.Text == first && input.IsKeyboardFocused, palette + " item dialog preserves entry and receives keyboard focus");
                Capture(dialog, Path.Combine(output, palette + "-item-editor.png"));
                input.Text = ".sample";
                if (palette == "light")
                    Children<Button>(dialog).Single(b => Equals(b.Content, "套用至草稿")).RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
                else dialog.DialogResult = false;
            }
            catch (Exception error) { failure = error; dialog.DialogResult = false; }
        }));
        open.RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
        if (failure is not null) throw failure;
        check(palette == "light" ? editor.VisibleEntries[0] == ".sample" : editor.Text == original, palette + " item dialog applies draft or cancels unchanged");
        editor.Text = original; await Draw(window);
        if (palette == "dark")
        {
            string second = editor.VisibleEntries[1];
            var secondButton = Children<Button>(editor).Single(b => AutomationProperties.GetName(b) == label + "：" + second);
            _ = window.Dispatcher.BeginInvoke(DispatcherPriority.ContextIdle, new Action(() =>
            {
                var dialog = Application.Current.Windows.OfType<Window>().Single(w => w.Owner == window && w.Title == label);
                Children<Button>(dialog).Single(b => Equals(b.Content, "套用至草稿")).RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
            }));
            secondButton.RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
            check(editor.Text == original, "applying an unchanged item preserves original separators and whitespace");
            editor.Text = new string('x', 80); await Draw(window);
            var longChip = Children<Button>(editor).Single(b => AutomationProperties.GetName(b) == label + "：" + new string('x', 80));
            var horizontal = Children<ScrollViewer>(longChip).Single(s => s.Content is TextBlock);
            horizontal.ScrollToHorizontalOffset(horizontal.ExtentWidth); horizontal.UpdateLayout();
            check(horizontal.HorizontalOffset > 0 && editor.Text == new string('x', 80), "long item remains intact and horizontally accessible");
            editor.Text = original; await Draw(window);
        }
    }
    private static async Task CheckBoundaryEditing(DesktopWindow owner, Action<bool, string> check)
    {
        var editor = new BoundaryListTextBox { AcceptsReturn = true, FontFamily = new FontFamily("Consolas"), FontSize = 14, Width = 240, Height = 140 };
        var other = new Button { Content = "focus" };
        var panel = new StackPanel(); panel.Children.Add(editor); panel.Children.Add(other);
        var host = new Window { Owner = owner, ShowInTaskbar = false, Width = 700, Height = 260, Content = panel };
        DesignSystem.Window(host);
        const string original = ".bat, .json, .tsx, credentials.json, build, secrets.yaml, 資料, .long-extension";
        try
        {
            host.Show(); editor.Text = original; await Draw(host);
            foreach (double width in new[] { 200.0, 320.0, 500.0 })
            {
                other.Focus(); editor.Width = width; await Draw(host);
                string view = ((TextBox)editor).Text;
                check(editor.Text == original && editor.TextAlignment == TextAlignment.Left, "resize retains exact logical text and left alignment at " + width);
                foreach (var item in ReaderListEditor.Entries(original)) check(view.Contains(item), "whole entry remains visible in text at " + width + ": " + item);
                editor.SelectAll(); check(editor.SelectedLogicalText() == original, "selection excludes only inserted display breaks at " + width);
            }
            editor.Width = 200; await Draw(host); editor.Focus();
            string? copied = null; editor.CopySink = value => copied = value; editor.SelectAll();
            ApplicationCommands.Copy.Execute(null, editor);
            check(copied == original, "copy command preserves original text without visual breaks");
            string visibleBeforeCut = ((TextBox)editor).Text;
            int visualBreak = visibleBeforeCut.IndexOf("\r\n", StringComparison.Ordinal);
            check(visualBreak >= 2, "fixture contains a visual-only boundary for cut");
            editor.Select(0, visualBreak - 2); int cutStart = editor.SelectedLogicalText().Length;
            editor.Select(visualBreak - 2, 5); string cutText = editor.SelectedLogicalText();
            string expectedCut = original.Remove(cutStart, cutText.Length);
            ApplicationCommands.Cut.Execute(null, editor);
            check(copied == cutText && editor.Text == expectedCut, "cut across display boundary preserves logical text");
            editor.Undo(); check(editor.Text == original, "undo cut restores exact logical text");
            editor.Redo(); check(editor.Text == expectedCut, "redo cut preserves logical text");
            editor.Undo(); check(editor.Text == original, "second undo cut restores exact logical text");
            string viewText = ((TextBox)editor).Text; int position = viewText.IndexOf(".json", StringComparison.Ordinal) + 1;
            editor.Select(position, 0); editor.SelectedText = "X";
            check(editor.Text == original.Insert(original.IndexOf(".json", StringComparison.Ordinal) + 1, "X"), "editing preserves all unedited original text");
            editor.Undo(); check(editor.Text == original, "undo restores original text while editing");
            other.Focus(); await Draw(host);
            var stable = editor.Text;
            check(stable == original, "leaving editor does not save display breaks");
            editor.Focus(); viewText = ((TextBox)editor).Text; position = viewText.IndexOf(".json", StringComparison.Ordinal) + 1;
            editor.Select(position, 0); editor.SelectedText = new string('x', 24);
            other.Focus(); await Draw(host); editor.Focus(); editor.Undo();
            check(editor.Text == original, "undo remains available after focus change and reflow");
            var random = new Random(713); string[] replacements = ["x", "", ", ", "\r\n", "台"];
            editor.Focus();
            for (int i = 0; i < 50; i++)
            {
                viewText = ((TextBox)editor).Text;
                int start = random.Next(viewText.Length + 1), length = random.Next(Math.Min(6, viewText.Length - start) + 1);
                editor.Select(0, start); int rawStart = editor.SelectedLogicalText().Length;
                editor.Select(start, length); int rawLength = editor.SelectedLogicalText().Length;
                string replacement = replacements[random.Next(replacements.Length)];
                string expected = editor.Text.Remove(rawStart, rawLength).Insert(rawStart, replacement);
                editor.SelectedText = replacement;
                check(editor.Text == expected, "selection replacement retains logical text: " + i);
            }
            string afterEdits = editor.Text; int undos = 0, redos = 0;
            while (editor.CanUndo && undos < 100) { editor.Undo(); undos++; }
            while (editor.CanRedo && redos < 100) { editor.Redo(); redos++; }
            check(undos > 0 && undos == redos && editor.Text == afterEdits, "multiple undo and redo preserve exact logical text");
            other.Focus(); await Draw(host);
            editor.Text = new string('x', 80); editor.Width = 200; await Draw(host);
            editor.ScrollToHorizontalOffset(editor.ExtentWidth); editor.UpdateLayout();
            check(editor.HorizontalOffset > 0 && editor.Text == new string('x', 80), "single long entry remains intact and horizontally reachable");
        }
        finally { host.Close(); }
    }
    internal static async Task Run(DesktopWindow window, string report, Action<bool, string> check)
    {
        var output = Path.Combine(Path.GetDirectoryName(report)!, "screenshots"); Directory.CreateDirectory(output);
        await CheckBoundaryEditing(window, check);
        var original = window.Tunnel.Text; window.Tunnel.Text = "tunnel_unsaved_theme_check";
        string? selected = window.Monitor.Selected?.Id;
        window.Monitor.Response.Mode.SelectedIndex = 0;
        int page = window.Monitor.Response.PageIndex, loads = window.Monitor.Response.DocumentLoads;
        var labels = new[] { "connection", "reader", "appearance", "permissions", "monitor" };
        window.Width = 1120; window.Height = 880;
        foreach (var (name, index) in new[] { ("light", 1), ("dark", 2) })
        {
            window.ThemeChoice.SelectedIndex = index;
            await Draw(window);
            var background = ((SolidColorBrush)window.Background).Color;
            check(name == "light" ? background.R > 200 : background.R < 60, name + " application palette applied");
            var input = ((SolidColorBrush)window.Tunnel.Background).Color;
            double inputRed = (input.A * input.R + (255 - input.A) * background.R) / 255.0;
            check(name == "light" ? inputRed > 180 : inputRed < 100, name + " native Fluent input palette applied: " + input);
            for (int tab = 0; tab < 5; tab++)
            {
                window.Tabs.SelectedIndex = tab; await Draw(window);
                check(window.Tabs.SelectedContent is FrameworkElement { ActualWidth: > 600, ActualHeight: > 500 }, name + " page " + labels[tab] + " laid out");
                CheckActions(window, check, name + " " + labels[tab]);
                if (tab == 1) CheckExtensions(window, check, name);
                if (tab == 4)
                {
                    CheckActions(window, check, name);
                    CheckMonitorLayout(window.Monitor, check, name + " main");
                    check(window.Monitor.SelectedSummary.Text == window.Monitor.Selected?.Summary, name + " selected context preserves parameter summary");
                    var cell = Children<DataGridCell>(window.Monitor.Listing).First(c => Children<TextBlock>(c).Any());
                    var text = Children<TextBlock>(cell).First();
                    var top = text.TranslatePoint(new Point(0, 0), cell).Y;
                    check(Math.Abs((top + text.ActualHeight / 2) - cell.ActualHeight / 2) <= 1.5,
                        name + " monitor cell text vertically centered");
                }
                Capture(window, Path.Combine(output, name + "-" + labels[tab] + ".png"));
                if (tab == 4)
                {
                    window.Monitor.FocusDetails(true); await Draw(window);
                    CheckMonitorPanel(window.Monitor, check, name + " main");
                    Capture(window, Path.Combine(output, name + "-monitor-panel.png"));
                    window.Monitor.FocusDetails(false); await Draw(window);
                }
                if (tab == 1)
                {
                    await CheckListDialog(window, output, name, check);
                    var editor = Children<ReaderListEditor>(window.Tabs).Single(t => AutomationProperties.GetName(t) == "文字 reader 副檔名");
                    editor.BringIntoView(); await Draw(window);
                    Capture(window, Path.Combine(output, name + "-extensions.png"));
                    check(editor.VisibleEntries.All(item => !item.Contains(',') && !item.Contains('，')),
                        name + " visible items contain no separator commas");
                }
            }
            var detached = Window.GetWindow(window.DetachedMonitor!); await Draw(detached);
            CheckMonitorLayout(window.DetachedMonitor!, check, name + " detached");
            check(((SolidColorBrush)detached.Background).Color == background, name + " detached monitor shares palette");
            Capture(detached, Path.Combine(output, name + "-detached.png"));
            window.DetachedMonitor!.FocusDetails(true); await Draw(detached);
            CheckMonitorPanel(window.DetachedMonitor, check, name + " detached");
            Capture(detached, Path.Combine(output, name + "-detached-panel.png"));
            window.DetachedMonitor.FocusDetails(false); await Draw(detached);
            // Exercise every page at the minimum size in both palettes.
            window.Width = 620; window.Height = 480;
            for (int tab = 0; tab < 5; tab++)
            {
                window.Tabs.SelectedIndex = tab; await Draw(window);
                CheckActions(window, check, name + " compact " + labels[tab]);
                check(window.Tabs.SelectedContent is FrameworkElement { ActualWidth: > 490, ActualHeight: > 250 }, name + " compact page " + labels[tab] + " laid out");
                Capture(window, Path.Combine(output, name + "-compact-" + labels[tab] + ".png"));
            }
            detached.Width = 620; detached.Height = 480; await Draw(detached);
            check(window.DetachedMonitor!.IsCompact && window.DetachedMonitor.Listing.ActualHeight >= 80, name + " compact detached uses bounded list without outer scrolling");
            Capture(detached, Path.Combine(output, name + "-compact-detached.png"));
            window.DetachedMonitor.FocusDetails(true); await Draw(detached);
            check(window.DetachedMonitor.Response.ColoredReader.ActualHeight >= 64, name + " compact detached dedicates space to JSON");
            Capture(detached, Path.Combine(output, name + "-compact-detached-detail.png"));
            window.DetachedMonitor.FocusDetails(false);
            detached.Width = 1120; detached.Height = 880;
            window.Width = 1120; window.Height = 880; await Draw(window);
        }
        check(window.Tunnel.Text == "tunnel_unsaved_theme_check", "theme switches retain unsaved editor text");
        check(window.Monitor.Selected?.Id == selected && window.Monitor.Response.PageIndex == page && window.Monitor.Response.DocumentLoads == loads,
            "theme switches retain selected record, page and loaded payload");
        var pane = window.Monitor.Response;
        check(new System.Windows.Documents.TextRange(pane.ColoredReader.Document.ContentStart, pane.ColoredReader.Document.ContentEnd).Text.TrimEnd('\r', '\n') == pane.Reader.Text,
            "theme refresh preserves exact colored JSON text");
        DesignSystem.PreviewContrast(true); await Draw(window);
        check(DesignSystem.HighContrast && Equals(window.Background, SystemColors.WindowBrush) && Equals(window.Foreground, SystemColors.WindowTextBrush), "high contrast palette uses Windows system colors without OS mutation");
        for (int tab = 0; tab < 5; tab++)
        {
            window.Tabs.SelectedIndex = tab; await Draw(window);
            Capture(window, Path.Combine(output, "system-color-preview-" + labels[tab] + ".png"));
        }
        var contrastDetached = Window.GetWindow(window.DetachedMonitor!); await Draw(contrastDetached);
        check(Equals(contrastDetached.Background, SystemColors.WindowBrush), "detached contrast palette uses Windows colors");
        Capture(contrastDetached, Path.Combine(output, "system-color-preview-detached.png"));
        DesignSystem.Apply("Dark");
        window.Width = 620; window.Height = 480; await Draw(window);
        check((string)window.Tabs.Tag == "Compact", "minimum width uses compact navigation");
        check(window.Monitor.IsCompact && window.Monitor.Listing.ActualHeight >= 80, "minimum window retains usable list viewport");
        Capture(window, Path.Combine(output, "compact-monitor.png"));
        window.Activate(); await Draw(window);
        window.Monitor.FocusDetails(true); await Draw(window);
        Capture(window, Path.Combine(output, "compact-monitor-detail.png"));
        check(window.Monitor.Response.ColoredReader.IsKeyboardFocusWithin, "compact detail focuses the active colored JSON reader: " + Keyboard.FocusedElement);
        check(window.Monitor.Response.ActualHeight >= 120 && window.Monitor.Response.ColoredReader.ActualHeight >= 64,
            "minimum window dedicates viewport to complete JSON reading: " + window.Monitor.Response.ActualHeight + " / " + window.Monitor.Response.ColoredReader.ActualHeight);
        window.Monitor.FocusDetails(false); await Draw(window);
        check(window.Monitor.Listing.IsVisible && !window.Monitor.Response.IsVisible && window.Monitor.Listing.IsKeyboardFocusWithin, "return from compact detail restores record list and focus");
        window.Tabs.SelectedIndex = 1; await Draw(window);
        await CheckListDialog(window, output, "compact", check);
        CheckExtensions(window, check, "compact");
        var scrolling = Children<ScrollViewer>(window.Tabs).First(s => s.Content is StackPanel);
        scrolling.ScrollToEnd(); await Draw(window);
        check(scrolling.ScrollableHeight > 0 && scrolling.VerticalOffset == scrolling.ScrollableHeight, "compact settings scroll to final controls");
        Capture(window, Path.Combine(output, "compact-reader.png"));
        window.Tabs.SelectedIndex = 0; await Draw(window);
        window.Activate();
        window.Tunnel.Focus();
        check(window.Tunnel.IsKeyboardFocused && AutomationProperties.GetName(window.Tunnel) == "通道識別碼", "connection input has keyboard focus and accessible name");
        window.Tunnel.MoveFocus(new TraversalRequest(FocusNavigationDirection.Next));
        check(Keyboard.FocusedElement is PasswordBox, "form tab order proceeds from tunnel to password");
        foreach (var item in window.Tabs.Items.Cast<TabItem>()) check(!string.IsNullOrEmpty(AutomationProperties.GetName(item)), "navigation accessible name: " + item.Header);
        window.Tunnel.Text = original; window.ThemeChoice.SelectedIndex = 0;
        window.Width = 1120; window.Height = 880; window.Tabs.SelectedIndex = 4; pane.Mode.SelectedIndex = 0;
        await Draw(window);
        check(DesignSystem.Preference == "System", "system theme preference restored without changing Windows");
        Capture(window, Path.Combine(output, "system-monitor.png"));
        window.Width = 900; window.Height = 680; await Draw(window);
        CheckActions(window, check, "medium");
        Capture(window, Path.Combine(output, "medium-monitor.png"));
        window.Width = 1120; window.Height = 880; await Draw(window);
        check(!window.Monitor.IsCompact && window.Monitor.Listing.IsVisible && !window.Monitor.Response.IsVisible, "normal window preserves collapsed panel");
    }

    private static void CheckMonitorLayout(MonitorView monitor, Action<bool, string> check, string label)
    {
        check(!monitor.DetailsFocused && monitor.Listing.IsVisible && !monitor.Tabs.IsVisible &&
            Math.Abs(monitor.Listing.ActualWidth - monitor.LayoutGrid.ActualWidth) < 1,
            label + " defaults to full-width records with JSON collapsed");
        check(monitor.Listing.Columns.All(c => c.Visibility == Visibility.Visible), label + " retains all six columns");
        check(monitor.Listing.RowHeight == 40 && Children<DataGridRow>(monitor.Listing).All(r => Math.Abs(r.ActualHeight - 40) < 0.1),
            label + " rows retain 40 DIP height");
    }
    private static void CheckMonitorPanel(MonitorView monitor, Action<bool, string> check, string label)
    {
        var tabs = monitor.Tabs;
        check(!monitor.Listing.IsVisible && tabs.IsVisible &&
            Math.Abs(tabs.ActualWidth - monitor.LayoutGrid.ActualWidth) < 1,
            label + " JSON occupies full content width");
        check(monitor.SelectedTime.IsVisible && monitor.SelectedTime.Text == monitor.Selected?.DisplayTime &&
            monitor.SelectedTitle.IsVisible && monitor.SelectedSummary.IsVisible && monitor.ExecutionState.IsVisible && monitor.CaptureState.IsVisible,
            label + " full-page header preserves selected row information");
        var cells = new[] { monitor.SelectedTime, monitor.SelectedTitle, monitor.SelectedSummary, monitor.ExecutionState, monitor.SelectedDuration, monitor.CaptureState };
        check(cells.Select((cell, i) => Grid.GetColumn(cell) == i &&
            Math.Abs(cell.TranslatePoint(new Point(0, cell.ActualHeight / 2), monitor.RecordHeader).Y - 56) < 1).All(value => value),
            label + " current record uses one aligned row in list column order");
        check(monitor.CompactSwitch.TranslatePoint(new Point(0, 0), monitor).X > monitor.ActualWidth - 60 &&
            monitor.CompactSwitch.TranslatePoint(new Point(0, 0), monitor).Y < 50,
            label + " JSON action remains in the top-right corner");
    }
}
