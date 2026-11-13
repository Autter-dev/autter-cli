#!/usr/bin/env node
'use strict';

// npm bootstrapper for the autter CLI.
//
// npm distributes a thin launcher, not the binary itself: this script fetches
// the official release binary for the current platform into ~/.autter/bin --
// the same location the curl/PowerShell installers use and the location the
// autter daemon self-upgrades in place. Keeping one canonical binary path
// means hooks, the daemon, and every install method stay in agreement, and
// the npm global bin directory (already on PATH) provides the `autter`
// command via bin/autter.js.
//
// The package version is stamped to the CLI release tag at publish time
// (release.yml runs `npm version <tag>`), so `npm i -g @autter/cli@1.7.0`
// fetches the v1.7.0 binaries. The checked-in version is the placeholder
// 0.0.0-dev, which is never published: an install from a checkout resolves
// the latest release instead.
//
// Escape hatches:
//   AUTTER_NPM_SKIP_DOWNLOAD=1  skip the postinstall download entirely
//   AUTTER_RELEASE_TAG=vX.Y.Z   install a specific release (parity with install.sh)
//   AUTTER_NO_INSTALL_PING=1    disable the anonymous install ping

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const pkg = require('./package.json');

const REPO = 'autter-dev/autter-cli';
// Public write-only project token, same one embedded in install.sh; used for
// a single anonymous install-count event. Opt out with AUTTER_NO_INSTALL_PING=1.
const INSTALL_PING_API_KEY = 'phc_aWveMd1bPhuEYtFnCS1G2IHgln3iGQqjfIdkfnuolxI';
const INSTALL_PING_HOST = 'https://us.i.posthog.com';

const OS_NAMES = { darwin: 'macos', linux: 'linux', win32: 'windows' };
const CPU_NAMES = { x64: 'x64', arm64: 'arm64' };

function assetName() {
  const osName = OS_NAMES[process.platform];
  const cpuName = CPU_NAMES[process.arch];
  if (!osName || !cpuName) {
    throw new Error(
      `no prebuilt autter binary for ${process.platform}/${process.arch}. ` +
        'See https://github.com/autter-dev/autter-cli#installation for supported platforms.'
    );
  }
  return `autter-${osName}-${cpuName}${osName === 'windows' ? '.exe' : ''}`;
}

function installDir() {
  return path.join(os.homedir(), '.autter', 'bin');
}

function binaryDest() {
  return path.join(installDir(), process.platform === 'win32' ? 'autter.exe' : 'autter');
}

// ~/.autter/npm-install.json says the binary in ~/.autter/bin was installed
// by this package, so npm picks its version: the autter background service
// then never replaces it, and `autter upgrade` names the npm command instead
// (src/commands/upgrade.rs reads it). The shell and PowerShell installers
// remove it when they install over an npm install.
function installMarkerPath(home = os.homedir()) {
  return path.join(home, '.autter', 'npm-install.json');
}

function readInstallMarker(home = os.homedir()) {
  try {
    return JSON.parse(fs.readFileSync(installMarkerPath(home), 'utf8'));
  } catch {
    return null;
  }
}

function writeInstallMarker({ home = os.homedir(), packageVersion = pkg.version } = {}) {
  const file = installMarkerPath(home);
  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(
      file,
      JSON.stringify(
        { installer: 'npm', package: pkg.name, package_version: packageVersion },
        null,
        2,
      ) + '\n',
    );
    return true;
  } catch {
    return false;
  }
}

// Whether the launcher has to (re)install before it runs the binary: the
// binary is missing, or this published package version has not installed its
// binary yet. The second happens when npm skipped the postinstall script
// (npm 10+ with scripts off, --ignore-scripts, pnpm): `npm install -g
// @autter/cli@<version>` then changed nothing but the launcher.
function launcherNeedsInstall({ binaryExists, marker, packageVersion = pkg.version }) {
  if (!binaryExists) return true;
  if (packageVersion === DEV_VERSION) return false;
  return !marker || marker.package_version !== packageVersion;
}

// The version checked in to the repository. release.yml replaces it with the
// release version before `npm publish`.
const DEV_VERSION = '0.0.0-dev';

