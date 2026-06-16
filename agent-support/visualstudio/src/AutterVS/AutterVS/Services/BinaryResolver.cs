using System;
using System.Diagnostics;
using System.IO;
using System.Threading.Tasks;

namespace AutterVS.Services
{
    /// <summary>
    /// Locates the autter binary on the system.
    /// Search order:
    ///   1. %USERPROFILE%\.autter\bin\autter.exe  (production install)
    ///   2. %USERPROFILE%\.autter-local-dev\gitwrap\bin\autter.exe  (nix dev)
    ///   3. PATH lookup via "where autter"
    ///
    /// Modeled after IntelliJ's AutterService.findAutterBinary().
    /// </summary>
    public sealed class BinaryResolver
    {
        private static readonly Version MinVersion = new(1, 0, 23);
        private const int VersionCheckTimeoutMs = 5000;
        private const int PathLookupTimeoutMs = 5000;

        private string? _cachedPath;
        private Version? _cachedVersion;
        private string[]? _lastSearchedPaths;

        public string? ResolvedPath => _cachedPath;
        public Version? ResolvedVersion => _cachedVersion;

        public async Task<string?> ResolveAsync()
        {
            if (_cachedPath != null && File.Exists(_cachedPath))
                return _cachedPath;

            _cachedPath = null;
            _cachedVersion = null;

            var path = await FindBinaryAsync().ConfigureAwait(false);
            if (path == null)
            {
                var searched = _lastSearchedPaths != null ? string.Join(", ", _lastSearchedPaths) : "(none)";
                Trace.WriteLine("[autter] autter not found");
                Trace.WriteLine($"[autter]   Searched locations: {searched}");
                Trace.WriteLine("[autter]   To fix: Install autter from https://autter.dev");
                return null;
            }

            var version = await GetVersionAsync(path).ConfigureAwait(false);
            if (version == null)
            {
                Trace.WriteLine($"[autter] Could not determine autter version at {path}");
                return null;
            }

            if (version < MinVersion)
            {
                Trace.WriteLine($"[autter] autter version {version} is below minimum required version {MinVersion}");
                return null;
            }

            _cachedPath = path;
            _cachedVersion = version;
            Trace.WriteLine($"[autter] Found autter at {path} (version {version})");
            return path;
        }

        public void Reset()
        {
            _cachedPath = null;
            _cachedVersion = null;
        }

        private async Task<string?> FindBinaryAsync()
        {
            var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
            var isWindows = Environment.OSVersion.Platform == PlatformID.Win32NT;

            _lastSearchedPaths = isWindows
                ? new[]
                {
                    Path.Combine(home, ".autter", "bin", "autter.exe"),
                    Path.Combine(home, ".autter-local-dev", "gitwrap", "bin", "autter.exe"),
                }
                : new[]
                {
                    Path.Combine(home, ".autter", "bin", "autter"),
                    Path.Combine(home, ".autter-local-dev", "gitwrap", "bin", "autter"),
                };

            foreach (var candidate in _lastSearchedPaths)
            {
                if (File.Exists(candidate))
                    return candidate;
            }

            Trace.WriteLine("[autter] autter not found in known locations, trying PATH lookup");
            return await TryPathLookupAsync(isWindows).ConfigureAwait(false);
        }

        private static async Task<string?> TryPathLookupAsync(bool isWindows)
        {
            try
            {
                var psi = new ProcessStartInfo
                {
                    FileName = isWindows ? "cmd" : "/bin/sh",
                    Arguments = isWindows ? "/c where autter" : "-l -c \"which autter\"",
                    UseShellExecute = false,
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                    CreateNoWindow = true,
                };

                var result = await RunProcessAsync(psi, PathLookupTimeoutMs).ConfigureAwait(false);
                if (result == null || result.ExitCode != 0) return null;

                var firstLine = result.Stdout.Split(new[] { '\r', '\n' }, StringSplitOptions.RemoveEmptyEntries);
                if (firstLine.Length > 0 && File.Exists(firstLine[0]))
                {
                    Trace.WriteLine($"[autter] Found autter via PATH lookup: {firstLine[0]}");
                    return firstLine[0];
                }
            }
            catch
            {
                Trace.WriteLine("[autter] PATH lookup for autter failed");
            }

            return null;
        }

        private static async Task<Version?> GetVersionAsync(string binaryPath)
        {
            try
            {
                var psi = new ProcessStartInfo
                {
                    FileName = binaryPath,
                    Arguments = "version",
                    UseShellExecute = false,
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                    CreateNoWindow = true,
                };

                var result = await RunProcessAsync(psi, VersionCheckTimeoutMs).ConfigureAwait(false);
                if (result == null)
                {
                    Trace.WriteLine("[autter] autter version check timed out");
                    return null;
                }

                if (result.ExitCode != 0)
                {
                    Trace.WriteLine($"[autter] autter version check failed");
                    Trace.WriteLine($"[autter]   Exit code: {result.ExitCode}");
                    Trace.WriteLine($"[autter]   Stdout: {result.Stdout}");
                    Trace.WriteLine($"[autter]   Stderr: {result.Stderr}");
                    return null;
                }

                return ParseVersion(result.Stdout);
            }
            catch (Exception ex)
            {
                Trace.WriteLine($"[autter] autter version check error: {ex.Message}");
                return null;
            }
        }

        internal static Version? ParseVersion(string versionString)
        {
            var part = versionString.Trim().Split(' ')[0];
            var segments = part.Split('.');
            if (segments.Length < 3) return null;

            if (int.TryParse(segments[0], out var major)
                && int.TryParse(segments[1], out var minor)
                && int.TryParse(segments[2].Split('-', '+')[0], out var patch))
            {
                return new Version(major, minor, patch);
            }

            return null;
        }

        private static async Task<ProcessResult?> RunProcessAsync(ProcessStartInfo psi, int timeoutMs)
        {
            using var proc = Process.Start(psi);
            if (proc == null) return null;

            var outputTask = proc.StandardOutput.ReadToEndAsync();
            var stderrTask = proc.StandardError.ReadToEndAsync();

            if (!await WaitForExitAsync(proc, timeoutMs).ConfigureAwait(false))
            {
                TryKill(proc);
                return null;
            }

            var stdout = (await outputTask.ConfigureAwait(false)).Trim();
            var stderr = (await stderrTask.ConfigureAwait(false)).Trim();
            return new ProcessResult(proc.ExitCode, stdout, stderr);
        }

        private static async Task<bool> WaitForExitAsync(Process proc, int timeoutMs)
        {
            var exited = new TaskCompletionSource<bool>();

            void OnExited(object sender, EventArgs args) => exited.TrySetResult(true);

            try
            {
                proc.EnableRaisingEvents = true;
                proc.Exited += OnExited;

                if (proc.HasExited)
                    return true;

                var completed = await Task.WhenAny(exited.Task, Task.Delay(timeoutMs)).ConfigureAwait(false);
                return completed == exited.Task;
            }
            finally
            {
                proc.Exited -= OnExited;
            }
        }

        private static void TryKill(Process proc)
        {
            try
            {
                if (!proc.HasExited)
                    proc.Kill();
            }
            catch (Exception ex)
            {
                Trace.WriteLine($"[autter] Failed to kill timed-out process: {ex.Message}");
            }
        }

        private sealed class ProcessResult
        {
            public ProcessResult(int exitCode, string stdout, string stderr)
            {
                ExitCode = exitCode;
                Stdout = stdout;
                Stderr = stderr;
            }

            public int ExitCode { get; }
            public string Stdout { get; }
            public string Stderr { get; }
        }
    }
}
