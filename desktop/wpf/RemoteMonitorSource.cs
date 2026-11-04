using System.Text.Json;

namespace McpLocal.Desktop;

internal sealed class RemoteMonitorSource(BackendClient backend) : IMonitorSource
{
    private readonly Dictionary<string, CaptureRecord> records = [];
    private readonly HashSet<(string, long)> loading = [];
    private long sequence;
    private readonly SemaphoreSlim searchGate = new(1);
    public MonitorSnapshot Snapshot { get; private set; } = new([], new(0, 0, "未連線"));
    public bool Enabled { get; private set; }
    public bool Connected { get; private set; }
    public bool Failed { get; private set; }
    internal long CapturedBytes { get; private set; }
    public event Action? Changed;
    public void Update(JsonElement result)
    {
        Enabled = result.GetProperty("enabled").GetBoolean(); Connected = result.GetProperty("connected").GetBoolean();
        Failed = result.GetProperty("failed").GetBoolean();
        CapturedBytes = result.GetProperty("captured_bytes").GetInt64();
        var incoming = result.GetProperty("records").EnumerateArray().ToArray();
        var ids = incoming.Select(r => r.GetProperty("id").GetString()!).ToHashSet();
        foreach (var id in records.Keys.Where(id => !ids.Contains(id)).ToArray()) records.Remove(id);
        foreach (var raw in incoming)
        {
            string id = raw.GetProperty("id").GetString()!;
            long revision = raw.GetProperty("_revision").GetInt64();
            records.TryGetValue(id, out var old);
            if (old?.Revision == revision) continue;
            var row = new CaptureRecord(id, old?.Sequence ?? ++sequence, DateTimeOffset.Parse(raw.GetProperty("time").GetString()!),
                raw.GetProperty("tool").GetString()!, raw.GetProperty("status").GetString()!,
                raw.TryGetProperty("elapsed_ms", out var elapsed) && elapsed.ValueKind == JsonValueKind.Number ? elapsed.GetDouble() : null,
                old?.Request, old?.Response, old?.RequestNotes ?? [], old?.ResponseNotes ?? [])
            { Revision = revision, DetailsLoaded = false, Partial = raw.GetProperty("partial").GetBoolean(), SummaryOverride = raw.GetProperty("summary").GetString() };
            records[id] = row;
        }
        string state = Failed ? "監控異常" : !Connected ? "未連線" : !Enabled ? "已暫停" : result.GetProperty("attached").GetInt32() == 0 ? "等待接入" : "監控中";
        Snapshot = new(records.Values.OrderBy(r => r.Sequence).ToArray(),
            new(result.GetProperty("lost_events").GetInt64(), result.GetProperty("evicted_records").GetInt64(), state));
        Changed?.Invoke();
    }
    public async Task<bool> EnsureDetails(string id, long revision)
    {
        if (!loading.Add((id, revision))) return false;
        try
        {
            var result = await backend.Send("detail", new { record_id = id });
            var document = result.GetProperty("document");
            if (document.ValueKind == JsonValueKind.Null || !records.TryGetValue(id, out var current) || current.Revision != revision) return false;
            var raw = document.GetProperty("record");
            Payload? Read(string part, Payload? old)
            {
                if (!raw.TryGetProperty(part, out var value)) return null;
                var text = value.GetRawText();
                return old?.Json == text ? old : new Payload(text);
            }
            string[] Notes(string part) => raw.TryGetProperty(part, out var notes) ? notes.EnumerateArray().Select(n =>
                n.ValueKind == JsonValueKind.String ? n.GetString()! : n.GetRawText()).ToArray() : [];
            records[id] = current with { Request = Read("request", current.Request), Response = Read("response", current.Response),
                RequestNotes = Notes("request_notes"), ResponseNotes = Notes("response_notes"), RawDocument = document.Clone(), DetailsLoaded = true };
            Snapshot = Snapshot with { Records = records.Values.OrderBy(r => r.Sequence).ToArray() };
            return true;
        }
        finally { loading.Remove((id, revision)); }
    }
    public async Task<HashSet<string>> Search(string query, string tool, string status, CancellationToken token)
    {
        await searchGate.WaitAsync(token);
        try
        {
            token.ThrowIfCancellationRequested();
            var result = await backend.Send("monitor_search", new { query, tool, status });
            token.ThrowIfCancellationRequested();
            return result.GetProperty("ids").EnumerateArray().Select(v => v.GetString()!).ToHashSet();
        }
        finally { searchGate.Release(); }
    }
    public Task<JsonElement> Toggle() => backend.Send("toggle_request_monitor");
    public void Advance() { }
    public async void Clear()
    {
        try { await backend.Send("clear_request_monitor"); Update(await backend.Send("monitor")); }
        catch (Exception) { /* Polling presents connection failure in the owning desktop. */ }
    }
    public void Dispose() { records.Clear(); loading.Clear(); }
}
