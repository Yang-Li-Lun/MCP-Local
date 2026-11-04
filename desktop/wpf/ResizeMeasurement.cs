using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Threading;

namespace McpLocal.Desktop;

internal sealed class ResizeMeasurement : IDisposable
{
    private readonly Window window;
    private readonly MonitorView view;
    private readonly DispatcherTimer heartbeat = new() { Interval = TimeSpan.FromMilliseconds(8) };
    private readonly Stopwatch watch = new();
    private readonly List<double> beats = [], renders = [];
    private HwndSource? nativeSource;
    private bool active;
    private string mode = "", result = "";
    private double lastBeat, lastRender;
    private TimeSpan cpuStart;
    private int ticksStart, loadsStart, windowSizes, readerSizes, nativeLoops, dropped;
    internal string State { get; private set; } = "NOT_RUN";

    public ResizeMeasurement(Window window, MonitorView view)
    {
        this.window = window; this.view = view;
        heartbeat.Tick += (_, _) =>
        {
            double now = watch.Elapsed.TotalMilliseconds;
            if (active) Add(beats, now - lastBeat);
            lastBeat = now;
        };
        window.SizeChanged += OnSize;
        view.Response.Reader.SizeChanged += OnReaderSize;
        void Attach()
        {
            nativeSource = HwndSource.FromHwnd(new WindowInteropHelper(window).Handle);
            nativeSource?.AddHook(Hook);
        }
        if (new WindowInteropHelper(window).Handle != 0) Attach();
        else window.SourceInitialized += (_, _) => Attach();
    }
    private nint Hook(nint hwnd, int message, nint wparam, nint lparam, ref bool handled)
    { if (active && message == 0x0231) nativeLoops++; return 0; }
    private void Add(List<double> list, double value) { if (list.Count < 20000) list.Add(value); else dropped++; }
    private void OnSize(object sender, SizeChangedEventArgs e) { if (active) windowSizes++; }
    private void OnReaderSize(object sender, SizeChangedEventArgs e) { if (active) readerSizes++; }
    private void OnRender(object? sender, EventArgs e)
    {
        double now = watch.Elapsed.TotalMilliseconds;
        if (lastRender > 0) Add(renders, now - lastRender);
        lastRender = now;
    }
    public void Start(string kind)
    {
        if (active) Stop();
        mode = kind; beats.Clear(); renders.Clear(); result = "";
        windowSizes = readerSizes = nativeLoops = dropped = 0;
        ticksStart = view.RefreshTicks; loadsStart = view.Request.DocumentLoads + view.Response.DocumentLoads;
        using var process = Process.GetCurrentProcess(); cpuStart = process.TotalProcessorTime;
        lastBeat = lastRender = 0; watch.Restart(); active = true; State = "MEASURING";
        CompositionTarget.Rendering += OnRender; heartbeat.Start();
    }
    public string Stop()
    {
        if (!active) return result;
        active = false; watch.Stop(); heartbeat.Stop(); CompositionTarget.Rendering -= OnRender;
        State = "MEASURED";
        using var process = Process.GetCurrentProcess();
        result = JsonSerializer.Serialize(new
        {
            test = mode, state = State, synthetic_only = true, connections_started = 0,
            records = view.Rows.Count, payload_chars = view.Rows.Sum(r => r.Record.Response?.Json.Length ?? 0),
            framework = RuntimeInformation.FrameworkDescription,
            dpi_scale = VisualTreeHelper.GetDpi(window).DpiScaleX,
            measured_seconds = Math.Round(watch.Elapsed.TotalSeconds, 3),
            cpu_seconds = Math.Round((process.TotalProcessorTime - cpuStart).TotalSeconds, 3),
            working_set_bytes = process.WorkingSet64, window_size_events = windowSizes, reader_size_events = readerSizes,
            native_move_resize_loops = nativeLoops, monitor_refresh_calls = view.RefreshTicks - ticksStart,
            detail_reloads = view.Request.DocumentLoads + view.Response.DocumentLoads - loadsStart,
            selected_id = view.Selected?.Id, heartbeat = Summary(beats), rendering_callbacks = Summary(renders),
            dropped_measurement_samples = dropped, human_acceptance = "NOT_RECORDED",
            caution = "UI heartbeat / CompositionTarget.Rendering / SizeChanged are not presented FPS, input latency, or human acceptance."
        }, JsonData.PrettyOptions);
        return result;
    }
    private static object Summary(List<double> values)
    {
        var sorted = values.Order().ToArray();
        return new { count = sorted.Length, median_ms = sorted.Length == 0 ? 0 : Math.Round((sorted[(sorted.Length - 1) / 2] + sorted[sorted.Length / 2]) / 2, 3),
            p95_ms = sorted.Length == 0 ? 0 : Math.Round(sorted[Math.Min(sorted.Length - 1, (int)(sorted.Length * .95))], 3),
            max_ms = sorted.Length == 0 ? 0 : Math.Round(sorted[^1], 3) };
    }
    public void Dispose()
    {
        if (active) Stop();
        heartbeat.Stop(); nativeSource?.RemoveHook(Hook);
        window.SizeChanged -= OnSize; view.Response.Reader.SizeChanged -= OnReaderSize;
    }
}
