using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Threading.Tasks;
using AutterVS.Models;

namespace AutterVS.Services
{
    /// <summary>
    /// Spawns the autter CLI to create checkpoints.
    /// All methods are fire-and-forget safe -- they never throw.
    ///
    /// Modeled after IntelliJ's AutterService.kt checkpoint methods.
    /// </summary>
    public sealed class CheckpointService
    {
        private const int CheckpointTimeoutMs = 30_000;

        private readonly BinaryResolver _resolver;
        private readonly string _sessionId;

        /// <summary>
        /// Global singleton so MEF-created components (TextBufferListener) can access it
        /// without manual wiring. Set by AutterPackage during initialization.
        /// </summary>
        public static CheckpointService? Current { get; set; }

        public CheckpointService(BinaryResolver resolver)
        {
            _resolver = resolver;
            _sessionId = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds().ToString();
        }

        public string SessionId => _sessionId;

        /// <summary>
        /// Send a human (before_edit) checkpoint via agent-v1 preset.
        /// </summary>
        public Task<bool> SendBeforeEditAsync(string repoRoot, string[] willEditPaths, Dictionary<string, string>? dirtyFiles)
        {
            var input = new HumanInput
            {
                RepoWorkingDir = repoRoot,
                WillEditFilepaths = new List<string>(willEditPaths),
                DirtyFiles = dirtyFiles,
            };

            return RunCheckpointAsync("agent-v1", "human", input.ToJson(), repoRoot);
        }

        /// <summary>
        /// Send an AI agent (after_edit) checkpoint via agent-v1 preset.
        /// </summary>
        public Task<bool> SendAfterEditAsync(string repoRoot, string[] editedPaths, string agentName, Dictionary<string, string>? dirtyFiles)
        {
            var input = new AiAgentInput
            {
                RepoWorkingDir = repoRoot,
                EditedFilepaths = new List<string>(editedPaths),
                AgentName = agentName,
                Model = "unknown",
                ConversationId = _sessionId,
                DirtyFiles = dirtyFiles,
            };

            return RunCheckpointAsync("agent-v1", $"ai_agent ({agentName})", input.ToJson(), repoRoot);
        }

        /// <summary>
        /// Send a known_human checkpoint.
        /// </summary>
        public Task<bool> SendKnownHumanAsync(string repoRoot, string editorVersion, string extensionVersion,
            List<string> editedPaths, Dictionary<string, string> dirtyFiles)
        {
            var input = new KnownHumanInput
            {
                Editor = "visualstudio",
                EditorVersion = editorVersion,
                ExtensionVersion = extensionVersion,
                Cwd = repoRoot,
                EditedFilepaths = editedPaths,
                DirtyFiles = dirtyFiles,
            };

            return RunCheckpointAsync("known_human", "known_human", input.ToJson(), repoRoot);
        }

        private async Task<bool> RunCheckpointAsync(string preset, string inputType, string stdinJson, string cwd)
        {
            var binaryPath = await _resolver.ResolveAsync();
            if (binaryPath == null)
            {
                Trace.WriteLine("[autter] Skipping checkpoint -- autter not available");
                return false;
            }

            try
            {
                var args = $"checkpoint {preset} --hook-input stdin";

                Trace.WriteLine($"[autter] Creating checkpoint ({preset}): {inputType}");

                var psi = new ProcessStartInfo
                {
                    FileName = binaryPath,
                    Arguments = args,
                    WorkingDirectory = cwd,
                    UseShellExecute = false,
                    RedirectStandardInput = true,
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                    CreateNoWindow = true,
                };

                using var proc = Process.Start(psi);
                if (proc == null)
                {
                    Trace.WriteLine("[autter] Failed to start autter process");
                    return false;
                }

                await proc.StandardInput.WriteAsync(stdinJson);
                proc.StandardInput.Close();

                var stdoutTask = proc.StandardOutput.ReadToEndAsync();
                var stderrTask = proc.StandardError.ReadToEndAsync();

                var completed = proc.WaitForExit(CheckpointTimeoutMs);
                if (!completed)
                {
                    proc.Kill();
                    Trace.WriteLine($"[autter] Checkpoint timed out after {CheckpointTimeoutMs}ms");
                    return false;
                }

                var stdout = (await stdoutTask).Trim();
                var stderr = (await stderrTask).Trim();

                if (proc.ExitCode != 0)
                {
                    Trace.WriteLine($"[autter] Checkpoint failed");
                    Trace.WriteLine($"[autter]   Command: {binaryPath} {args}");
                    Trace.WriteLine($"[autter]   Exit code: {proc.ExitCode}");
                    Trace.WriteLine($"[autter]   Stdout: {stdout}");
                    Trace.WriteLine($"[autter]   Stderr: {stderr}");
                    return false;
                }

                Trace.WriteLine($"[autter] Checkpoint created successfully ({inputType})");
                if (stdout.Length > 0)
                    Trace.WriteLine($"[autter]   Output: {stdout}");

                return true;
            }
            catch (Exception ex)
            {
                Trace.WriteLine($"[autter] Checkpoint error: {ex.Message}");
                return false;
            }
        }
    }
}
