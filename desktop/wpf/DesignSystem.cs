using System.ComponentModel;
using System.Windows;
using System.Windows.Automation;
using System.Windows.Controls;
using System.Windows.Media;
using Microsoft.Win32;

namespace McpLocal.Desktop;

// Frontend-only presentation state. No settings, credentials or backend commands.
internal static class DesignSystem
{
    private static ResourceDictionary? resources;
    internal static string Preference { get; private set; } = "System";
    internal static bool HighContrast { get; private set; }
    internal static event Action? Changed;

    internal static void Initialize(Application app)
    {
        Apply("System");
        SystemEvents.UserPreferenceChanged += PreferencesChanged;
        SystemParameters.StaticPropertyChanged += ParametersChanged;
        app.Exit += (_, _) =>
        {
            SystemEvents.UserPreferenceChanged -= PreferencesChanged;
            SystemParameters.StaticPropertyChanged -= ParametersChanged;
        };
    }
    private static void PreferencesChanged(object sender, UserPreferenceChangedEventArgs e) => RefreshSystem();
    private static void ParametersChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName == nameof(SystemParameters.HighContrast)) RefreshSystem();
    }
    private static void RefreshSystem()
    {
        var app = Application.Current;
        if (!app.Dispatcher.HasShutdownStarted) app.Dispatcher.BeginInvoke(() => Apply(Preference));
    }
    internal static void Apply(string preference)
    {
        if (preference is not ("System" or "Light" or "Dark")) throw new ArgumentException("未知外觀模式。");
        Preference = preference;
        ApplyPalette(SystemParameters.HighContrast);
    }
    // Tests exercise the same palette path without changing Windows accessibility settings.
    internal static void PreviewContrast(bool enabled) => ApplyPalette(enabled);
    private static void ApplyPalette(bool highContrast)
    {
        var app = Application.Current;
        HighContrast = highContrast;
        if (resources is not null) app.Resources.MergedDictionaries.Remove(resources);
        // .NET 9 marks only this API experimental. Keep all other diagnostics enabled.
#pragma warning disable WPF0001
        app.ThemeMode = highContrast ? ThemeMode.None : Preference switch
        {
            "Light" => ThemeMode.Light, "Dark" => ThemeMode.Dark, _ => ThemeMode.System
        };
#pragma warning restore WPF0001
        bool dark = Preference == "Dark";
        if (Preference == "System")
        {
            using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
            dark = key?.GetValue("AppsUseLightTheme") is int value && value == 0;
        }
        var palette = new ResourceDictionary { Source = new Uri("/McpLocal.Desktop;component/DesignResources.xaml", UriKind.Relative) };
        resources = palette;
        void Color(string name, string light, string night, Brush contrast)
        { palette[name] = highContrast ? contrast : new SolidColorBrush((Color)ColorConverter.ConvertFromString(dark ? night : light)); }
        Color("Canvas", "#F3F3F3", "#202020", SystemColors.WindowBrush);
        Color("Surface", "#FFFFFF", "#2B2B2B", SystemColors.WindowBrush);
        Color("TextPrimary", "#1A1A1A", "#F5F5F5", SystemColors.WindowTextBrush);
        Color("TextSecondary", "#5C5C5C", "#C7C7C7", SystemColors.WindowTextBrush);
        Color("Stroke", "#D6D6D6", "#535353", SystemColors.WindowTextBrush);
        Color("Divider", "#E5E5E5", "#3D3D3D", SystemColors.WindowTextBrush);
        palette["SurfaceBorderThickness"] = new Thickness(highContrast ? 1 : 0);
        palette["SurfaceCornerRadius"] = new CornerRadius(highContrast ? 0 : 8);
        Color("Hover", "#E8E8E8", "#383838", SystemColors.HighlightBrush);
        Color("SelectedSurface", "#E5E5E5", "#383838", SystemColors.HighlightBrush);
        Color("SelectedText", "#1A1A1A", "#FFFFFF", SystemColors.HighlightTextBrush);
        Color("Accent", "#005FB8", "#60CDFF", SystemColors.HighlightBrush);
        Color("Warning", "#805500", "#FFD166", SystemColors.WindowTextBrush);
        Color("ErrorText", "#C42B1C", "#FF99A4", SystemColors.WindowTextBrush);
        Color("JsonKey", "#005A9E", "#79C0FF", SystemColors.WindowTextBrush);
        Color("JsonString", "#146C38", "#A5D6A7", SystemColors.WindowTextBrush);
        Color("JsonNumber", "#8A3800", "#FFBA85", SystemColors.WindowTextBrush);
        Color("JsonLiteral", "#802080", "#E5A6ED", SystemColors.WindowTextBrush);
        foreach (var type in new[] { typeof(Button), typeof(TextBox), typeof(PasswordBox), typeof(ComboBox), typeof(CheckBox), typeof(RadioButton) })
        {
            var style = new Style(type, app.TryFindResource(type) as Style);
            style.Setters.Add(new Setter(Control.FontFamilyProperty, resources["AppFont"]));
            style.Setters.Add(new Setter(Control.FontSizeProperty, 14.0));
            style.Setters.Add(new Setter(FrameworkElement.MinHeightProperty, 32.0));
            if (type == typeof(Button)) style.Setters.Add(new Setter(Control.PaddingProperty, new Thickness(12, 5, 12, 5)));
            if (type == typeof(TextBox) || type == typeof(PasswordBox)) style.Setters.Add(new Setter(Control.PaddingProperty, new Thickness(10, 6, 10, 6)));
            resources[type] = style;
        }
        app.Resources.MergedDictionaries.Add(resources);
        Changed?.Invoke();
    }
    internal static void Window(Window window)
    {
        window.SetResourceReference(Control.BackgroundProperty, "Canvas");
        window.SetResourceReference(Control.ForegroundProperty, "TextPrimary");
        window.SetResourceReference(Control.FontFamilyProperty, "AppFont");
        window.FontSize = 14; window.UseLayoutRounding = true;
    }
    internal static void Name(DependencyObject control, string name) => AutomationProperties.SetName(control, name);
    internal static TextBlock Text(string text, string? style = null)
    {
        var block = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap };
        if (style is not null) block.SetResourceReference(FrameworkElement.StyleProperty, style);
        return block;
    }
    internal static void Secondary(TextBlock block)
    { block.SetResourceReference(TextBlock.ForegroundProperty, "TextSecondary"); block.TextWrapping = TextWrapping.Wrap; }
    internal static StackPanel Card(Panel parent, string title)
    {
        var panel = new StackPanel();
        if (title.Length > 0) panel.Children.Add(Text(title, "SectionTitle"));
        var border = new Border { Child = panel }; border.SetResourceReference(FrameworkElement.StyleProperty, "Card");
        parent.Children.Add(border); return panel;
    }
    internal static void Field(Panel parent, string label, Control editor)
    {
        bool first = parent.Children.Count == 0 || parent.Children.Count == 1 && parent.Children[0] is TextBlock;
        var caption = new Label { Content = label, Target = editor, Padding = new Thickness(0), Margin = new Thickness(0, first ? 0 : 16, 0, 8) };
        parent.Children.Add(caption); parent.Children.Add(editor); Name(editor, label);
    }
}
