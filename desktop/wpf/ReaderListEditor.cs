using System.Text.RegularExpressions;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;

namespace McpLocal.Desktop;

// The backend remains the authority for validating the complete list.
internal sealed class ReaderListEditor : UserControl
{
    private readonly WrapPanel items = new();
    private readonly TextBlock count = new() { FontSize = 12, Margin = new Thickness(0, 6, 0, 0) };
    private readonly Button add = Ui.Button("新增");
    private readonly string label;
    private string text = "";
    private readonly List<string> undo = [], redo = [];
    internal event EventHandler? LogicalChanged;
    internal static string[] Entries(string value) => Regex.Split(value, "[,;，；\\n]+")
        .Select(part => part.Trim()).Where(part => part.Length > 0).ToArray();
    internal string[] VisibleEntries => Items(text).Select(match => match.Value.Trim()).ToArray();
    private static IEnumerable<Match> Items(string value) => Regex.Matches(value, @"[^,;，；\n]+")
        .Cast<Match>().Where(match => match.Value.Trim().Length > 0);

    internal string Text
    {
        get => text;
        set
        {
            value ??= "";
            if (text == value) return;
            undo.Clear(); redo.Clear(); SetText(value);
        }
    }
    internal ReaderListEditor(string label)
    {
        this.label = label;
        KeyboardNavigation.SetTabNavigation(items, KeyboardNavigationMode.Once);
        KeyboardNavigation.SetDirectionalNavigation(items, KeyboardNavigationMode.Cycle);
        var content = new StackPanel();
        content.Children.Add(items);
        var footer = new DockPanel();
        add.Margin = new Thickness(8, 0, 0, 0); DockPanel.SetDock(add, Dock.Right); footer.Children.Add(add);
        DesignSystem.Secondary(count); footer.Children.Add(count); content.Children.Add(footer); Content = content;
        DesignSystem.Name(this, label); DesignSystem.Name(add, "新增" + label);
        add.Click += (_, _) => Edit(null, 0, "");
        PreviewMouseWheel += (_, e) =>
        {
            DependencyObject? parent = System.Windows.Media.VisualTreeHelper.GetParent(this);
            while (parent is not null && parent is not ScrollViewer) parent = System.Windows.Media.VisualTreeHelper.GetParent(parent);
            if (parent is ScrollViewer page)
            {
                page.ScrollToVerticalOffset(page.VerticalOffset - e.Delta / 3.0);
                e.Handled = true;
            }
        };
        PreviewKeyDown += (_, e) =>
        {
            if (Keyboard.Modifiers == ModifierKeys.Control && e.Key == Key.Z) { Undo(); e.Handled = true; }
            else if (Keyboard.Modifiers == ModifierKeys.Control && e.Key == Key.Y) { Redo(); e.Handled = true; }
        };
        RefreshItems();
    }
    private void SetText(string value) { text = value; RefreshItems(); LogicalChanged?.Invoke(this, EventArgs.Empty); }
    private void Change(string value)
    {
        if (value == text) return;
        undo.Add(text); if (undo.Count > 100) undo.RemoveAt(0); redo.Clear(); SetText(value);
    }
    internal void Undo()
    {
        if (undo.Count == 0) return;
        string value = undo[^1]; undo.RemoveAt(undo.Count - 1); redo.Add(text); SetText(value); FocusItem(0);
    }
    internal void Redo()
    {
        if (redo.Count == 0) return;
        string value = redo[^1]; redo.RemoveAt(redo.Count - 1); undo.Add(text); SetText(value); FocusItem(0);
    }
    private void RefreshItems()
    {
        items.Children.Clear();
        var matches = Items(text).ToArray(); count.Text = $"{matches.Length} 個項目";
        foreach (var match in matches)
        {
            int offset = match.Index, length = match.Length;
            string entry = match.Value.Trim();
            string snapshot = text;
            var chip = Ui.Button(""); chip.Margin = new Thickness(0, 0, 8, 8);
            chip.Content = new ScrollViewer { Content = new TextBlock { Text = entry }, MaxWidth = 300, Focusable = false,
                HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled };
            chip.HorizontalContentAlignment = HorizontalAlignment.Left;
            DesignSystem.Name(chip, label + "：" + entry);
            chip.Click += (_, _) => Edit(offset, length, entry);
            var menu = new ContextMenu(); Ui.Menu(menu, "編輯…", () => Edit(offset, length, entry));
            Ui.Menu(menu, "移除", () => { if (text == snapshot) Remove(offset, length); }); chip.ContextMenu = menu;
            chip.PreviewKeyDown += (_, e) => { if (e.Key == Key.Delete) { if (text == snapshot) Remove(offset, length); e.Handled = true; } };
            items.Children.Add(chip);
        }
    }
    private void FocusItem(int index)
    {
        _ = Dispatcher.BeginInvoke(System.Windows.Threading.DispatcherPriority.Input, new Action(() =>
        {
            if (!IsVisible) return;
            if (index >= items.Children.Count || items.Children.Count == 0) add.Focus();
            else ((Button)items.Children[Math.Clamp(index, 0, items.Children.Count - 1)]).Focus();
        }));
    }
    internal void Remove(int offset, int length)
    {
        int index = Items(text).TakeWhile(match => match.Index < offset).Count();
        Change(text.Remove(offset, length)); FocusItem(index);
    }
    internal bool Apply(int? offset, int length, string value)
    {
        if (value.Trim().Length == 0 || value.IndexOfAny([',', ';', '，', '；', '\r', '\n']) >= 0) return false;
        int focus = offset is int position ? Items(text).TakeWhile(match => match.Index < position).Count() : Items(text).Count();
        string next = offset is int index ? text.Remove(index, length).Insert(index, value)
            : text.Length == 0 ? value : text + "\n" + value;
        if (next.Length > 32768) return false;
        Change(next); FocusItem(focus); return true;
    }
    private void Edit(int? offset, int length, string value)
    {
        string snapshot = text;
        var editor = new TextBox { Text = value, MaxLength = 32768, Margin = new Thickness(0, 0, 0, 12) };
        DesignSystem.Name(editor, label + "項目");
        var root = new DockPanel { Margin = new Thickness(20) };
        var hint = DesignSystem.Text(""); hint.SetResourceReference(TextBlock.ForegroundProperty, "Warning");
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, HorizontalAlignment = HorizontalAlignment.Right, Margin = new Thickness(0, 16, 0, 0) };
        var apply = Ui.Button("套用至草稿"); apply.SetResourceReference(FrameworkElement.StyleProperty, "AccentButtonStyle");
        var cancel = Ui.Button("取消"); cancel.IsCancel = true; cancel.Margin = new Thickness(0);
        if (offset is not null)
        {
            var remove = Ui.Button("移除"); buttons.Children.Add(remove);
            remove.Click += (_, _) => { if (text != snapshot) { hint.Text = "清單已變更，請重新開啟項目。"; return; } Remove(offset.Value, length); Window.GetWindow(editor).DialogResult = true; };
        }
        buttons.Children.Add(apply); buttons.Children.Add(cancel); DockPanel.SetDock(buttons, Dock.Bottom); root.Children.Add(buttons);
        var body = new StackPanel(); body.Children.Add(editor); body.Children.Add(hint); root.Children.Add(body);
        var dialog = new Window { Owner = Window.GetWindow(this), Title = label, Content = root, Width = 520, Height = 220,
            MinWidth = 400, MinHeight = 200, WindowStartupLocation = WindowStartupLocation.CenterOwner, ShowInTaskbar = false };
        DesignSystem.Window(dialog);
        apply.Click += (_, _) =>
        {
            if (text != snapshot) hint.Text = "清單已變更，請重新開啟項目。";
            else if (offset is not null && editor.Text == value || Apply(offset, length, editor.Text)) dialog.DialogResult = true;
            else hint.Text = "請輸入一個項目，且不要使用分隔符。";
        };
        dialog.Loaded += (_, _) => { editor.Focus(); editor.SelectAll(); };
        dialog.ShowDialog();
    }
}
