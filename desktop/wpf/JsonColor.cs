using System.Windows;
using System.Windows.Documents;
using System.Windows.Media;
using System.Globalization;

namespace McpLocal.Desktop;

// Tokenization runs with formatting off the UI thread; rendering contains one bounded page only.
internal static class JsonColor
{
    internal readonly record struct Span(int Start, int End, int Kind);
    internal static IReadOnlyList<Span> Scan(string text)
    {
        var spans = new List<Span>();
        for (int i = 0; i < text.Length;)
        {
            int start = i;
            if (text[i] == '"')
            {
                i++;
                while (i < text.Length)
                {
                    if (text[i] == '\\') { i = Math.Min(i + 2, text.Length); continue; }
                    if (text[i++] == '"') break;
                }
                int next = i; while (next < text.Length && char.IsWhiteSpace(text[next])) next++;
                spans.Add(new(start, i, next < text.Length && text[next] == ':' ? 0 : 1));
            }
            else if (char.IsDigit(text[i]) || text[i] == '-')
            {
                while (i < text.Length && (char.IsDigit(text[i]) || "+-.eE".Contains(text[i]))) i++;
                spans.Add(new(start, i, 2));
            }
            else if (char.IsLetter(text[i]))
            {
                while (i < text.Length && char.IsLetter(text[i])) i++;
                spans.Add(new(start, i, 3));
            }
            else i++;
        }
        return spans;
    }
    internal static FlowDocument Page(string text, IReadOnlyList<Span> spans, int start, int end, bool wrap)
    {
        var paragraph = new Paragraph { Margin = new Thickness(0), LineHeight = 21, LineStackingStrategy = LineStackingStrategy.MaxHeight };
        var colors = new[] { "JsonKey", "JsonString", "JsonNumber", "JsonLiteral" };
        int cursor = start;
        foreach (var token in spans)
        {
            if (token.End <= start) continue;
            if (token.Start >= end) break;
            int left = Math.Max(start, token.Start), right = Math.Min(end, token.End);
            if (left > cursor) paragraph.Inlines.Add(new Run(text[cursor..left]));
            var run = new Run(text[left..right]); run.SetResourceReference(TextElement.ForegroundProperty, colors[token.Kind]);
            paragraph.Inlines.Add(run); cursor = right;
        }
        if (cursor < end) paragraph.Inlines.Add(new Run(text[cursor..end]));
        return new FlowDocument(paragraph) { FontFamily = new FontFamily("Consolas"), FontSize = 14,
            PagePadding = new Thickness(14, 12, 14, 12), PageWidth = wrap ? double.NaN : ContentWidth(text[start..end]) };
    }
    private static double ContentWidth(string text)
    {
        var typeface = new Typeface("Consolas");
        double width = 0;
        foreach (var line in text.Split('\n'))
        {
            if (line.Length == 0) continue;
            var formatted = new FormattedText(line.TrimEnd('\r'), CultureInfo.CurrentUICulture,
                FlowDirection.LeftToRight, typeface, 14, Brushes.Black, 1);
            width = Math.Max(width, formatted.WidthIncludingTrailingWhitespace);
        }
        return Math.Ceiling(width) + 30;
    }
}
