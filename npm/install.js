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
// The package version is stamped to the CLI release tag at publish time, so
// `npm i -g @autter/cli@1.7.0` fetches the v1.7.0 binaries. The in-repo placeholder
// version (0.0.0-dev) resolves to the latest release instead.
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

function releaseTag() {
  const override = process.env.AUTTER_RELEASE_TAG;
  if (override && override !== 'latest') return override;
  if (pkg.version === '0.0.0-dev') return 'latest';
  return `v${pkg.version}`;
}

function downloadUrl(tag, file) {
  return tag === 'latest'
    ? `https://github.com/${REPO}/releases/latest/download/${file}`
    : `https://github.com/${REPO}/releases/download/${tag}/${file}`;
}

async function fetchBuffer(url) {
  const res = await fetch(url, { redirect: 'follow' });
  if (!res.ok) {
    throw new Error(`download failed (HTTP ${res.status}) for ${url}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

// Verify against the release's checksums.txt (`<sha256>  <asset>` lines).
// Releases before v1.6.8 shipped no checksums file; skip with a warning then.
async function verifyChecksum(buf, asset, tag) {
  let checksums;
  try {
    checksums = (await fetchBuffer(downloadUrl(tag, 'checksums.txt'))).toString('utf8');
  } catch {
    console.warn(`autter: no checksums.txt for release ${tag}; skipping checksum verification`);
    return;
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
    throw new Error(`The autter binary could not run on this system: ${detail}`);
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

// Make sure the native binary exists at ~/.autter/bin, downloading it when
// missing or (for a pinned package version) when a different version is
// present. Returns { bin, downloaded }.
async function ensureBinary() {
  const asset = assetName(); // throws early on unsupported platforms
  const dest = binaryDest();
  const tag = releaseTag();

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

  console.log(`autter: downloading ${asset} (release: ${tag})...`);
  const buf = await fetchBuffer(downloadUrl(tag, asset));
  if (buf.length === 0) {
    throw new Error('downloaded file is empty');
  }
  await verifyChecksum(buf, asset, tag);

  fs.mkdirSync(installDir(), { recursive: true });
  const tmp = path.join(installDir(), `autter.tmp.${process.pid}`);
  fs.writeFileSync(tmp, buf, { mode: 0o755 });
  try {
    fs.renameSync(tmp, dest);
  } catch (err) {
    // Windows cannot rename over a running binary; retry after removing it.
    try {
      fs.rmSync(dest, { force: true });
      fs.renameSync(tmp, dest);
    } catch {
      fs.rmSync(tmp, { force: true });
      throw err;
    }
  }

  if (process.platform === 'darwin') {
    try {
      execFileSync('xattr', ['-d', 'com.apple.quarantine', dest], { stdio: 'ignore' });
    } catch {
      // Attribute not present or xattr unavailable; nothing to clean.
    }
  }

  verifyBinaryRuns(dest);

  await reportInstallPing(tag);
  return { bin: dest, downloaded: true };
}

// postinstall entry point. Never fails the surrounding `npm install`: on any
// error it warns and defers to bin/autter.js, which retries on first run.
async function main() {
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
    result = await ensureBinary();
  } catch (err) {
    console.warn(`autter: could not install the native binary now (${err.message})`);
    console.warn('autter: it will be downloaded on first run instead');
    return;
  }

  if (!result.downloaded) {
    return;
  }

  // Mirror install.sh: wire up agent/editor integrations right away. The
  // daemon re-checks hooks daily, so a failure here is non-fatal.
  try {
    execFileSync(result.bin, ['install-hooks'], { stdio: 'inherit', timeout: 120_000 });
  } catch {
    console.warn("autter: could not set up IDE/agent hooks; run 'autter install-hooks' manually");
  }

  console.log('');
  console.log(`autter installed at ${result.bin}`);
  console.log("Run 'autter onboard' to finish setup (local-only mode or connect to autter.dev).");
}

module.exports = { assetName, binaryDest, ensureBinary, releaseTag };

if (require.main === module) {
  main().catch((err) => {
    // Belt and braces: postinstall must never break `npm install`.
    console.warn(`autter: postinstall failed (${err.message})`);
  });
}
