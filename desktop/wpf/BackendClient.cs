using System.Collections.Concurrent;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Text.Json;

namespace McpLocal.Desktop;

internal sealed class BackendClient : IDisposable
{
    private readonly Process process;
    private readonly SemaphoreSlim writer = new(1);
    private readonly ConcurrentDictionary<int, TaskCompletionSource<JsonElement>> pending = new();
    private readonly ConcurrentDictionary<string, int> queries = new();
    private readonly ConcurrentDictionary<int, string> names = new();
    private readonly CancellationTokenSource lifetime = new();
    private int sequence, closed, mutation;
    internal TimeSpan QueryTimeout { get; set; } = TimeSpan.FromSeconds(3);
    internal TimeSpan OperationTimeout { get; set; } = TimeSpan.FromSeconds(15);
    internal bool OperationPending => Volatile.Read(ref mutation) != 0;
    internal bool OperationUnconfirmed { get; private set; }
    internal string? OperationError { get; private set; }
    internal event Action? OperationChanged;
    private static bool IsQuery(string command) => command is "hello" or "state" or "monitor" or "detail" or "export" or "monitor_search" or "diagnostics" or "security_state" or "security_events" or "security_detail" or "security_export" or "security_defaults" or "security_detect" or "fixture_counts";
    public event Action<JsonElement>? Event;
    public event Action? Disconnected;
    public bool IsFixture { get; }
    public bool HasNativeTray { get; }
    public string Root { get; }
    public BackendClient(string root, bool startup, string? fixture, bool fixtureLoad = false, bool fixtureNativeTray = false)
    {
        Root = root; IsFixture = fixture is not null;
        if (fixtureNativeTray && !IsFixture) throw new ArgumentException("原生系統匣測試必須使用 fixture。");
        HasNativeTray = !IsFixture || fixtureNativeTray;
        var python = Path.Combine(root, ".venv", "Scripts", "python.exe");
        if (!File.Exists(python)) throw new IOException("找不到專案 Python 環境。");
        var info = new ProcessStartInfo(python)
        {
            WorkingDirectory = root, UseShellExecute = false, CreateNoWindow = true,
            RedirectStandardInput = true, RedirectStandardOutput = true, RedirectStandardError = true,
            StandardInputEncoding = new UTF8Encoding(false), StandardOutputEncoding = Encoding.UTF8
        };
        info.ArgumentList.Add("-B"); info.ArgumentList.Add("-m"); info.ArgumentList.Add("mcp_local.desktop_identity");
        info.ArgumentList.Add("--build-manifest"); info.ArgumentList.Add(Path.Combine(AppContext.BaseDirectory, "desktop-build.json"));
        if (startup) info.ArgumentList.Add("--startup");
        if (fixture is not null) { info.ArgumentList.Add("--fixture"); info.ArgumentList.Add(Path.GetFullPath(fixture)); }
        if (fixtureLoad) info.ArgumentList.Add("--fixture-load");
        if (fixtureNativeTray) info.ArgumentList.Add("--fixture-native-tray");
        // Never forward ambient credential or old ephemeral channel capabilities.
        info.Environment.Remove("CONTROL_PLANE_API_KEY"); info.Environment.Remove("MCP_LOCAL_REQUEST_MONITOR");
        info.Environment.Remove("MCP_LOCAL_SECURITY_CHANNEL");
        info.Environment.Remove("PYTHONPATH"); info.Environment["PYTHONNOUSERSITE"] = "1";
        process = Process.Start(info) ?? throw new IOException("無法啟動桌面後端。");
    }
    public void Start() { _ = Task.Run(Read); _ = Task.Run(DrainErrors); }
    private async Task DrainErrors()
    {
        // Errors are intentionally not persisted and raw stderr is never shown as a credential-bearing diagnostic.
        char[] buffer = new char[4096];
        try { while (await process.StandardError.ReadAsync(buffer.AsMemory(), lifetime.Token) > 0) { } }
        catch (Exception error) when (error is IOException or OperationCanceledException or ObjectDisposedException) { }
    }
    private async Task Read()
    {
        try
        {
            // Bounded framing; selected captured request + response can exceed one display page.
            var line = new StringBuilder(); char[] buffer = new char[8192];
            while (!lifetime.IsCancellationRequested)
            {
                int count = await process.StandardOutput.ReadAsync(buffer.AsMemory(), lifetime.Token);
                if (count == 0) break;
                for (int i = 0; i < count; i++)
                {
                    if (buffer[i] != '\n')
                    {
                        if (line.Length >= 16 * 1024 * 1024) throw new IOException("後端訊息超出框架上限。");
                        line.Append(buffer[i]); continue;
                    }
                    using var document = JsonDocument.Parse(line.ToString()); line.Clear();
                    var value = document.RootElement;
                    if (value.TryGetProperty("id", out var id) && pending.TryRemove(id.GetInt32(), out var waiter))
                    {
                        CompleteRequest(id.GetInt32(), value.TryGetProperty("error", out var failure) ? failure.GetString() : null);
                        if (value.TryGetProperty("error", out var error)) waiter.TrySetException(new InvalidOperationException(error.GetString()));
                        else waiter.TrySetResult(value.GetProperty("result").Clone());
                    }
                    else if (value.TryGetProperty("event", out _)) Event?.Invoke(value.Clone());
                }
            }
        }
        catch (Exception error) when (error is IOException or JsonException or OperationCanceledException or ObjectDisposedException) { }
        finally
        {
            Interlocked.Exchange(ref closed, 1);
            foreach (var (_, waiter) in pending) waiter.TrySetException(new IOException("桌面後端已結束。"));
            pending.Clear(); Disconnected?.Invoke();
        }
    }
    private void CompleteRequest(int id, string? error = null)
    {
        if (names.TryRemove(id, out var name)) queries.TryRemove(name, out _);
        if (Interlocked.CompareExchange(ref mutation, 0, id) == id)
        {
            OperationUnconfirmed = false; OperationError = error; OperationChanged?.Invoke();
        }
    }
    public async Task<JsonElement> Send(string command, object? args = null)
    {
        int id = Interlocked.Increment(ref sequence);
        bool query = IsQuery(command);
        if (query)
        {
            if (!queries.TryAdd(command, id)) throw new TimeoutException("後端未回應；仍在等待先前的唯讀查詢。");
        }
        else
        {
            if (Interlocked.CompareExchange(ref mutation, id, 0) != 0)
                throw new InvalidOperationException("先前操作結果尚未確認，未重送操作。");
            OperationError = null; OperationChanged?.Invoke();
        }
        names[id] = command;
        var waiter = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        pending[id] = waiter;
        // Observe late errors even after the caller's deadline; keep the request until its own reply.
        _ = waiter.Task.ContinueWith(t => _ = t.Exception, TaskContinuationOptions.OnlyOnFaulted);
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(lifetime.Token);
        deadline.CancelAfter(query ? QueryTimeout : OperationTimeout);
        bool sent = false;
        try
        {
            if (Volatile.Read(ref closed) != 0) throw new IOException("桌面後端已結束。");
            var text = JsonSerializer.Serialize(new { id, command, args = args ?? new { } });
            await writer.WaitAsync(deadline.Token);
            sent = true;
            // A partially written frame must finish under the same writer lock. A deadline
            // stops waiting, never cancels/replays the operation or interleaves another frame.
            var write = WriteLocked(text);
            _ = write.ContinueWith(t => _ = t.Exception, TaskContinuationOptions.OnlyOnFaulted);
            await write.WaitAsync(deadline.Token);
            return await waiter.Task.WaitAsync(deadline.Token);
        }
        catch (OperationCanceledException) when (!lifetime.IsCancellationRequested)
        {
            if (waiter.Task.IsCompleted) return await waiter.Task;
            if (!sent) { pending.TryRemove(id, out _); CompleteRequest(id); }
            else if (!query && OperationPending) { OperationUnconfirmed = true; OperationChanged?.Invoke(); }
            throw new TimeoutException(query ? "後端未回應；唯讀查詢已逾時。" : sent ?
                "後端未回應；操作結果尚未確認，請等待原請求回覆，勿重送。" : "等待傳送逾時；操作尚未送出。");
        }
        catch
        {
            if (!sent) { pending.TryRemove(id, out _); CompleteRequest(id); }
            else if (!query && OperationPending) { OperationUnconfirmed = true; OperationChanged?.Invoke(); }
            throw;
        }
    }
    public Task Answer(string token, bool accepted) => Write(JsonSerializer.Serialize(new
    { id = Interlocked.Increment(ref sequence), command = "dialog_answer", token, accepted }));
    private async Task Write(string text)
    {
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(lifetime.Token);
        deadline.CancelAfter(OperationTimeout);
        await writer.WaitAsync(deadline.Token);
        var write = WriteLocked(text);
        _ = write.ContinueWith(t => _ = t.Exception, TaskContinuationOptions.OnlyOnFaulted);
        await write.WaitAsync(deadline.Token);
    }
    private async Task WriteLocked(string text)
    {
        try { await process.StandardInput.WriteLineAsync(text); await process.StandardInput.FlushAsync(); }
        finally { writer.Release(); }
    }
    public void Dispose()
    {
        Interlocked.Exchange(ref closed, 1);
        // Closing stdin asks the backend to stop only its own connection.
        // Do not kill the process tree: independent VM guards must finish recovery.
        try { process.StandardInput.Close(); } catch (IOException) { }
        lifetime.Cancel(); process.Dispose();
    }
}
