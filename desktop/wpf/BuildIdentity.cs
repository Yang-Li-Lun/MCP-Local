using System.Diagnostics;
using System.IO;
using System.Reflection;
using System.Reflection.Metadata;
using System.Reflection.PortableExecutable;
using System.Security.Cryptography;
using System.Text.Json;

namespace McpLocal.Desktop;

internal static class BuildIdentity
{
    internal static readonly string StartId = Guid.NewGuid().ToString("N");
    private static readonly string Started = Process.GetCurrentProcess().StartTime.ToUniversalTime().ToString("O");
    private static readonly Assembly Assembly = typeof(BuildIdentity).Assembly;
    private static readonly JsonElement Embedded = ReadEmbedded();
    private static JsonElement ReadEmbedded()
    {
        using var stream = Assembly.GetManifestResourceStream("desktop-build.json");
        if (stream is null) return default;
        using var document = JsonDocument.Parse(stream); return document.RootElement.Clone();
    }
    private static object Field(string name) => Embedded.ValueKind == JsonValueKind.Object && Embedded.TryGetProperty(name, out var value) ? value : "未知";
    internal static object Current => new
    {
        process_id = Environment.ProcessId, process_start_id = StartId, started_at_utc = Started,
        loaded_assembly_mvid = Assembly.ManifestModule.ModuleVersionId,
        frontend_build_id = Field("frontend_build_id"), source = Field("source"),
        built_at_utc = Field("built_at_utc"), deployment_id = Field("deployment_id"),
        loaded_location = Assembly.Location,
        disk_deployment_now = DiskDeployment(),
        scope = "建置識別取自已載入組件內嵌資源；磁碟成品雜湊另列，不能代表既有程序已更新。"
    };
    private static object DiskDeployment()
    {
        try
        {
            using var document = JsonDocument.Parse(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "desktop-build.json")));
            var root = document.RootElement;
            using var assemblyFile = File.OpenRead(Assembly.Location);
            using var pe = new PEReader(assemblyFile);
            var metadata = pe.GetMetadataReader();
            bool moduleMatches = metadata.GetGuid(metadata.GetModuleDefinition().Mvid) == Assembly.ManifestModule.ModuleVersionId;
            var hashes = new Dictionary<string, string>();
            foreach (var item in root.GetProperty("artifacts").EnumerateObject())
            {
                if (Path.GetFileName(item.Name) != item.Name || !item.Name.StartsWith("McpLocal.Desktop.", StringComparison.Ordinal)) continue;
                using var file = File.OpenRead(Path.Combine(AppContext.BaseDirectory, item.Name));
                hashes[item.Name] = Convert.ToHexString(SHA256.HashData(file)).ToLowerInvariant();
            }
            return new { deployment_id = root.GetProperty("deployment_id").GetString(), artifacts_sha256 = hashes,
                matches_manifest = root.GetProperty("artifacts").EnumerateObject().All(p => hashes.GetValueOrDefault(p.Name) == p.Value.GetString()),
                loaded_mvid_matches_disk = moduleMatches,
                matches_loaded_build = moduleMatches && root.GetProperty("frontend_build_id").ToString() == Field("frontend_build_id").ToString() };
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException or KeyNotFoundException or BadImageFormatException or InvalidOperationException) { return "未知"; }
    }
}
