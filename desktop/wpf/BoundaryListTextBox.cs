using System.Globalization;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.RegularExpressions;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;

namespace McpLocal.Desktop;

// The TextBox displays line breaks only at delimiters; logical text excludes those visual breaks.
internal sealed class BoundaryListTextBox : TextBox
{
    private string logical = "", displayed = "";
    private bool[] visualBreak = [];
    private sealed record Snapshot(string Displayed, string Logical, bool[] VisualBreak);
    private readonly List<Snapshot> undo = [], redo = [];
    private bool rendering;
    internal event EventHandler? LogicalChanged;
    internal Action<string> CopySink { get; set; } = text => Clipboard.SetText(text);

    internal BoundaryListTextBox()
    {
        TextWrapping = TextWrapping.NoWrap; TextAlignment = TextAlignment.Left;
        HorizontalScrollBarVisibility = ScrollBarVisibility.Auto;
        TextChanged += (_, e) => CaptureEdit(e);
        SizeChanged += (_, _) => { if (!IsKeyboardFocusWithin) Render(); };
        LostKeyboardFocus += (_, _) => Dispatcher.BeginInvoke(() => { if (!IsKeyboardFocusWithin) Render(); });
        CommandBindings.Add(new CommandBinding(ApplicationCommands.Copy, (_, e) => CopySelection(e, false), CanCopy));
        CommandBindings.Add(new CommandBinding(ApplicationCommands.Cut, (_, e) => CopySelection(e, true), CanCopy));
    }

    internal new string Text
    {
        get => logical;
        set
        {
            value ??= "";
            if (logical == value) return;
            logical = value; Render(preserveUndo: false); LogicalChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    private double Measure(string text)
    {
        if (text.Length == 0) return 0;
        return new FormattedText(text, CultureInfo.CurrentUICulture, FlowDirection.LeftToRight,
            new Typeface(FontFamily, FontStyle, FontWeight, FontStretch), FontSize, Brushes.Black,
            VisualTreeHelper.GetDpi(this).PixelsPerDip).WidthIncludingTrailingWhitespace;
    }

    private void Render(bool preserveUndo = true)
    {
        if (rendering) return;
        double available = ActualWidth - Padding.Left - Padding.Right - BorderThickness.Left - BorderThickness.Right
            - SystemParameters.VerticalScrollBarWidth - 12;
        var builder = new StringBuilder(); var flags = new List<bool>();
        double lineWidth = 0;
        foreach (Match match in Regex.Matches(logical, @"[^\r\n,;，；]*[,;，；][ \t]*|[^\r\n,;，；]+|[\r\n]+"))
        {
            string part = match.Value;
            bool newline = part.Contains('\r') || part.Contains('\n');
            double width = newline ? 0 : Measure(part);
            if (!newline && available > 0 && lineWidth > 0 && lineWidth + width > available)
            {
                builder.Append("\r\n"); flags.Add(true); flags.Add(true); lineWidth = 0;
            }
            builder.Append(part); for (int i = 0; i < part.Length; i++) flags.Add(false);
            lineWidth = newline ? 0 : lineWidth + width;
        }
        string text = builder.ToString();
        if (text == displayed) { visualBreak = flags.ToArray(); return; }
        // Replacing TextBox.Text clears native undo; keep existing lines while edits are undoable.
        if (preserveUndo && (CanUndo || CanRedo)) return;
        rendering = true;
        try
        {
            base.Text = text;
            if (base.Text != text) throw new InvalidOperationException("清單編輯器文字不一致。");
            displayed = text; visualBreak = flags.ToArray();
            if (IsUndoEnabled) { IsUndoEnabled = false; IsUndoEnabled = true; }
            undo.Clear(); redo.Clear();
        }
        finally { rendering = false; }
    }

    private void CaptureEdit(TextChangedEventArgs changeEvent)
    {
        if (rendering) return;
        string text = base.Text;
        if (changeEvent.UndoAction is UndoAction.Undo or UndoAction.Redo)
        {
            var from = changeEvent.UndoAction == UndoAction.Undo ? undo : redo;
            var to = changeEvent.UndoAction == UndoAction.Undo ? redo : undo;
            if (from.Count == 0 || from[^1].Displayed != text)
                throw new InvalidOperationException("清單編輯器復原紀錄不一致。");
            var target = from[^1]; from.RemoveAt(from.Count - 1);
            to.Add(new Snapshot(displayed, logical, visualBreak));
            string previous = logical; displayed = target.Displayed; logical = target.Logical; visualBreak = target.VisualBreak;
            if (logical != previous) LogicalChanged?.Invoke(this, EventArgs.Empty);
            return;
        }
        if (changeEvent.UndoAction == UndoAction.Create)
        {
            undo.Add(new Snapshot(displayed, logical, visualBreak));
            if (UndoLimit >= 0 && undo.Count > UndoLimit) undo.RemoveAt(0);
            redo.Clear();
        }
        else if (changeEvent.UndoAction == UndoAction.Merge) redo.Clear();
        var flags = new List<bool>(visualBreak);
        foreach (TextChange change in changeEvent.Changes)
        {
            if (change.Offset > flags.Count || change.RemovedLength > flags.Count - change.Offset)
                throw new InvalidOperationException("清單編輯器變更範圍不一致。");
            flags.RemoveRange(change.Offset, change.RemovedLength);
            flags.InsertRange(change.Offset, Enumerable.Repeat(false, change.AddedLength));
        }
        if (flags.Count != text.Length) throw new InvalidOperationException("清單編輯器索引不一致。");
        var raw = new StringBuilder(text.Length);
        for (int i = 0; i < text.Length; i++) if (!flags[i]) raw.Append(text[i]);
        string updated = raw.ToString(); displayed = text; visualBreak = flags.ToArray();
        if (logical != updated) { logical = updated; LogicalChanged?.Invoke(this, EventArgs.Empty); }
    }

    private void CanCopy(object sender, CanExecuteRoutedEventArgs e)
    { e.CanExecute = SelectionLength > 0 && (e.Command != ApplicationCommands.Cut || !IsReadOnly); e.Handled = true; }

    private void CopySelection(ExecutedRoutedEventArgs e, bool cut)
    {
        string text = SelectedLogicalText();
        try { if (text.Length > 0) CopySink(text); if (cut) SelectedText = ""; }
        catch (ExternalException) { }
        e.Handled = true;
    }
    internal string SelectedLogicalText()
    {
        var text = new StringBuilder();
        for (int i = SelectionStart; i < SelectionStart + SelectionLength; i++)
            if (!visualBreak[i]) text.Append(displayed[i]);
        return text.ToString();
    }
}
