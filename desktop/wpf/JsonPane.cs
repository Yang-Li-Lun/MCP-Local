using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Documents;

namespace McpLocal.Desktop;

internal sealed class JsonPane : Grid, IDisposable
{
    internal readonly ComboBox Mode = new() { Width = 144, ItemsSource = new[] { "JSON", "欄位" }, SelectedIndex = 0 };
    internal readonly RichTextBox ColoredReader = new()
    {
        IsReadOnly = true, IsUndoEnabled = false, FontFamily = new FontFamily("Consolas"), FontSize = 14,
        BorderThickness = new Thickness(0), VerticalScrollBarVisibility = ScrollBarVisibility.Auto,
        HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, Visibility = Visibility.Collapsed
    };
    private IReadOnlyList<JsonColor.Span> colorSpans = [];
    internal readonly TextBox Reader = new()
    {
        IsReadOnly = true, AcceptsReturn = true, IsUndoEnabled = false,
        FontFamily = new FontFamily("Consolas"), FontSize = 14, Padding = new Thickness(14, 12, 14, 12),
        BorderThickness = new Thickness(0), VerticalScrollBarVisibility = ScrollBarVisibility.Auto,
        HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, TextWrapping = TextWrapping.NoWrap
    };
    internal readonly TreeView Fields = new() { BorderThickness = new Thickness(0) };
    internal readonly TextBox Search = new() { MinWidth = 40, MaxLength = 200 };
    internal readonly TextBlock Feedback = new() { VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(6, 0, 0, 0) };
    private readonly Grid searchBar = new() { Margin = new Thickness(12, 0, 12, 8), Visibility = Visibility.Collapsed };
    private readonly TextBlock notice = new() { TextWrapping = TextWrapping.Wrap, FontSize = 12 };
    internal readonly ScrollViewer CaptureNotice = new() { MaxHeight = 72, Margin = new Thickness(14, 0, 14, 8), VerticalScrollBarVisibility = ScrollBarVisibility.Auto, HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled, Focusable = true };
    internal readonly Expander CaptureDetails = new() { Margin = new Thickness(12, 0, 12, 8), Visibility = Visibility.Collapsed };
    private readonly StackPanel navigation = new() { Orientation = Orientation.Horizontal, HorizontalAlignment = HorizontalAlignment.Right, Margin = new Thickness(12, 8, 4, 12) };
    private readonly TextBlock pageLabel = new() { VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(8, 0, 8, 0) };
    private readonly Button previous = Ui.Button("上一頁"), next = Ui.Button("下一頁"), scope = Ui.Button("返回完整內容"), copy = Ui.Button("複製");
    private readonly Action<string> copyText;
    private Payload? original, current;
    private string[] notes = [];
    private string missing = "";
    private int generation, page, hit = -1;
    private bool disposed, wrapped;
    private readonly List<int> starts = [0];
    internal string ContentText { get; private set; } = "";
    internal int DocumentLoads { get; private set; }
    internal Task Ready { get; private set; } = Task.CompletedTask;
    internal int PageCount => starts.Count;
    internal int PageIndex => page;
    internal int PageSize => wrapped ? 2048 : 4096;
    internal bool Available => original is not null;

