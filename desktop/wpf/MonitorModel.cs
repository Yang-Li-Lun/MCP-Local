using System.ComponentModel;
using System.IO;
using System.Text;
using System.Text.Encodings.Web;
using System.Text.Json;

namespace McpLocal.Desktop;

// Rendering consumes immutable snapshots; Python owns capture and retention limits.
internal interface IMonitorSource : IDisposable
{
    MonitorSnapshot Snapshot { get; }
    void Advance();
    void Clear();
}

internal sealed record MonitorDiagnostics(long LostEvents, long EvictedRecords, string State);
internal sealed record MonitorSnapshot(IReadOnlyList<CaptureRecord> Records, MonitorDiagnostics Diagnostics);
internal sealed record CaptureRecord(
    string Id, long Sequence, DateTimeOffset Time, string Tool, string Status,
    double? ElapsedMs, Payload? Request, Payload? Response,
    string[] RequestNotes, string[] ResponseNotes)
{
    public string? SummaryOverride { get; init; }
    public long Revision { get; init; }
    public bool DetailsLoaded { get; init; } = true;
    public bool Partial { get; init; }
    public JsonElement? RawDocument { get; init; }
}

internal sealed class Payload(string json)
{
    public string Json { get; } = json;
    private readonly Lazy<JsonElement> root = new(() =>
    {
        using var document = JsonDocument.Parse(json);
        return document.RootElement.Clone();
    });
    public JsonElement Root => root.Value;
    public string Pretty => JsonSerializer.Serialize(Root, JsonData.PrettyOptions);
    public static Payload From(object value) => new(JsonSerializer.Serialize(value, JsonData.CompactOptions));
}

internal static class JsonData
{
    public static readonly JsonSerializerOptions CompactOptions = new()
    {
        Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping
    };
    public static readonly JsonSerializerOptions PrettyOptions = new(CompactOptions) { WriteIndented = true };
    public static readonly IReadOnlyDictionary<string, string> StatusNames = new Dictionary<string, string>
    {
        ["running"] = "執行中", ["success"] = "完成", ["error"] = "錯誤",
        ["cancelled"] = "已取消", ["paused"] = "監控已暫停", ["disconnected"] = "已斷線",
        ["ambiguous"] = "ID 重複，無法對應", ["incomplete"] = "紀錄不完整", ["transport_error"] = "傳輸錯誤"
    };
    public static string StatusName(string status) => StatusNames.GetValueOrDefault(status, status);

    public static bool Contains(JsonElement value, string query, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        return value.ValueKind switch
        {
            JsonValueKind.Object => value.EnumerateObject().Any(p =>
                p.Name.Contains(query, StringComparison.OrdinalIgnoreCase) || Contains(p.Value, query, token)),
            JsonValueKind.Array => value.EnumerateArray().Any(p => Contains(p, query, token)),
            JsonValueKind.String => (value.GetString() ?? "").Contains(query, StringComparison.OrdinalIgnoreCase),
            _ => value.GetRawText().Contains(query, StringComparison.OrdinalIgnoreCase)
        };
    }

    public static bool Matches(CaptureRecord row, string query, string tool, string status, CancellationToken token)
    {
        if (tool.Length > 0 && row.Tool != tool || status.Length > 0 && row.Status != status) return false;
        return query.Length == 0 || row.Tool.Contains(query, StringComparison.OrdinalIgnoreCase)
            || row.Request is not null && Contains(row.Request.Root, query, token)
            || row.Response is not null && Contains(row.Response.Root, query, token)
            || row.RequestNotes.Concat(row.ResponseNotes).Any(n => n.Contains(query, StringComparison.OrdinalIgnoreCase));
    }

    public static string Preview(JsonElement value)
    {
        if (value.ValueKind == JsonValueKind.Object) return $"{{…}} {value.EnumerateObject().Count()} 個欄位";
        if (value.ValueKind == JsonValueKind.Array) return $"[…] {value.GetArrayLength()} 個項目";
        var text = value.ValueKind == JsonValueKind.String ? value.GetString() ?? "" : value.GetRawText();
        return text.Length > 100 ? text[..100] + "…" : text;
    }

    public static string Summary(CaptureRecord row)
    {
        if (row.SummaryOverride is not null) return row.SummaryOverride;
        if (row.Request is null) return "未擷取參數";
        var root = row.Request.Root;
        if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("params", out var p)
            || p.ValueKind != JsonValueKind.Object || !p.TryGetProperty("arguments", out var args)) return "未擷取參數";
        if (args.ValueKind != JsonValueKind.Object) return Preview(args);
        return string.Join(", ", args.EnumerateObject().Take(3).Select(p => $"{p.Name}: {Preview(p.Value)}"));
    }

    public static string Export(CaptureRecord row, MonitorDiagnostics diagnostics)
    {
        if (row.RawDocument is { } raw) return JsonSerializer.Serialize(raw, PrettyOptions);
        var record = new Dictionary<string, object?>
        {
            ["id"] = row.Id, ["time"] = row.Time.ToString("O"), ["tool"] = row.Tool,
            ["status"] = row.Status, ["elapsed_ms"] = row.ElapsedMs,
            ["request_notes"] = row.RequestNotes, ["response_notes"] = row.ResponseNotes
        };
        // Missing data stays missing. A captured JSON null remains an explicit null.
        if (row.Request is not null) record["request"] = row.Request.Root;
        if (row.Response is not null) record["response"] = row.Response.Root;
        return JsonSerializer.Serialize(new
        {
            format = "mcp-local-wpf-fixture-v1", synthetic_only = true, record,
            diagnostics = new
            {
                lost_events = diagnostics.LostEvents, evicted_records = diagnostics.EvictedRecords,
                scope = "目前合成歷程的累計計數，非此紀錄專屬。",
                capture = "僅含已提供的內容；省略、截斷與未收到的內容無法還原。"
            }
        }, PrettyOptions);
    }

    public static void WriteNew(string path, string text)
    {
        using var stream = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.None);
        using var writer = new StreamWriter(stream, new UTF8Encoding(false));
        writer.Write(text);
        writer.Write('\n');
    }
}

