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
    private readonly CancellationTokenSource lifetime = new();
    private int sequence, closed;
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
        info.ArgumentList.Add("-B"); info.ArgumentList.Add("-m"); info.ArgumentList.Add("mcp_local.desktop_bridge");
        if (startup) info.ArgumentList.Add("--startup");
        if (fixture is not null) { info.ArgumentList.Add("--fixture"); info.ArgumentList.Add(Path.GetFullPath(fixture)); }
        if (fixtureLoad) info.ArgumentList.Add("--fixture-load");
        if (fixtureNativeTray) info.ArgumentList.Add("--fixture-native-tray");
        // Never forward ambient credential or old ephemeral channel capabilities.
        info.Environment.Remove("CONTROL_PLANE_API_KEY"); info.Environment.Remove("MCP_LOCAL_REQUEST_MONITOR");
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
    public async Task<JsonElement> Send(string command, object? args = null)
    {
        int id = Interlocked.Increment(ref sequence);
        var waiter = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        pending[id] = waiter;
        try
        {
            if (Volatile.Read(ref closed) != 0) throw new IOException("桌面後端已結束。");
            await Write(JsonSerializer.Serialize(new { id, command, args = args ?? new { } }));
            return await waiter.Task.WaitAsync(lifetime.Token);
        }
        catch { pending.TryRemove(id, out _); throw; }
    }
    public Task Answer(string token, bool accepted) => Write(JsonSerializer.Serialize(new
    { id = Interlocked.Increment(ref sequence), command = "dialog_answer", token, accepted }));
    private async Task Write(string text)
    {
        await writer.WaitAsync(lifetime.Token);
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