    public JsonPane(Action<string> copyText)
    {
        this.copyText = copyText;
        SetResourceReference(BackgroundProperty, "Surface");
        Reader.SetResourceReference(StyleProperty, "JsonReader");
        ColoredReader.SetResourceReference(StyleProperty, "JsonReader");
        foreach (var reader in new Control[] { Reader, ColoredReader, Fields })
        {
            reader.SetResourceReference(Control.BackgroundProperty, "Surface");
            reader.SetResourceReference(Control.ForegroundProperty, "TextPrimary");
        }
        Fields.Padding = new Thickness(12, 8, 12, 12);
        foreach (var control in new Control[] { Mode, Search, previous, next, scope, copy }) Ui.Outline(control);
        notice.SetResourceReference(TextBlock.ForegroundProperty, "Warning");
        CaptureNotice.Content = notice;
        CaptureDetails.Content = CaptureNotice;
        DesignSystem.Name(CaptureNotice, "內容狀態與擷取註記");
        DesignSystem.Name(Mode, "內容呈現方式"); DesignSystem.Name(Search, "尋找內容");
        DesignSystem.Name(Reader, "JSON 內容"); DesignSystem.Name(ColoredReader, "JSON 上色內容"); DesignSystem.Name(Fields, "JSON 欄位");
        DesignSystem.Changed += RefreshColors;
        foreach (var height in new[] { GridLength.Auto, GridLength.Auto, GridLength.Auto, new GridLength(1, GridUnitType.Star), GridLength.Auto })
            RowDefinitions.Add(new() { Height = height });
        var bar = new DockPanel { Margin = new Thickness(12, 12, 4, 12) };
        var options = Ui.Button("檢視"); Ui.Outline(options);
        DockPanel.SetDock(copy, Dock.Right); bar.Children.Add(copy);
        DockPanel.SetDock(options, Dock.Right); bar.Children.Add(options);
        DockPanel.SetDock(Mode, Dock.Left); bar.Children.Add(Mode);
        scope.Visibility = Visibility.Collapsed;
        scope.HorizontalAlignment = HorizontalAlignment.Left;
        bar.Children.Add(scope); Children.Add(bar);
        SizeChanged += (_, _) =>
        {
            bool compact = ActualHeight < 240;
            double inset = ActualHeight < 180 ? 4 : compact ? 6 : 12;
            bar.Margin = new Thickness(12, inset, 4, inset);
            CaptureDetails.Margin = new Thickness(12, 0, 12, compact ? 4 : 8);
            CaptureNotice.MaxHeight = Math.Clamp(ActualHeight / 4, 32, 72);
        };
        var menu = new ContextMenu();
        var wrapItem = new MenuItem { Header = "自動換行", IsCheckable = true };
        wrapItem.Click += (_, _) => SetWrap(wrapItem.IsChecked);
        menu.Items.Add(wrapItem);
        Ui.Menu(menu, "尋找…  Ctrl+F", FocusSearch);
        Ui.Menu(menu, "閱讀選取欄位", ReadSelectedField);
        Ui.Menu(menu, "摺疊全部欄位", CollapseFields);
        Ui.Menu(menu, "返回完整內容", ResetScope);
        Ui.Attach(options, menu);
        searchBar.ColumnDefinitions.Add(new() { Width = new GridLength(1, GridUnitType.Star) });
        for (int i = 0; i < 4; i++) searchBar.ColumnDefinitions.Add(new() { Width = GridLength.Auto });
        searchBar.Children.Add(Search);
        var up = Ui.Button("↑"); var down = Ui.Button("↓"); var close = Ui.Button("×");
        Ui.Outline(up); Ui.Outline(down); Ui.Outline(close);
        up.ToolTip = "上一個相符項目"; down.ToolTip = "下一個相符項目"; close.ToolTip = "關閉尋找";
        DesignSystem.Name(up, "上一個相符項目"); DesignSystem.Name(down, "下一個相符項目"); DesignSystem.Name(close, "關閉尋找");
        Ui.Place(searchBar, up, 0, 1); Ui.Place(searchBar, down, 0, 2);
        Ui.Place(searchBar, close, 0, 3); Ui.Place(searchBar, Feedback, 0, 4);
        Ui.Place(this, searchBar, 1); Ui.Place(this, CaptureDetails, 2);
        Ui.Place(this, Reader, 3); Ui.Place(this, Fields, 3); Ui.Place(this, ColoredReader, 3);
        navigation.Children.Add(pageLabel); navigation.Children.Add(previous); navigation.Children.Add(next);
        Ui.Place(this, navigation, 4);
        VirtualizingStackPanel.SetIsVirtualizing(Fields, true);
        VirtualizingStackPanel.SetVirtualizationMode(Fields, VirtualizationMode.Recycling);
        Mode.SelectionChanged += (_, _) => ShowMode();
        previous.Click += (_, _) => Turn(-1); next.Click += (_, _) => Turn(1);
        up.Click += (_, _) => Find(-1); down.Click += (_, _) => Find(1);
        close.Click += (_, _) => { searchBar.Visibility = Visibility.Collapsed; FocusContent(); };
        Search.TextChanged += (_, _) => { hit = -1; Feedback.Text = ""; };
        Search.KeyDown += (_, e) => { if (e.Key == Key.Enter) { Find(Keyboard.Modifiers.HasFlag(ModifierKeys.Shift) ? -1 : 1); e.Handled = true; } };
        PreviewKeyDown += (_, e) =>
        {
            if (e.Key == Key.Escape && searchBar.Visibility == Visibility.Visible) { searchBar.Visibility = Visibility.Collapsed; FocusContent(); e.Handled = true; }
            if (e.Key == Key.F && Keyboard.Modifiers.HasFlag(ModifierKeys.Control)) { FocusSearch(); e.Handled = true; }
            if (e.Key == Key.F3) { Find(Keyboard.Modifiers.HasFlag(ModifierKeys.Shift) ? -1 : 1); e.Handled = true; }
        };
        scope.Click += (_, _) => ResetScope();
        copy.Click += (_, _) => { if (current is not null) copyText(ContentText); };
        Fields.MouseDoubleClick += (_, _) => ReadSelectedField();
        var fieldMenu = new ContextMenu();
        Ui.Menu(fieldMenu, "閱讀選取欄位", ReadSelectedField);
        Ui.Menu(fieldMenu, "複製選取欄位", () =>
        {
            if (Fields.SelectedItem is TreeViewItem { Tag: FieldNode node }) copyText(JsonSerializer.Serialize(node.Value, JsonData.PrettyOptions));
        });
        Fields.ContextMenu = fieldMenu;
        ShowMode();
    }

