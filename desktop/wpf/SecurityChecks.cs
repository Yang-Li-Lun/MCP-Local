using System.IO;
using System.Text.Json;
using System.Text.Json.Nodes;
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
        check(view.Sections["overview"].IsVisible && view.Status.IsVisible && !view.Enabled.IsVisible, "security opens on overview with settings hidden");
        check(!view.Actions["grant"].IsEnabled && view.GrantReason.Text.Contains("啟用"), "disabled policy prevents authorization and explains prerequisite");
        check(!view.Actions["save"].IsEnabled, "unchanged draft cannot be submitted");
        bool Shortcut(int index) => ((ComboBoxItem)view.Duration.Items[index]).IsEnabled;
        window.ConfirmFixture = _ => true;
        for (int i = 0; i < 40 && view.Recent.Text == "載入中…"; i++) await Task.Delay(50);
        check(view.Recent.Text != "載入中…" && !view.Recent.Text.Contains("未確認"), "security overview loads recent events automatically");
        view.Sections["settings"].RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
        view.Advanced.IsExpanded = true; window.UpdateLayout(); await Task.Delay(60);
        view.Diagnostics.IsExpanded = true; window.UpdateLayout();
        check(view.Enabled.IsVisible && view.Advanced.IsExpanded, "security settings and advanced controls are reachable");
        var pageScroll = (ScrollViewer)view.Content;
        pageScroll.ScrollToTop(); window.UpdateLayout();
        void Wheel(UIElement target, int delta)
        {
            var preview = new MouseWheelEventArgs(Mouse.PrimaryDevice, Environment.TickCount, delta) { RoutedEvent = Mouse.PreviewMouseWheelEvent };
            target.RaiseEvent(preview);
            if (!preview.Handled) target.RaiseEvent(new MouseWheelEventArgs(Mouse.PrimaryDevice, Environment.TickCount, delta) { RoutedEvent = Mouse.MouseWheelEvent });
        }
        Wheel(view.Enabled, -120); await Task.Delay(60); window.UpdateLayout();
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
        check(!view.Detector.IsEnabled && view.Guard.IsEnabled, "guard dependency prevents disabling its detector");
        view.Guard.IsChecked = false; view.Detector.IsChecked = false;
        check(!view.Guard.IsEnabled, "disabled detector prevents enabling dependent guard");
        view.Detector.IsChecked = true; view.Guard.IsChecked = true;
        view.Maximum.Text = "10";
        view.SelectSection("overview"); await view.Refresh(); view.SelectSection("settings");
        check(view.Maximum.Text == "10" && view.Dirty, "navigation and status refresh preserve unsaved draft");
        window.ConfirmFixture = _ => false;
        await Click("reload"); await Click("defaults");
        check(view.Maximum.Text == "10" && view.Dirty, "reload and defaults retain draft when discard is declined");
        await window.Run("quit");
        check(view.IsLoaded && view.Dirty, "window exit retains draft when discard is declined");
        window.ConfirmFixture = _ => true;
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
        check(!view.Actions["grant"].IsEnabled && view.Status.Text.Contains("等待"), "saved policy without execution client is not shown as effective");
        await backend.Send("fixture_security_peer");
        await Until(() => view.CurrentState.GetProperty("effective").GetString() == "confirmed");
        view.SelectSection("overview"); window.UpdateLayout();
        check(Shortcut(0) && Shortcut(1) && !Shortcut(2) && view.Actions["grant"].IsEnabled, "duration choices and single grant respect saved maximum");
        view.Maximum.Text = "15";
        check(!Shortcut(2) && view.Dirty, "unsaved maximum cannot enable longer quick grants");
        view.Duration.SelectedIndex = 1;
        check(!view.CurrentState.GetProperty("authorization").GetProperty("authorized").GetBoolean(), "duration selection alone never grants authorization");
        window.ConfirmFixture = _ => false;
        await Click("grant");
        check(!view.CurrentState.GetProperty("authorization").GetProperty("authorized").GetBoolean(), "security quick grant requires local confirmation");
        window.ConfirmFixture = _ => true;
        await Click("grant");
        check(view.CurrentState.GetProperty("authorization").GetProperty("remaining_seconds").GetInt32() > 0, "security quick grant reaches authority");
        check(view.Actions["grant"].Visibility == Visibility.Collapsed && view.Actions["revoke"].IsVisible, "active lease replaces grant with immediate revoke");
        await Click("revoke");
        check(!view.CurrentState.GetProperty("authorization").GetProperty("authorized").GetBoolean(), "security immediate revoke reaches authority");
        await Until(() => view.CurrentState.GetProperty("effective").GetString() == "confirmed");
        // Synthetic snapshots test presentation only; mutations still use the isolated real authority.
        var synthetic = JsonNode.Parse(view.CurrentState.GetRawText())!;
        synthetic["effective"] = "error"; synthetic["error"] = "SYNTHETIC：測試異常";
        view.Apply(JsonSerializer.SerializeToElement(synthetic), false);
        check(!view.Actions["grant"].IsEnabled && view.Actions["open_diagnostics"].IsVisible && !view.Actions["save"].IsEnabled, "abnormal snapshot blocks grant and save with direct diagnostics entry");
        await Click("open_diagnostics"); window.UpdateLayout();
        check(view.Diagnostics.IsExpanded && view.Diagnostics.IsVisible, "overview opens diagnostics directly");
        await Click("recover");
        await Until(() => view.CurrentState.GetProperty("effective").GetString() == "confirmed");
        check(!view.CurrentState.GetProperty("authorization").GetProperty("authorized").GetBoolean(), "security recovery never grants or renews a lease");
        view.SelectSection("settings"); window.UpdateLayout();
        view.Maximum.Text = "3"; await Click("save");
        await Until(() => view.CurrentState.GetProperty("effective").GetString() == "confirmed");
        view.SelectSection("overview"); window.UpdateLayout();
        check(!Shortcut(0) && !Shortcut(1) && !Shortcut(2), "all shortcuts disabled below five-minute saved maximum");
        view.Duration.SelectedIndex = 3;
        foreach (string invalid in new[] { "", "0", "4", "61", "xx" })
        { view.Minutes.Text = invalid; check(!view.Actions["grant"].IsEnabled, "invalid custom duration is blocked: " + invalid); }
        view.Minutes.Text = "1"; await Click("grant");
        check(view.CurrentState.GetProperty("authorization").GetProperty("authorized").GetBoolean(), "custom duration remains usable below five-minute maximum");
        await Click("revoke");
        view.SelectSection("events"); window.UpdateLayout();
        for (int i = 0; i < 50 && view.Events.ItemsSource is null; i++) await Task.Delay(50);
        check(view.Events.Items.Count > 0, "security events load automatically when opened");
        view.Events.SelectedIndex = 0;
        for (int i = 0; i < 30 && view.DetailText.Length == 0; i++) await Task.Delay(50);
        check(view.DetailText.Contains("hash") && !view.EventDetail.Text.Contains("hash") && view.EventDiagnostics.Visibility == Visibility.Visible, "security event detail is readable with adjacent raw diagnostics");
        check(!view.Actions["previous"].IsEnabled && !view.Actions["next"].IsEnabled, "single page disables unavailable pagination");
        async Task FilterSettled() { await Task.Delay(250); for (int i = 0; i < 40 && !view.Actions["events"].IsEnabled; i++) await Task.Delay(50); }
        view.Risk.SelectedIndex = 1; await FilterSettled();
        check(view.Events.Items.Count > 0 && view.Events.Items.Cast<object>().All(row => row.ToString()!.Contains("高風險")), "security risk filter affects backend query results");
        view.Risk.SelectedIndex = 0; view.Kind.SelectedIndex = 4; await FilterSettled();
        check(view.Events.Items.Count == 0, "security kind filter returns no unrelated events");
        view.Kind.SelectedIndex = 0; await FilterSettled();
        string export = Path.Combine(fixture, "synthetic-security-export.json");
        view.ExportSink = text => File.WriteAllTextAsync(export, text);
        await Click("export");
        using (var evidence = JsonDocument.Parse(File.ReadAllText(export)))
            check(evidence.RootElement.GetProperty("chain").GetArrayLength() >= 5 && !evidence.RootElement.GetProperty("contains_sensitive_originals").GetBoolean(), "security export contains verified metadata chain without sensitive originals");
        view.ClearConfirmation.Text = "incorrect"; await Click("clear");
        check(!view.Actions["clear"].IsEnabled, "security clear blocks incomplete confirmation before submission");
        await Click("events"); view.ClearConfirmation.Text = "清除安全事件";
        window.ConfirmFixture = _ => false; await Click("clear");
        check(view.ClearConfirmation.Text == "清除安全事件" && view.Events.Items.Count > 1, "clear still requires local confirmation after exact text");
        window.ConfirmFixture = _ => true; await Click("clear");
        check(view.Events.Items.Count > 0 && view.Events.Items[0].ToString()!.Contains("事件已清除"), "security controlled clear preserves a boundary event");
        foreach (string theme in new[] { "Light", "Dark", "HighContrast" })
        {
            if (theme == "HighContrast") DesignSystem.PreviewContrast(true); else DesignSystem.Apply(theme);
            window.Width = theme == "Dark" ? 660 : 1120; window.Height = 880;
            view.SelectSection("overview"); window.UpdateLayout(); await Task.Delay(60);
            check(((SolidColorBrush)view.Status.Foreground).Color == ((SolidColorBrush)Application.Current.FindResource("TextPrimary")).Color,
                "security live status uses current accessible foreground in " + theme);
            DesignChecks.Capture(window, Path.Combine(Path.GetDirectoryName(report)!, "security-" + theme + ".png"));
            check(view.ActualWidth > 300 && view.Actions["grant"].ActualWidth > 30, "security overview renders in " + theme);
            view.SelectSection("settings"); window.UpdateLayout(); await Task.Delay(60);
            check(view.Actions["save"].IsVisible && view.Actions["save"].ActualWidth > 60, "security settings render in " + theme);
            DesignChecks.Capture(window, Path.Combine(Path.GetDirectoryName(report)!, "security-settings-" + theme + ".png"));
            view.SelectSection("events"); window.UpdateLayout(); await Task.Delay(60);
            check(view.Events.IsVisible && view.Events.ActualWidth > 150, "security events render in " + theme);
            DesignChecks.Capture(window, Path.Combine(Path.GetDirectoryName(report)!, "security-events-" + theme + ".png"));
        }
        DesignSystem.Apply("System"); window.Width = 1120;
        view.SelectSection("settings"); window.UpdateLayout();
        await Click("defaults"); await Click("save");
        window.Tabs.SelectedIndex = 4;
    }
}
