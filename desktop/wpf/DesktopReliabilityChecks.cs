using System.Diagnostics;
using System.Text.Json;

namespace McpLocal.Desktop;

internal static class DesktopReliabilityChecks
{
    internal static async Task Run(DesktopWindow window, BackendClient backend, Action<bool, string> check)
    {
        window.PausePollingForChecks();
        await Task.Delay(300); await window.Poll();
        int refreshes = window.Monitor.RefreshTicks;
        var snapshot = window.Source.Snapshot;
        for (int i = 0; i < 5; i++) await window.Poll();
        check(ReferenceEquals(snapshot, window.Source.Snapshot) && refreshes == window.Monitor.RefreshTicks,
            "unchanged monitor snapshots skip WPF refresh and retain objects");
        check(window.LastStateSuccess is not null && window.Source.LastSuccess is not null, "successful query timestamps recorded");
        backend.QueryTimeout = TimeSpan.FromMilliseconds(150);
        foreach (string query in new[] { "state", "monitor" })
        {
            var timer = Stopwatch.StartNew(); bool expired = false;
            try { await backend.Send(query, new { _fixture_reply_delay_ms = 800 }); }
            catch (TimeoutException) { expired = true; }
            check(expired && timer.ElapsedMilliseconds < 700, query + " deadline bounds a delayed response");
            await window.Poll();
            check(query == "state" ? window.BackendStatus.Text.Contains("最後成功更新") : window.Monitor.NoticeText.Contains("最後成功更新"),
                query + " timeout displays last success without discarding snapshot");
            await Task.Delay(850); await window.Poll();
            check(window.BackendStatus.Text.Length == 0 && window.Source.QueryError is null, query + " late reply permits read-only recovery");
        }
        backend.QueryTimeout = TimeSpan.FromSeconds(3);
        backend.OperationTimeout = TimeSpan.FromMilliseconds(150);
        string originalTunnel = window.Tunnel.Text;
        window.Tunnel.Text = "tunnel_delayed_draft";
        await window.Run("edit", new { form = new { tunnel = "tunnel_delayed_draft" }, _fixture_reply_delay_ms = 800 });
        window.Tunnel.Text = "tunnel_newer_unsent_draft";
        await window.Run("edit", new { form = new { tunnel = "tunnel_newer_unsent_draft" } });
        await Task.Delay(850); await window.Poll();
        check(window.Tunnel.Text == "tunnel_newer_unsent_draft" && window.DraftState.Text.Contains("未儲存"),
            "late acknowledgement never confirms or overwrites a newer unsent draft");
        window.Tunnel.Text = originalTunnel;
        await window.Run("save", includeForm: true);
        for (int i = 0; i < 60 && window.LastState.GetProperty("busy").GetBoolean(); i++) { await Task.Delay(50); await window.Poll(); }
        foreach (string operation in new[] { "save", "start", "restart", "stop" })
        {
            var before = await backend.Send("fixture_counts");
            int count = before.TryGetProperty(operation, out var previous) ? previous.GetInt32() : 0;
            await window.Run(operation, new { _fixture_reply_delay_ms = 800 });
            check(backend.OperationPending && backend.OperationUnconfirmed, operation + " deadline leaves result unconfirmed");
            await window.Poll();
            check(backend.OperationPending && window.BackendStatus.Text.Contains("尚未確認"), operation + " state polling cannot confirm a missing acknowledgement");
            bool refused = false;
            try { await backend.Send(operation); } catch (InvalidOperationException) { refused = true; }
            check(refused, operation + " duplicate action blocked while original reply is pending");
            await Task.Delay(850); await window.Poll();
            check(!backend.OperationPending && !backend.OperationUnconfirmed, operation + " original late acknowledgement resolves uncertainty");
            var after = await backend.Send("fixture_counts");
            check(after.GetProperty(operation).GetInt32() == count + 1, operation + " dispatched exactly once");
            for (int i = 0; i < 60 && window.LastState.GetProperty("busy").GetBoolean(); i++) { await Task.Delay(50); await window.Poll(); }
            check(!window.LastState.GetProperty("busy").GetBoolean(), operation + " transaction polled to completion");
        }
        backend.OperationTimeout = TimeSpan.FromSeconds(15);
        await window.Run("start");
        for (int i = 0; i < 80 && !window.Source.Connected; i++) { await Task.Delay(50); await window.Poll(); }
        window.OpenMonitor(); await Task.Delay(100); window.Hide();
        long lost = window.Source.Snapshot.Diagnostics.LostEvents;
        // No frontend monitor query runs during this controlled burst. Collection
        // is asynchronous; verify completion separately from event loss.
        for (int batch = 0; batch < 8; batch++)
        {
            for (int i = 0; i < 4; i++) await backend.Send("fixture_emit", new { number = batch * 4 + i });
            await Task.Delay(100);
        }
        await Task.Delay(250); await window.Poll();
        for (int i = 0; i < 20 && window.Source.Snapshot.Records.Count(r => r.Id.StartsWith("live-")) < 32 &&
                window.Source.Snapshot.Diagnostics.LostEvents == lost; i++)
        { await Task.Delay(100); await window.Poll(); }
        check(window.Source.Snapshot.Records.Count(r => r.Id.StartsWith("live-")) == 32,
            "hidden main window keeps collecting all 32 events without frontend polling during burst; observed=" +
            window.Source.Snapshot.Records.Count(r => r.Id.StartsWith("live-")) + ", lost=" + window.Source.Snapshot.Diagnostics.LostEvents);
        check(window.Source.Snapshot.Diagnostics.LostEvents == lost, "unchanged collection cadence adds no event loss in controlled burst");
        check(window.DetachedMonitor!.Rows.Count(r => r.Id.StartsWith("live-")) == 32, "detached monitor updates while main window is hidden");
        window.Show(); await window.Run("stop");
        for (int i = 0; i < 80 && window.LastState.GetProperty("running").GetBoolean(); i++) { await Task.Delay(50); await window.Poll(); }
        await window.Source.RefreshIdentity();
        var identity = window.Source.BackendIdentity;
        check(identity.GetProperty("loaded_matches_build").GetBoolean(), "actual loaded Python bytes match the candidate build manifest");
        check(identity.GetProperty("disk_matches_build_now").GetBoolean(), "current backend source matches the candidate deployment");
        check(identity.GetProperty("loaded_modules").TryGetProperty("mcp_local/desktop_bridge.py", out _), "executed bridge is included in runtime identity");
        using var diagnostic = JsonDocument.Parse(window.Monitor.DiagnosticsText());
        var frontend = diagnostic.RootElement.GetProperty("frontend");
        check(frontend.GetProperty("frontend_build_id").GetString() != "未知", "frontend build identity comes from loaded assembly resource");
        check(frontend.GetProperty("disk_deployment_now").GetProperty("matches_manifest").GetBoolean(), "deployment artifact hashes match disk");
        check(frontend.GetProperty("disk_deployment_now").GetProperty("matches_loaded_build").GetBoolean(), "disk build matches loaded assembly identity");
        check(frontend.GetProperty("process_start_id").GetString() != identity.GetProperty("process_start_id").GetString(), "frontend and backend have independent startup identities");
    }
}