    public void SetPayload(Payload? payload, string[] captureNotes, string absent)
    {
        if (ReferenceEquals(original, payload) && notes.SequenceEqual(captureNotes) && missing == absent) return;
        if (payload is not null && ReferenceEquals(original, payload))
        {
            // Status/notes changes must not discard the current page or field scope.
            notes = captureNotes; missing = absent;
            notice.Text = string.Join(" · ", notes);
            UpdateNotice();
            return;
        }
        original = payload; notes = captureNotes; missing = absent;
        Ready = Load(payload);
    }

    private async Task Load(Payload? payload)
    {
        int version = ++generation;
        DocumentLoads++;
        current = payload; ContentText = ""; page = 0; hit = -1;
        starts.Clear(); starts.Add(0); Reader.Clear(); Fields.Items.Clear(); colorSpans = [];
        Feedback.Text = ""; copy.IsEnabled = false;
        notice.Text = payload is null ? string.Join(" · ", new[] { missing }.Concat(notes).Where(text => text.Length > 0)) : "";
        CaptureDetails.IsExpanded = payload is null;
        scope.Visibility = ReferenceEquals(payload, original) ? Visibility.Collapsed : Visibility.Visible;
        ShowPage();
        if (payload is null) return;
        try
        {
            var formatted = await Task.Run(() => { var text = payload.Pretty; return (text, spans: JsonColor.Scan(text)); });
            if (disposed || generation != version) return;
            ContentText = formatted.text; colorSpans = formatted.spans;
            notice.Text = string.Join(" · ", notes);
            copy.IsEnabled = true;
            Repage(0);
            if (Mode.SelectedIndex == 1) BuildFields();
        }
        catch (Exception error) when (error is JsonException or InvalidOperationException)
        {
            if (disposed || generation != version) return;
            notice.Text = "JSON 無法解析；可閱讀及複製原始文字。";
            ContentText = payload.Json; copy.IsEnabled = true; Repage(0);
        }
    }

    internal void SetWrap(bool value)
    {
        int position = starts[page];
        wrapped = value;
        Reader.TextWrapping = value ? TextWrapping.Wrap : TextWrapping.NoWrap;
        Repage(position);
    }

    private void Repage(int position)
    {
        starts.Clear(); starts.Add(0);
        for (int offset = 0; offset + PageSize < ContentText.Length;)
        {
            offset += PageSize;
            if (char.IsHighSurrogate(ContentText[offset - 1]) && char.IsLowSurrogate(ContentText[offset])) offset--;
            starts.Add(offset);
        }
        page = Math.Max(0, starts.FindLastIndex(x => x <= position)); ShowPage();
    }

