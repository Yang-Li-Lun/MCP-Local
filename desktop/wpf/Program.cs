using System.IO;
using System.Windows;

namespace McpLocal.Desktop;

internal static class Program
{
    [STAThread]
    private static int Main(string[] args)
    {
        bool startup = false, test = false, benchmark = false, integration = false, desktopBenchmark = false, fixtureLoad = false, fixtureNativeTray = false, watchOwner = false;
        string? fixture = null, report = null;
        try
        {
            for (int i = 0; i < args.Length; i++)
            {
                switch (args[i])
                {
                    case "--startup": startup = true; break;
                    case "--fixture": fixture = args[++i]; break;
                    case "--self-test": test = true; break;
                    case "--benchmark": benchmark = true; break;
                    case "--integration-test": integration = true; break;
                    case "--desktop-benchmark": desktopBenchmark = true; fixtureLoad = true; break;
                    case "--fixture-load": fixtureLoad = true; break;
                    case "--fixture-native-tray": fixtureNativeTray = true; break;
                    case "--watch-owner-stdin": watchOwner = true; break;
                    case "--report": report = Path.GetFullPath(args[++i]); break;
                    default: throw new ArgumentException("不支援的桌面參數。");
                }
            }
            if ((test || benchmark || integration || desktopBenchmark) && (report is null || File.Exists(report) || !Directory.Exists(Path.GetDirectoryName(report))) || test && benchmark || (integration || fixtureLoad || fixtureNativeTray) && (fixture is null || test || benchmark))
                throw new ArgumentException("驗證須指定既有目錄內的新 --report 路徑。");
            var app = new Application { ShutdownMode = ShutdownMode.OnMainWindowClose };
            DesignSystem.Initialize(app);
            if (test || benchmark)
            {
                var view = new MonitorView(new SyntheticSource(test));
                var host = new Window { Title = "MCP-Local｜WPF 隔離控制項驗證", Content = view, Width = 1120, Height = 880, MinWidth = 540, MinHeight = 400 };
                DesignSystem.Window(host);
                int exit = 0;
                host.Loaded += async (_, _) =>
                {
                    try
                    {
                        await host.Dispatcher.InvokeAsync(() => { }, System.Windows.Threading.DispatcherPriority.ContextIdle);
                        if (test) await MonitorChecks.Run(view, report!); else await MonitorChecks.Benchmark(view, report!);
                    }
                    catch (Exception error)
                    { exit = 1; JsonData.WriteNew(report!, System.Text.Json.JsonSerializer.Serialize(new { state = "FAIL", error = error.ToString() }, JsonData.PrettyOptions)); }
                    finally { view.Dispose(); host.Close(); }
                };
                app.Run(host); return exit;
            }
            var root = new DirectoryInfo(AppContext.BaseDirectory);
            while (root is not null && !File.Exists(Path.Combine(root.FullName, "local_files_mcp.py"))) root = root.Parent;
            if (root is null) throw new IOException("找不到 MCP-Local 專案／部署根目錄。");
            var backend = new BackendClient(root.FullName, startup, fixture, fixtureLoad, fixtureNativeTray);
            var window = new DesktopWindow(backend);
            if (watchOwner) window.Loaded += (_, _) => _ = Task.Run(async () =>
            {
                using var input = Console.OpenStandardInput();
                byte[] buffer = new byte[1];
                try { while (await input.ReadAsync(buffer) != 0) { } }
                catch (IOException) { }
                await window.Dispatcher.InvokeAsync(window.CloseFromOwnerExit);
            });
            int integrationExit = 0;
            if (integration || desktopBenchmark) window.Loaded += async (_, _) =>
            {
                try
                {
                    if (integration) await DesktopChecks.Run(window, backend, fixture!, report!);
                    else await DesktopChecks.Benchmark(window, report!);
                }
                catch (Exception error)
                { integrationExit = 1; JsonData.WriteNew(report!, System.Text.Json.JsonSerializer.Serialize(new { state = "FAIL", error = error.ToString() }, JsonData.PrettyOptions)); }
                finally { await window.Run("quit"); }
            };
            else if (fixtureLoad) window.Loaded += async (_, _) =>
            {
                await DesktopChecks.PrepareResize(window);
                var measurement = new ResizeMeasurement(window, window.Monitor);
                window.PreviewKeyDown += (_, key) =>
                {
                    if (key.Key == System.Windows.Input.Key.F9) { measurement.Start("MANUAL_BORDER_RESIZE"); key.Handled = true; }
                    if (key.Key == System.Windows.Input.Key.F10)
                    {
                        string result = measurement.Stop();
                        if (result.Length > 0)
                        {
                            var save = MonitorView.NewSaveDialog("儲存縮放量測（非 FPS）", "wpf-manual-resize.json");
                            if (save.ShowDialog(window) == true)
                            {
                                try { JsonData.WriteNew(save.FileName, result); }
                                catch (IOException) { MessageBox.Show(window, "量測未保存；請指定新的可寫檔名。", "縮放量測"); }
                            }
                        }
                        key.Handled = true;
                    }
                };
                window.Closed += (_, _) => measurement.Dispose();
            };
            app.Run(window); return integrationExit;
        }
        catch (Exception error)
        {
            if (report is not null && !File.Exists(report)) JsonData.WriteNew(report, System.Text.Json.JsonSerializer.Serialize(new { state = "FAIL", error = error.ToString() }, JsonData.PrettyOptions));
            else MessageBox.Show(error.Message, "MCP-Local 未啟動", MessageBoxButton.OK, MessageBoxImage.Error);
            return 1;
        }
    }
}