internal sealed class CaptureRow(CaptureRecord record) : INotifyPropertyChanged
{
    public CaptureRecord Record { get; private set; } = record;
    public string Id => Record.Id;
    public string Time => Record.Time.ToString("HH:mm:ss");
    public string DisplayTime => Time.Replace(":", "\u2009:\u2009");
    public string Tool => Record.Tool;
    public string Summary { get; private set; } = JsonData.Summary(record);
    public string Status => JsonData.StatusName(Record.Status);
    public bool IsError => Record.Status is "error" or "transport_error";
    public string Capture => Record.Partial || Record.RequestNotes.Length + Record.ResponseNotes.Length > 0
        ? "部分內容" : Record.DetailsLoaded && (Record.Request is null || Record.Response is null && Record.Status != "running")
        ? "內容缺失" : "—";
    public bool HasCaptureWarning => Capture is "部分內容" or "內容缺失";
    public string Outcome => Record.Status is "incomplete" or "ambiguous" or "paused" or "disconnected" or "transport_error"
        ? "結果未確認 · " + Status : Status;
    public double? Elapsed => Record.ElapsedMs;
    public string Duration => Elapsed is { } value ? $"{value:N2} ms" : "—";
    public void Update(CaptureRecord next)
    {
        var previous = Record;
        Record = next;
        if (!ReferenceEquals(previous.Request, next.Request) || previous.SummaryOverride != next.SummaryOverride)
        {
            Summary = JsonData.Summary(next);
            PropertyChanged?.Invoke(this, new(nameof(Summary)));
        }
        if (previous.ElapsedMs != next.ElapsedMs)
        {
            PropertyChanged?.Invoke(this, new(nameof(Duration)));
            PropertyChanged?.Invoke(this, new(nameof(Elapsed)));
        }
        if (previous.Status != next.Status || previous.Partial != next.Partial || previous.RequestNotes != next.RequestNotes || previous.ResponseNotes != next.ResponseNotes)
        {
            PropertyChanged?.Invoke(this, new(nameof(Status)));
            PropertyChanged?.Invoke(this, new(nameof(Outcome)));
            PropertyChanged?.Invoke(this, new(nameof(IsError)));
        }
        if (previous != next)
        {
            PropertyChanged?.Invoke(this, new(nameof(Capture)));
            PropertyChanged?.Invoke(this, new(nameof(HasCaptureWarning)));
        }
    }
    public event PropertyChangedEventHandler? PropertyChanged;
}

internal sealed class SyntheticSource : IMonitorSource
{
    private readonly bool arrivals;
    private readonly List<CaptureRecord> records = [];
    private long sequence = 199, evicted;
    private int tick;
    public MonitorSnapshot Snapshot => new(records.ToArray(), new(0, evicted, "合成資料"));
    public SyntheticSource(bool arrivals)
    {
        this.arrivals = arrivals;
        for (int i = 0; i < 200; i++) records.Add(Create(i, !arrivals));
    }
    private static CaptureRecord Create(long i, bool resize)
    {
        var tool = i % 7 == 0 ? "list_directory" : "read_file";
        var request = Payload.From(new { jsonrpc = "2.0", id = i, method = "tools/call",
            @params = new { name = tool, arguments = new { path = $"資料/{i}.txt", line_count = 3 } } });
        Payload? response = Payload.From(new { text = new string('x', i == 199 ? 1048000 : i < 200 ? 128000 : 800) });
        var status = "success";
        string[] notes = [];
        if (!resize)
        {
            if (i == 198) response = Payload.From(new { items = Enumerable.Range(0, 6000).Select(n => new { n, path = "demo/a.txt", ok = true }) });
            if (i % 17 == 0) { status = "error"; response = Payload.From(new { error = new { message = "合成錯誤", code = -32602 } }); }
            if (i % 19 == 0) { status = "incomplete"; notes = ["合成截斷註記：未提供的內容無法還原"]; }
            if (i < 4) { status = "running"; response = null; }
        }
        return new(i.ToString(), i, new DateTimeOffset(2026, 11, 1, 12, 0, 0, TimeSpan.FromHours(8)).AddSeconds(i),
            tool, status, !resize && i == 4 ? null : 1.25 + (resize ? 0 : i % 11), request, response, [], notes);
    }
    public void Advance()
    {
        for (int i = 0; i < Math.Min(4, records.Count); i++)
            records[i] = records[i] with { ElapsedMs = records[i].ElapsedMs + .25 };
        if (arrivals && ++tick % 20 == 0)
        {
            records.Add(Create(++sequence, false));
            if (records.Count > 200) { records.RemoveAt(0); evicted++; }
        }
    }
    public void Clear() { records.Clear(); evicted = 0; }
    public void Dispose() { records.Clear(); }
}