    internal void Turn(int direction) { page = Math.Clamp(page + direction, 0, starts.Count - 1); ShowPage(); }
    private void ShowPage()
    {
        int start = starts[page], end = page + 1 < starts.Count ? starts[page + 1] : ContentText.Length;
        Reader.Text = ContentText[start..end];
        if (Mode.SelectedIndex == 0) ColoredReader.Document = JsonColor.Page(ContentText, colorSpans, start, end, wrapped);
        pageLabel.Text = $"第 {page + 1} / {starts.Count} 頁";
        previous.IsEnabled = page > 0; next.IsEnabled = page + 1 < starts.Count;
        navigation.Visibility = starts.Count > 1 && Mode.SelectedIndex != 1 ? Visibility.Visible : Visibility.Collapsed;
        UpdateNotice();
    }
    private void UpdateNotice()
    {
        CaptureDetails.Header = notes.Length > 0 ? $"擷取註記（{notes.Length}）" : "內容狀態";
        CaptureDetails.Visibility = notice.Text.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    internal int Find(int direction)
    {
        var query = Search.Text;
        if (query.Length == 0 || ContentText.Length == 0) return -1;
        int found;
        if (direction > 0)
        {
            int from = hit < 0 ? 0 : Math.Min(ContentText.Length, hit + Math.Max(1, query.Length));
            found = ContentText.IndexOf(query, from, StringComparison.OrdinalIgnoreCase);
            if (found < 0) found = ContentText.IndexOf(query, StringComparison.OrdinalIgnoreCase);
        }
        else
        {
            found = hit > 0 ? ContentText.LastIndexOf(query, hit - 1, StringComparison.OrdinalIgnoreCase) : -1;
            if (found < 0) found = ContentText.LastIndexOf(query, StringComparison.OrdinalIgnoreCase);
        }
        hit = found;
        Feedback.Text = found < 0 ? "找不到" : $"字元 {found + 1:N0}";
        if (found >= 0)
        {
            Mode.SelectedIndex = 0;
            page = starts.FindLastIndex(x => x <= found); ShowPage();
            int offset = found - starts[page];
            var paragraph = (Paragraph)ColoredReader.Document.Blocks.FirstBlock;
            TextPointer? start = null, end = null;
            int consumed = 0, limit = offset + Math.Min(query.Length, Reader.Text.Length - offset);
            foreach (Run run in paragraph.Inlines.OfType<Run>())
            {
                int nextOffset = consumed + run.Text.Length;
                if (start is null && offset <= nextOffset) start = run.ContentStart.GetPositionAtOffset(offset - consumed);
                if (limit <= nextOffset) { end = run.ContentStart.GetPositionAtOffset(limit - consumed); break; }
                consumed = nextOffset;
            }
            if (start is not null && end is not null)
            {
                ColoredReader.Selection.Select(start, end); ColoredReader.Focus();
                var rect = start.GetCharacterRect(LogicalDirection.Forward);
                ColoredReader.ScrollToVerticalOffset(ColoredReader.VerticalOffset + rect.Top);
            }
        }
        return found;
    }

    private void FocusSearch() { searchBar.Visibility = Visibility.Visible; Search.Focus(); Search.SelectAll(); }
    internal void FocusContent()
    {
        if (Mode.SelectedIndex == 1) Fields.Focus();
        else if (Mode.SelectedIndex == 0) ColoredReader.Focus();
        else Reader.Focus();
    }
    private void ShowMode()
    {
        bool tree = Mode.SelectedIndex == 1;
        Reader.Visibility = Visibility.Collapsed;
        ColoredReader.Visibility = Mode.SelectedIndex == 0 ? Visibility.Visible : Visibility.Collapsed;
        Fields.Visibility = tree ? Visibility.Visible : Visibility.Collapsed;
        if (tree && Fields.Items.Count == 0 && ContentText.Length > 0) BuildFields();
        ShowPage();
    }

    internal sealed class FieldNode(JsonElement value)
    {
        public JsonElement Value { get; } = value;
        public IEnumerator<(string Name, JsonElement Value)>? Children;
        public bool Loaded;
    }
    private TreeViewItem CreateField(string name, JsonElement value)
    {
        var node = new FieldNode(value);
        var item = new TreeViewItem { Header = name + "  " + JsonData.Preview(value), Tag = node };
        VirtualizingStackPanel.SetIsVirtualizing(item, true);
        VirtualizingStackPanel.SetVirtualizationMode(item, VirtualizationMode.Recycling);
        if (value.ValueKind is JsonValueKind.Object or JsonValueKind.Array)
        {
            item.Items.Add(new TreeViewItem { Header = "…" });
            item.Expanded += (_, e) => { if (e.OriginalSource == item && !node.Loaded) Expand(item); };
        }
        return item;
    }
    private static IEnumerable<(string, JsonElement)> ChildrenOf(JsonElement value)
    {
        if (value.ValueKind == JsonValueKind.Object)
            foreach (var property in value.EnumerateObject()) yield return (property.Name, property.Value);
        if (value.ValueKind == JsonValueKind.Array)
        {
            int i = 0;
            foreach (var child in value.EnumerateArray()) yield return ($"[{i++}]", child);
        }
    }
    internal void Expand(TreeViewItem item)
    {
        if (item.Tag is not FieldNode node) return;
        if (!node.Loaded)
        {
            item.Items.Clear(); node.Loaded = true;
            node.Children = ChildrenOf(node.Value).GetEnumerator();
        }
        else if (item.Items.Count > 0 && item.Items[^1] is TreeViewItem { Tag: "more" }) item.Items.RemoveAt(item.Items.Count - 1);
        if (node.Children is null) return;
        int count = 0;
        while (count < 100 && node.Children.MoveNext())
        {
            var child = node.Children.Current;
            item.Items.Add(CreateField(child.Name, child.Value)); count++;
        }
        if (count == 100)
        {
            var more = new TreeViewItem { Header = "載入後續欄位…", Tag = "more" };
            more.Selected += (_, e) => { e.Handled = true; Expand(item); };
            item.Items.Add(more);
        }
        else { node.Children.Dispose(); node.Children = null; }
    }
    private void BuildFields()
    {
        if (current is null) return;
        try
        {
            var top = CreateField("$", current.Root);
            Fields.Items.Add(top); top.IsExpanded = true; ExpandIfNeeded(top);
        }
        catch (JsonException) { notice.Text = "無法展開非 JSON 文字。"; }
    }
    private void ExpandIfNeeded(TreeViewItem item) { if (item.Tag is FieldNode { Loaded: false }) Expand(item); }
    internal void ReadSelectedField()
    {
        if (Fields.SelectedItem is not TreeViewItem { Tag: FieldNode node }) return;
        Ready = Load(new Payload(node.Value.GetRawText())); Mode.SelectedIndex = 0;
    }
    internal void ResetScope() { Ready = Load(original); }
    private void CollapseFields()
    {
        void Collapse(ItemsControl parent)
        {
            foreach (TreeViewItem child in parent.Items) { child.IsExpanded = false; Collapse(child); }
        }
        Collapse(Fields);
    }
    private void RefreshColors() { if (!disposed && Mode.SelectedIndex == 0) ShowPage(); }
    public void Dispose() { DesignSystem.Changed -= RefreshColors; disposed = true; generation++; Fields.Items.Clear(); ContentText = ""; original = current = null; }
}

internal static class Ui
{
    public static Button Button(string text) => new() { Content = text, Margin = new Thickness(0, 0, 8, 0) };
    public static void Outline(Control control)
    { control.SetResourceReference(Control.BorderBrushProperty, "Stroke"); control.BorderThickness = new Thickness(1); }
    public static void Place(Grid grid, UIElement child, int row, int column = 0)
    { Grid.SetRow(child, row); Grid.SetColumn(child, column); grid.Children.Add(child); }
    public static MenuItem Menu(ContextMenu menu, string label, Action action)
    {
        var item = new MenuItem { Header = label }; item.Click += (_, _) => action(); menu.Items.Add(item); return item;
    }
    public static void Attach(Button button, ContextMenu menu)
    {
        button.ContextMenu = menu;
        button.Click += (_, _) => { menu.PlacementTarget = button; menu.IsOpen = true; };
    }
}
