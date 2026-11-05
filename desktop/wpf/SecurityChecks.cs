using System.IO;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Input;

namespace McpLocal.Desktop;

internal static class SecurityChecks
{
    internal static async Task Run(DesktopWindow window, BackendClient backend, string fixture, string report, Action<bool, string> check)
    {
        if (!backend.IsFixture) throw new InvalidOperationException("安全中心驗證只允許合成 fixture。");
        window.Tabs.SelectedIndex = 5; window.UpdateLayout();
        var view = window.Security;
        await Task.Delay(100); await view.Refresh();
        var pageScroll = (ScrollViewer)view.Content;
        pageScroll.ScrollToTop(); window.UpdateLayout();
        void Wheel(UIElement target, int delta)
        {
            var preview = new MouseWheelEventArgs(Mouse.PrimaryDevice, Environment.TickCount, delta) { RoutedEvent = Mouse.PreviewMouseWheelEvent };
            target.RaiseEvent(preview);
            if (!preview.Handled) target.RaiseEvent(new MouseWheelEventArgs(Mouse.PrimaryDevice, Environment.TickCount, delta) { RoutedEvent = Mouse.MouseWheelEvent });
        }
        Wheel(view.Status, -120); await Task.Delay(60); window.UpdateLayout();
        check(pageScroll.ScrollableHeight > 0 && pageScroll.VerticalOffset > 0, "security page scrolls with mouse wheel");
        pageScroll.ScrollToTop(); window.UpdateLayout();
        var grid = DesignChecks.Children<DataGrid>(view).Single();
        Wheel(DesignChecks.Children<ScrollViewer>(grid).First(), -120); await Task.Delay(60); window.UpdateLayout();
        check(pageScroll.VerticalOffset > 0, "security rule grid passes wheel to page when not scrollable");
        pageScroll.ScrollToTop(); window.UpdateLayout();
        var output = DesignChecks.Children<TextBox>(view).First(box => box.IsReadOnly);
        Wheel(DesignChecks.Children<ScrollViewer>(output).First(), -120); await Task.Delay(60); window.UpdateLayout();
        check(pageScroll.VerticalOffset > 0, "security output passes wheel to page when not scrollable");
        output.Text = string.Join("\n", Enumerable.Repeat("synthetic output", 100));
        pageScroll.ScrollToTop(); window.UpdateLayout();
        var innerScroll = DesignChecks.Children<ScrollViewer>(output).First();
        Wheel(innerScroll, -120); await Task.Delay(60); window.UpdateLayout();
        check(innerScroll.VerticalOffset > 0 && pageScroll.VerticalOffset == 0, "security scrollable output retains its own wheel");
        innerScroll.ScrollToEnd(); await Task.Delay(60); window.UpdateLayout();
        double innerBefore = innerScroll.VerticalOffset, innerMax = innerScroll.ScrollableHeight, pageBefore = pageScroll.VerticalOffset;
        Wheel(innerScroll, -120); await Task.Delay(60); window.UpdateLayout();
        check(pageScroll.VerticalOffset > pageBefore, $"security output passes wheel to page at lower boundary ({innerBefore}/{innerMax}, page {pageBefore}/{pageScroll.VerticalOffset})");
        innerScroll.ScrollToTop(); await Task.Delay(60); window.UpdateLayout();
        Wheel(innerScroll, 120); await Task.Delay(60); window.UpdateLayout();
        check(pageScroll.VerticalOffset == 0, "security output passes wheel to page at upper boundary");
        output.Clear(); pageScroll.ScrollToTop(); window.UpdateLayout();
        async Task Click(string action)
        {
            view.Actions[action].RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
            await view.LastOperation;
        }
        async Task Until(Func<bool> predicate)
        {
            for (int i = 0; i < 50; i++)
            {
                await view.Refresh();
                if (predicate()) return;
                await Task.Delay(100);
            }
            throw new TimeoutException(view.Status.Text + " / " + view.Feedback.Text);
        }
        check(!view.Enabled.IsChecked!.Value && view.Rules.Count == 5, "security legacy policy defaults OFF and five editable builtin rules");
        await Click("add_rule");
        check(view.Rules.Count == 6 && view.Dirty, "security add rule changes draft only");
        DesignChecks.Children<DataGrid>(view).Single().SelectedItem = view.Rules.Last();
        await Click("remove_rule");
        check(view.Rules.Count == 5, "security remove selected rule updates the actual editor");
        await Click("defaults");
        check(view.Rules.Count == 5 && view.Dirty && !File.Exists(Path.Combine(fixture, "security-policy.json")), "security defaults restore does not silently save");
        view.Enabled.IsChecked = true;
        await Click("detect");
        check(view.PreviewText.Contains("findings") && !view.PreviewText.Contains("synthetic_password_123"), "security detector tests synthetic data through private Python pipe");
        await Click("preview");
        check(view.PreviewText.Contains("masked") && !view.PreviewText.Contains("synthetic_password_123"), "security preview uses guard with current unsaved policy");
        window.ConfirmFixture = _ => false;
        await Click("save");
        check(!File.Exists(Path.Combine(fixture, "security-policy.json")), "security saving requires explicit local confirmation");
        window.ConfirmFixture = _ => true;
        await Click("save");
        using (var saved = JsonDocument.Parse(File.ReadAllText(Path.Combine(fixture, "security-policy.json"))))
            check(saved.RootElement.GetProperty("enabled").GetBoolean() && !view.Dirty, "security policy persisted independently and draft cleared on matching success");
        await backend.Send("fixture_security_peer");
        await Until(() => view.CurrentState.GetProperty("effective").GetString() == "confirmed");
        view.Minutes.Text = "1";
        await Click("grant");
        check(view.CurrentState.GetProperty("authorization").GetProperty("remaining_seconds").GetInt32() > 0, "security local grant reaches authority and displays remaining time");
        await Click("revoke");
        check(!view.CurrentState.GetProperty("authorization").GetProperty("authorized").GetBoolean(), "security immediate revoke reaches authority");
        await Click("diagnose"); await Click("recover");
        await Until(() => view.CurrentState.GetProperty("effective").GetString() == "confirmed");
        check(!view.CurrentState.GetProperty("authorization").GetProperty("authorized").GetBoolean(), "security recovery never grants or renews a lease");
        await Click("events");
        view.Events.SelectedIndex = 0;
        for (int i = 0; i < 30 && view.DetailText.Length == 0; i++) await Task.Delay(50);
        check(view.DetailText.Contains("hash") && view.DetailText.Contains("event"), "security event selection loads verified detail from backend");
        view.Risk.SelectedIndex = 1; await Click("events");
        check(view.Events.Items.Count > 0 && view.Events.Items.Cast<object>().All(row => row.ToString()!.Contains("high")), "security risk filter affects backend query results");
        view.Risk.SelectedIndex = 0; view.Kind.SelectedIndex = 4; await Click("events");
        check(view.Events.Items.Count == 0, "security kind filter returns no unrelated events");
        view.Kind.SelectedIndex = 0; await Click("events");
        string export = Path.Combine(fixture, "synthetic-security-export.json");
        view.ExportSink = text => File.WriteAllTextAsync(export, text);
        await Click("export");
        using (var evidence = JsonDocument.Parse(File.ReadAllText(export)))
            check(evidence.RootElement.GetProperty("chain").GetArrayLength() >= 5 && !evidence.RootElement.GetProperty("contains_sensitive_originals").GetBoolean(), "security export contains verified metadata chain without sensitive originals");
        view.ClearConfirmation.Text = "incorrect"; await Click("clear");
        check(view.Feedback.Text.Contains("SECURITY_AUDIT_CONFIRM"), "security clear rejects wrong confirmation text");
        await Click("events"); view.ClearConfirmation.Text = "清除安全事件"; await Click("clear");
        var cleared = await backend.Send("security_events");
        check(cleared.GetProperty("rows")[0].GetProperty("event").GetProperty("kind").GetString() == "audit_cleared", "security controlled clear preserves a boundary event");
        foreach (string theme in new[] { "Light", "Dark", "HighContrast" })
        {
            if (theme == "HighContrast") DesignSystem.PreviewContrast(true); else DesignSystem.Apply(theme);
            window.Width = theme == "Dark" ? 660 : 1120; window.Height = 880;
            window.UpdateLayout(); await Task.Delay(60);
            check(((SolidColorBrush)view.Status.Foreground).Color == ((SolidColorBrush)Application.Current.FindResource("TextPrimary")).Color,
                "security live status uses current accessible foreground in " + theme);
            DesignChecks.Capture(window, Path.Combine(Path.GetDirectoryName(report)!, "security-" + theme + ".png"));
            check(view.ActualWidth > 300 && view.Actions["save"].ActualWidth > 60, "security functional page renders in " + theme);
        }
        DesignSystem.Apply("System"); window.Width = 1120;
        await Click("defaults"); await Click("save");
        window.Tabs.SelectedIndex = 4;
    }
}