function releaseTag() {
  const override = process.env.AUTTER_RELEASE_TAG;
  if (override && override !== 'latest') return override;
  if (pkg.version === DEV_VERSION) return 'latest';
  return `v${pkg.version}`;
}

// "latest" is resolved to the tag it names before anything is downloaded, the
// way install.sh and install.ps1 do: the binary and checksums.txt then come
// from one release, and the checksum policy (which needs a version) applies.
// GitHub answers /releases/latest with a redirect to /releases/tag/<tag>.
async function resolveLatestTag(fetchImpl = fetch) {
  const url = `https://github.com/${REPO}/releases/latest`;
  const res = await fetchImpl(url, { method: 'HEAD', redirect: 'manual' });
  const target = (res.headers && res.headers.get('location')) || res.url || '';
  const match = /\/releases\/tag\/([^/?#]+)/.exec(target);
  if (!match) {
    throw new Error(`could not resolve the latest release (HTTP ${res.status}) from ${url}`);
  }
  return decodeURIComponent(match[1]);
}

function downloadUrl(tag, file) {
  return `https://github.com/${REPO}/releases/download/${tag}/${file}`;
}

async function fetchBuffer(url) {
  const res = await fetch(url, { redirect: 'follow' });
  if (!res.ok) {
    throw new Error(`download failed (HTTP ${res.status}) for ${url}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

// Verify against the release's checksums.txt (`<sha256>  <asset>` lines).
// Releases before v1.6.8 predate checksum publication and are the only
// releases for which verification may be skipped.
async function verifyChecksum(buf, asset, tag) {
  const version = tag.replace(/^v/, '').split(/[+-]/, 1)[0];
  const versionMatch = /^(\d+)\.(\d+)\.(\d+)/.exec(version);
  if (!versionMatch) {
    throw new Error(`invalid release tag ${tag}; refusing to determine checksum policy`);
  }
  const [, majorText, minorText, patchText] = versionMatch;
  const major = Number(majorText);
  const minor = Number(minorText);
  const patch = Number(patchText);
  const checksumsRequired = major > 1 || (major === 1 && (minor > 6 || (minor === 6 && patch >= 8)));
  let checksums;
  try {
    checksums = (await fetchBuffer(downloadUrl(tag, 'checksums.txt'))).toString('utf8');
  } catch (err) {
    if (!checksumsRequired) {
      console.warn(`autter: release ${tag} predates checksum publishing; skipping checksum verification`);
      return;
    }
    throw new Error(`could not fetch checksums.txt for release ${tag}; refusing to install an unverifiable binary (${err.message})`);
  }
  const entry = checksums
    .split('\n')
    .map((line) => line.trim().split(/\s+/))
    .find((parts) => parts[1] === asset);
  if (!entry) {
    throw new Error(`no checksum entry for ${asset} in release ${tag}`);
  }
  const actual = crypto.createHash('sha256').update(buf).digest('hex');
  if (actual !== entry[0]) {
    throw new Error(`checksum mismatch for ${asset}: expected ${entry[0]}, got ${actual}`);
  }
}

function checkGit() {
  try {
    execFileSync('git', ['--version'], { encoding: 'utf8', timeout: 10_000, stdio: 'pipe' });
  } catch {
    throw new Error(
      'git is required but not found. Install git 2.22 or newer, then re-run: npm install -g @autter/cli'
    );
  }
}

function checkLinuxGlibc() {
  if (process.platform !== 'linux') return;
  try {
    const out = execFileSync('ldd', ['--version'], { encoding: 'utf8', timeout: 10_000 });
    const match = out.match(/(\d+)\.(\d+)/);
    if (!match) return;
    const major = Number(match[1]);
    const minor = Number(match[2]);
    if (major < 2 || (major === 2 && minor < 35)) {
      throw new Error(
        `Unsupported glibc version (${major}.${minor}). autter requires glibc 2.35+ (Ubuntu 22.04+, Debian 12+, Fedora 36+). ` +
          'On Ubuntu 20.04 / older WSL2, use a newer distro or run inside ubuntu:22.04 Docker.'
      );
    }
  } catch (err) {
    if (err.message?.includes('Unsupported glibc')) throw err;
    // ldd missing — rely on post-download binary verify
  }
}

// What to tell someone when the operating system refuses to start the
// native binary. Node reports these as an error `code` on the spawn, and the
// bare message ("spawnSync ...autter.exe UNKNOWN") says nothing about the
// cause or the fix. Returns extra lines to print, or [] when there is
// nothing platform-specific to add.
function spawnFailureHelp(err, { platform = process.platform, bin = binaryDest() } = {}) {
  const code = err && err.code;
  if (platform !== 'win32') {
    if (code === 'EACCES') {
      return [`${bin} is not executable. Fix: chmod +x "${bin}"`];
    }
    return [];
  }
  const dir = path.win32.dirname(bin);
  const seeWindowsError = `See Windows' own message by running it directly in PowerShell: & '${bin}' --version`;
  const allowIt =
    'If Windows Security (or another antivirus) blocked it: open Windows Security > Virus & threat protection > ' +
    `Protection history and allow autter.exe, or add an exclusion for ${dir}.`;
  const reinstall =
    `To download it again, delete ${bin} and run the same autter command once more, ` +
    'or install with PowerShell from a normal (not Administrator) window: irm https://api.autter.dev/install.ps1 | iex';
  switch (code) {
    case 'UNKNOWN':
      return [
        'Windows would not start the file. The usual causes: an antivirus blocked or quarantined it, ' +
          'the download is incomplete, or the file is for another processor type ' +
          `(this Node is ${process.arch}).`,
        seeWindowsError,
        allowIt,
        reinstall,
      ];
    case 'EACCES':
    case 'EPERM':
      return [
        'Windows denied access to the file. The usual causes: an antivirus is holding or blocking it, ' +
          'or it was installed from an Administrator window and your account cannot run it.',
        seeWindowsError,
        allowIt,
        reinstall,
      ];
    case 'ENOENT':
      return [
        'The file is gone. An antivirus that quarantines it removes it right after the download.',
        allowIt,
        reinstall,
      ];
    case 'EBUSY':
      return [
        'The file is in use: another autter install or upgrade is replacing it. Wait a few seconds and run the command again.',
      ];
    default:
      return [];
  }
}

function verifyBinaryRuns(bin) {
  try {
    const out = execFileSync(bin, ['--version'], { encoding: 'utf8', timeout: 10_000 });
    return out.trim().split(/\s+/)[0] || null;
  } catch (err) {
    const detail = err.stderr?.toString() || err.stdout?.toString() || err.message || String(err);
    try {
      fs.rmSync(bin, { force: true });
    } catch {
      // best effort
    }
    if (process.platform === 'linux' && detail.includes('GLIBC')) {
      throw new Error(
        `The autter binary could not run on this system (incompatible glibc).\n${detail}\n\n` +
          'autter requires glibc 2.35+ (Ubuntu 22.04+). On Ubuntu 20.04 / older WSL2, use a newer distro or Docker.'
      );
    }
    const help = spawnFailureHelp(err, { bin });
    throw new Error(
      [`The autter binary could not run on this system: ${detail}`, ...help].join('\n')
    );
  }
}

// `autter --version` prints the bare version ("1.6.8", or "1.6.8 (debug)").
function installedVersion(bin) {
  try {
    const out = execFileSync(bin, ['--version'], { encoding: 'utf8', timeout: 10_000 });
    return out.trim().split(/\s+/)[0] || null;
  } catch {
    return null;
  }
}

function reportInstallPing(tag) {
  if (process.env.AUTTER_NO_INSTALL_PING === '1') return Promise.resolve();
  const safeTag = String(tag).replace(/[^A-Za-z0-9._-]/g, '');
  console.log(
    'Counting this install with an anonymous ping (OS, architecture, and version only). ' +
      'Set AUTTER_NO_INSTALL_PING=1 to disable.'
  );
  return fetch(`${INSTALL_PING_HOST}/capture/`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    signal: AbortSignal.timeout(5_000),
    body: JSON.stringify({
      api_key: INSTALL_PING_API_KEY,
      event: 'install_script_run',
      distinct_id: crypto.randomUUID(),
      properties: {
        os: OS_NAMES[process.platform],
        arch: CPU_NAMES[process.arch],
        release_tag: safeTag,
        trigger: 'install',
        source: 'npm',
      },
    }),
  }).catch(() => {});
}

// --- putting the binary in place --------------------------------------------
//
// Windows will not replace or delete an .exe that a running process has open
// (rename-over fails with EPERM), and the autter background service runs
// ~/.autter/bin/autter.exe. It will rename one, though. So on Windows the
// service is stopped first (`autter bg shutdown`, as a user would), and if the
// file is still held the old binary is renamed aside to autter.exe.old and the
// new one put in its place. The aside file is deleted once nothing runs it:
// right away, or by the next install. If the swap fails anyway the old binary
// stays where it was and the install fails, naming the cause and the fix.

const SERVICE_STOP_TIMEOUT_MS = 15_000;
const SERVICE_START_TIMEOUT_MS = 90_000;
const RELEASE_WAIT_MS = 10_000;
const ASIDE_SUFFIX = '.old';

// The binary could not be put in place. The previous one is still installed.
class SwapError extends Error {
  constructor(message, cause) {
    super(message);
    this.name = 'SwapError';
    this.code = 'AUTTER_SWAP_FAILED';
    this.inUse = isInUseError(cause);
    this.cause = cause;
  }
}

// The errors Windows gives for a file another process has open.
function isInUseError(err) {
  return Boolean(err) && ['EPERM', 'EACCES', 'EBUSY'].includes(err.code);
}

function reinstallCommand(packageVersion = pkg.version) {
  return packageVersion === DEV_VERSION
    ? 'npm install -g @autter/cli'
    : `npm install -g @autter/cli@${packageVersion}`;
}

function swapErrorMessage(dest, err) {
  const code = (err && err.code) || 'unknown error';
  if (isInUseError(err)) {
    return [
      `could not replace ${dest}: the file is in use (${code}).`,
      'The autter background service or another autter command still has it open, and Windows does not replace a running program.',
      'The previous autter is still installed and works.',
      `Fix: run \`autter bg shutdown\`, then run the install again: ${reinstallCommand()}`,
    ].join('\n');
  }
  return [
    `could not put the new autter binary at ${dest} (${(err && err.message) || code}).`,
    'The previous autter is still installed and works.',
  ].join('\n');
}

// Leftovers of earlier swaps next to `dest`: autter.exe.old, autter.exe.<pid>.old.
function leftoverBinaries(dest, fsImpl = fs) {
  const dir = path.dirname(dest);
  const base = path.basename(dest);
  let names;
  try {
    names = fsImpl.readdirSync(dir);
  } catch {
    return [];
  }
  return names
    .filter((name) => name === base + ASIDE_SUFFIX || (name.startsWith(`${base}.`) && name.endsWith(ASIDE_SUFFIX)))
    .map((name) => path.join(dir, name));
}

// Delete leftovers no process runs any more; the ones still running stay.
function removeLeftoverBinaries(dest, fsImpl = fs) {
  for (const file of leftoverBinaries(dest, fsImpl)) {
    try {
      fsImpl.rmSync(file, { force: true });
    } catch {
      // still running: removed by a later install
    }
  }
}

function asidePath(dest, pid, fsImpl = fs) {
  const plain = dest + ASIDE_SUFFIX;
  if (!fsImpl.existsSync(plain)) return plain;
  return `${dest}.${pid}${ASIDE_SUFFIX}`;
}

// `autter bg shutdown` on the installed binary. Exits 0 only when a service
// was running and was asked to stop, so the result says whether to start it
// again afterwards.
function stopService(bin, execFile = execFileSync) {
  try {
    execFile(bin, ['bg', 'shutdown'], { stdio: 'ignore', timeout: SERVICE_STOP_TIMEOUT_MS, windowsHide: true });
    return true;
  } catch {
    return false;
  }
}

// `autter bg start` on the new binary. Returns null, or why it failed.
function startService(bin, execFile = execFileSync) {
  try {
    execFile(bin, ['bg', 'start'], { stdio: 'ignore', timeout: SERVICE_START_TIMEOUT_MS, windowsHide: true });
    return null;
  } catch (err) {
    return err.message || String(err);
  }
}

function sleepMs(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

// Whether no process holds `file` open for running (opening it for writing
// then fails with EBUSY on Windows).
function fileReleased(file, fsImpl = fs) {
  try {
    fsImpl.closeSync(fsImpl.openSync(file, 'r+'));
    return true;
  } catch (err) {
    return !isInUseError(err);
  }
}

// Wait for the stopped service to let go of `file`. Not fatal: renaming
// aside works on a file that is still running.
function waitForRelease(file, { fsImpl = fs, timeoutMs = RELEASE_WAIT_MS, sleep = sleepMs, now = Date.now } = {}) {
  const deadline = now() + timeoutMs;
  for (;;) {
    if (fileReleased(file, fsImpl)) return true;
    if (now() >= deadline) return false;
    sleep(200);
  }
}

// Move the downloaded file `tmp` to `dest`. Returns { serviceWasRunning,
// aside }: `aside` is where the previous binary went (Windows, file in use),
// for finishInstall to delete or restore. Throws SwapError with the previous
// binary in place and `tmp` removed.
function replaceBinary(tmp, dest, deps = {}) {
  const {
    fs: fsImpl = fs,
    platform = process.platform,
    stopService: stop = stopService,
    startService: start = startService,
    waitForRelease: wait = (file) => waitForRelease(file, { fsImpl }),
    pid = process.pid,
    log = console.log,
  } = deps;
  const discardTmp = () => {
    try {
      fsImpl.rmSync(tmp, { force: true });
    } catch {
      // best effort
    }
  };

  if (platform !== 'win32' || !fsImpl.existsSync(dest)) {
    try {
      fsImpl.renameSync(tmp, dest);
    } catch (err) {
      discardTmp();
      throw new SwapError(swapErrorMessage(dest, err), err);
    }
    return { serviceWasRunning: false, aside: null };
  }

  removeLeftoverBinaries(dest, fsImpl);
  const serviceWasRunning = stop(dest);
  if (serviceWasRunning) {
    log('autter: stopped the background service to replace the binary');
    wait(dest);
  }
  const restartOld = () => {
    if (serviceWasRunning) start(dest);
  };

  try {
    fsImpl.renameSync(tmp, dest);
    return { serviceWasRunning, aside: null };
  } catch (err) {
    if (!isInUseError(err)) {
      discardTmp();
      restartOld();
      throw new SwapError(swapErrorMessage(dest, err), err);
    }
  }

  // Still held: rename the running file aside, which Windows allows.
  const aside = asidePath(dest, pid, fsImpl);
  try {
    fsImpl.renameSync(dest, aside);
  } catch (err) {
    discardTmp();
    restartOld();
    throw new SwapError(swapErrorMessage(dest, err), err);
  }
  try {
    fsImpl.renameSync(tmp, dest);
  } catch (err) {
    try {
      fsImpl.renameSync(aside, dest);
    } catch (restoreErr) {
      discardTmp();
      throw new SwapError(
        `${swapErrorMessage(dest, err)}\nThe previous binary could not be put back either (${restoreErr.message}): rename ${aside} to ${dest}.`,
        err,
      );
    }
    discardTmp();
    restartOld();
    throw new SwapError(swapErrorMessage(dest, err), err);
  }
  return { serviceWasRunning, aside };
}

// After the new binary is in place: check it runs (putting the previous one
// back if it does not), delete the aside file, and start the background
// service again on the new binary if it was running before.
function finishInstall(dest, swap, deps = {}) {
  const {
    fs: fsImpl = fs,
    verify = verifyBinaryRuns,
    startService: start = startService,
    warn = console.warn,
  } = deps;
  try {
    verify(dest);
  } catch (err) {
    if (swap.aside) {
      try {
        fsImpl.rmSync(dest, { force: true });
        fsImpl.renameSync(swap.aside, dest);
        err.message += '\nThe previous autter is still installed.';
      } catch {
        // the error below still says what failed
      }
    }
    if (swap.serviceWasRunning && fsImpl.existsSync(dest)) start(dest);
    throw err;
  }
  if (swap.aside) {
    try {
      fsImpl.rmSync(swap.aside, { force: true });
    } catch {
      // the old process still runs it: the next install removes it
    }
  }
  if (swap.serviceWasRunning) {
    const failure = start(dest);
    if (failure) {
      warn(`autter: could not start the background service again (${failure}); run 'autter bg restart'`);
    }
  }
}

// Make sure the native binary exists at ~/.autter/bin, downloading it when
// missing or (for a pinned package version) when a different version is
// present. Returns { bin, downloaded }. `deps` replaces the file system and
// process calls in tests.
async function ensureBinary(deps = {}) {
  const asset = assetName(); // throws early on unsupported platforms
  const dest = binaryDest();
  let tag = releaseTag();

  if (fs.existsSync(dest)) {
    if (tag === 'latest') {
      // An existing install is authoritative; the autter daemon keeps it fresh.
      return { bin: dest, downloaded: false };
    }
    const current = installedVersion(dest);
    if (current === tag.replace(/^v/, '')) {
      return { bin: dest, downloaded: false };
    }
    console.log(`autter: replacing version ${current ?? 'unknown'} at ${dest} with ${tag}`);
  }

  if (tag === 'latest') {
    tag = await resolveLatestTag();
  }

  console.log(`autter: downloading ${asset} (release: ${tag})...`);
  const buf = await fetchBuffer(downloadUrl(tag, asset));
  if (buf.length === 0) {
    throw new Error('downloaded file is empty');
  }
  await verifyChecksum(buf, asset, tag);

  fs.mkdirSync(installDir(), { recursive: true });
  const tmp = path.join(installDir(), `autter.tmp.${process.pid}`);
  fs.writeFileSync(tmp, buf, { mode: 0o755 });
  installDownloadedBinary(tmp, dest, deps);

  await reportInstallPing(tag);
  return { bin: dest, downloaded: true };
}

// Put the verified download `tmp` at `dest` and check that it runs.
function installDownloadedBinary(tmp, dest, deps = {}) {
  const swap = replaceBinary(tmp, dest, deps);

  if ((deps.platform || process.platform) === 'darwin') {
    try {
      execFileSync('xattr', ['-d', 'com.apple.quarantine', dest], { stdio: 'ignore' });
    } catch {
      // Attribute not present or xattr unavailable; nothing to clean.
    }
  }

  finishInstall(dest, swap, deps);
  return swap;
}

// postinstall entry point. Ordinary transient download/platform errors defer to
// bin/autter.js, but an integrity failure must fail installation closed.
async function main({ ensure = ensureBinary } = {}) {
  if (process.env.AUTTER_NPM_SKIP_DOWNLOAD === '1') {
    console.log('autter: AUTTER_NPM_SKIP_DOWNLOAD=1, skipping binary download');
    return;
  }

  try {
    checkGit();
    checkLinuxGlibc();
  } catch (err) {
    console.warn(`autter: ${err.message}`);
    return;
  }

  let result;
  try {
    result = await ensure();
  } catch (err) {
    if (/checksums\.txt|checksum mismatch|no checksum entry/.test(err.message)) {
      throw err;
    }
    // The binary was downloaded but could not be put in place (on Windows,
    // usually because the background service still runs it). Deferring to
    // the launcher would leave the old version installed while npm reports
    // success, so the install fails instead; the old binary still works.
    if (err instanceof SwapError) {
      throw err;
    }
    console.warn(`autter: could not install the native binary now (${err.message})`);
    console.warn('autter: it will be downloaded on first run instead');
    return;
  }
  writeInstallMarker();

  if (!result.downloaded) {
    return;
  }

  // Do not mutate git config / IDE hooks / daemon from postinstall — that
  // requires consent via `autter onboard` (or explicit `autter install --system`).
  console.log('');
  console.log(`autter installed at ${result.bin}`);
  console.log("Run 'autter onboard' to finish setup (consent for hooks, git config, and daemon).");
  console.log("Upgrade later with 'npm install -g @autter/cli@latest'; 'npm update' alone does not upgrade the CLI.");
}

module.exports = {
  DEV_VERSION,
  SwapError,
  assetName,
  binaryDest,
  downloadUrl,
  ensureBinary,
  finishInstall,
  installDownloadedBinary,
  installMarkerPath,
  leftoverBinaries,
  launcherNeedsInstall,
  readInstallMarker,
  main,
  releaseTag,
  removeLeftoverBinaries,
  replaceBinary,
  resolveLatestTag,
  spawnFailureHelp,
  startService,
  stopService,
  verifyChecksum,
  writeInstallMarker,
};

if (require.main === module) {
  main().catch((err) => {
    const [first, ...rest] = String(err.message).split('\n');
    console.error(`autter: postinstall failed: ${first}`);
    for (const line of rest) console.error(`autter: ${line}`);
    process.exitCode = 1;
  });
}
