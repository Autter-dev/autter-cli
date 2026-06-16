using System;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;
using AutterVS.Listeners;
using AutterVS.Services;
using Microsoft.VisualStudio;
using Microsoft.VisualStudio.Shell;
using Microsoft.VisualStudio.Shell.Interop;

namespace AutterVS
{
    /// <summary>
    /// The main package entry point for the autter Visual Studio extension.
    ///
    /// Responsibilities:
    ///   - Resolve the autter binary on startup
    ///   - Set CheckpointService.Current for MEF-exported listeners
    ///   - Subscribe to Running Document Table events for save-based known_human checkpoints
    ///   - Show an info bar if autter is not installed
    /// </summary>
    [PackageRegistration(UseManagedResourcesOnly = true, AllowsBackgroundLoading = true)]
    [ProvideAutoLoad(VSConstants.UICONTEXT.NoSolution_string, PackageAutoLoadFlags.BackgroundLoad)]
    [ProvideAutoLoad(VSConstants.UICONTEXT.SolutionExists_string, PackageAutoLoadFlags.BackgroundLoad)]
    [ProvideAutoLoad(VSConstants.UICONTEXT.SolutionHasMultipleProjects_string, PackageAutoLoadFlags.BackgroundLoad)]
    [ProvideAutoLoad(VSConstants.UICONTEXT.SolutionHasSingleProject_string, PackageAutoLoadFlags.BackgroundLoad)]
    [Guid(PackageGuidString)]
    public sealed class AutterPackage : AsyncPackage
    {
        public const string PackageGuidString = "B2C3D4E5-F6A7-8901-BCDE-F12345678901";
        private const string ExtensionVersion = "0.1.0";

        private BinaryResolver? _binaryResolver;
        private CheckpointService? _checkpointService;
        private DocumentSaveListener? _saveListener;
        private uint _rdtCookie;

        protected override async Task InitializeAsync(CancellationToken cancellationToken, IProgress<ServiceProgressData> progress)
        {
            try
            {
                await base.InitializeAsync(cancellationToken, progress);
                await JoinableTaskFactory.SwitchToMainThreadAsync(cancellationToken);

                System.Diagnostics.Trace.WriteLine("[autter] AutterPackage initializing...");

                _binaryResolver = new BinaryResolver();
                var binaryPath = await _binaryResolver.ResolveAsync();

                if (binaryPath == null)
                {
                    ShowInfoBar("autter is not installed. Visit https://autter.dev to install it.");
                    return;
                }

                _checkpointService = new CheckpointService(_binaryResolver);
                CheckpointService.Current = _checkpointService;

                SubscribeToSaveEvents();

                System.Diagnostics.Trace.WriteLine("[autter] AutterPackage initialized successfully.");
            }
            catch (Exception ex)
            {
                System.Diagnostics.Trace.WriteLine($"[autter] FATAL: InitializeAsync failed: {ex}");
            }
        }

        private void SubscribeToSaveEvents()
        {
            ThreadHelper.ThrowIfNotOnUIThread();

            var rdt = GetService(typeof(SVsRunningDocumentTable)) as IVsRunningDocumentTable;
            if (rdt == null)
            {
                System.Diagnostics.Trace.WriteLine("[autter] Could not get Running Document Table");
                return;
            }

            var vsVersion = GetVisualStudioVersion();
            _saveListener = new DocumentSaveListener(_checkpointService!, vsVersion, ExtensionVersion);

            var rdtEvents = new RdtSaveEventSink(_saveListener, rdt);
            rdt.AdviseRunningDocTableEvents(rdtEvents, out _rdtCookie);

            System.Diagnostics.Trace.WriteLine("[autter] Subscribed to document save events");
        }

        private static string GetVisualStudioVersion()
        {
            try
            {
                ThreadHelper.ThrowIfNotOnUIThread();
                var shell = Package.GetGlobalService(typeof(SVsShell)) as IVsShell;
                if (shell != null)
                {
                    shell.GetProperty((int)__VSSPROPID5.VSSPROPID_ReleaseVersion, out var version);
                    return version?.ToString() ?? "unknown";
                }
            }
            catch
            {
                // Best-effort
            }

            return "unknown";
        }

        private void ShowInfoBar(string message)
        {
            System.Diagnostics.Trace.WriteLine($"[autter] Info: {message}");
            // TODO: Implement VS info bar notification via IVsInfoBarUIFactory
        }

        protected override void Dispose(bool disposing)
        {
            ThreadHelper.ThrowIfNotOnUIThread();
            if (disposing)
            {
                _saveListener?.Dispose();

                if (_rdtCookie != 0)
                {
                    var rdt = GetService(typeof(SVsRunningDocumentTable)) as IVsRunningDocumentTable;
                    rdt?.UnadviseRunningDocTableEvents(_rdtCookie);
                }
            }

            base.Dispose(disposing);
        }
    }

    /// <summary>
    /// Bridges IVsRunningDocTableEvents3 to our DocumentSaveListener.
    /// Only OnAfterSave is meaningful; all other events are no-ops.
    /// </summary>
    internal sealed class RdtSaveEventSink : IVsRunningDocTableEvents3
    {
        private readonly DocumentSaveListener _listener;
        private readonly IVsRunningDocumentTable _rdt;

        public RdtSaveEventSink(DocumentSaveListener listener, IVsRunningDocumentTable rdt)
        {
            _listener = listener;
            _rdt = rdt;
        }

        public int OnAfterSave(uint docCookie)
        {
            Microsoft.VisualStudio.Shell.ThreadHelper.ThrowIfNotOnUIThread();

            var filePath = GetDocumentPath(docCookie);
            if (filePath != null)
                _listener.OnDocumentSaved(filePath);

            return VSConstants.S_OK;
        }

        private string? GetDocumentPath(uint docCookie)
        {
            Microsoft.VisualStudio.Shell.ThreadHelper.ThrowIfNotOnUIThread();

            _rdt.GetDocumentInfo(
                docCookie,
                out _,         // pgrfRDTFlags
                out _,         // pdwReadLocks
                out _,         // pdwEditLocks
                out var path,  // pbstrMkDocument
                out _,         // ppHier
                out _,         // pitemid
                out _);        // ppunkDocData

            return path;
        }

        public int OnAfterFirstDocumentLock(uint docCookie, uint dwRDTLockType, uint dwReadLocksRemaining, uint dwEditLocksRemaining) => VSConstants.S_OK;
        public int OnBeforeLastDocumentUnlock(uint docCookie, uint dwRDTLockType, uint dwReadLocksRemaining, uint dwEditLocksRemaining) => VSConstants.S_OK;
        public int OnAfterAttributeChange(uint docCookie, uint grfAttribs) => VSConstants.S_OK;
        public int OnBeforeDocumentWindowShow(uint docCookie, int fFirstShow, IVsWindowFrame pFrame) => VSConstants.S_OK;
        public int OnAfterDocumentWindowHide(uint docCookie, IVsWindowFrame pFrame) => VSConstants.S_OK;
        public int OnAfterAttributeChangeEx(uint docCookie, uint grfAttribs, IVsHierarchy pHierOld, uint itemidOld, string pszMkDocumentOld, IVsHierarchy pHierNew, uint itemidNew, string pszMkDocumentNew) => VSConstants.S_OK;
        public int OnBeforeSave(uint docCookie) => VSConstants.S_OK;
    }
}
