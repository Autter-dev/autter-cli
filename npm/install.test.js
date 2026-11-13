'use strict';

const assert = require('node:assert/strict');
const test = require('node:test');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const pkg = require('./package.json');
const {
  DEV_VERSION,
  SwapError,
  assetName,
  binaryDest,
  ensureBinary,
  finishInstall,
  installMarkerPath,
  launcherNeedsInstall,
  leftoverBinaries,
  main,
  readInstallMarker,
  releaseTag,
  removeLeftoverBinaries,
  replaceBinary,
  resolveLatestTag,
  spawnFailureHelp,
  verifyChecksum,
  writeInstallMarker,
} = require('./install');

const RELEASES = 'https://github.com/autter-dev/autter-cli/releases';

// Set or remove environment variables for one test and put them back after.
function withEnv(t, vars) {
  const saved = {};
  for (const [name, value] of Object.entries(vars)) {
    saved[name] = process.env[name];
    if (value === undefined) delete process.env[name];
    else process.env[name] = value;
  }
  t.after(() => {
    for (const [name, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[name];
      else process.env[name] = value;
    }
  });
}

test('the checked-in version is the dev placeholder, and the release stamps the real one', () => {
  // A release version left here goes stale: every install from a checkout
  // then fetches that old release's binary (3.1.0 was checked in until 3.6.0).
  assert.equal(DEV_VERSION, '0.0.0-dev');
  assert.equal(pkg.version, DEV_VERSION);

  // The placeholder must never reach npm: release.yml sets the version from
  // the tag in npm/ before it publishes.
  const workflow = fs.readFileSync(
    path.join(__dirname, '..', '.github', 'workflows', 'release.yml'),
    'utf8',
  );
  const stamp = workflow.indexOf('npm version "${GITHUB_REF_NAME#v}" --no-git-tag-version');
  const publish = workflow.indexOf('\n          npm publish ');
  assert.ok(stamp > 0, 'release.yml no longer stamps the npm version from the tag');
  assert.ok(publish > stamp, 'release.yml must stamp the version before npm publish');
  assert.match(workflow, /working-directory: npm\n/);
});

test('a checkout asks for the latest release; a published version asks for its own tag', (t) => {
  withEnv(t, { AUTTER_RELEASE_TAG: undefined });
  t.after(() => {
    pkg.version = DEV_VERSION;
  });

  assert.equal(releaseTag(), 'latest');

  // What `npm version 3.6.0` in release.yml leaves in the published package.
  pkg.version = '3.6.0';
  assert.equal(releaseTag(), 'v3.6.0');

  // AUTTER_RELEASE_TAG wins over both, as in install.sh.
  process.env.AUTTER_RELEASE_TAG = 'v3.5.1';
  assert.equal(releaseTag(), 'v3.5.1');
  process.env.AUTTER_RELEASE_TAG = 'latest';
  pkg.version = DEV_VERSION;
  assert.equal(releaseTag(), 'latest');
});

test('"latest" is resolved to the tag GitHub redirects to', async () => {
  const seen = [];
  const redirecting = async (url, init) => {
    seen.push([url, init.method, init.redirect]);
    return new Response(null, { status: 302, headers: { location: `${RELEASES}/tag/v3.6.0` } });
  };
  assert.equal(await resolveLatestTag(redirecting), 'v3.6.0');
  assert.deepEqual(seen, [[`${RELEASES}/latest`, 'HEAD', 'manual']]);

  // No redirect to a tag (GitHub is down, a proxy answers): an error that
  // names the URL, never a guess.
  const failing = async () => new Response(null, { status: 502 });
  await assert.rejects(resolveLatestTag(failing), (err) => {
    assert.match(err.message, /HTTP 502/);
    assert.ok(err.message.includes(`${RELEASES}/latest`), err.message);
    return true;
  });
});

test(
  'an install from a checkout downloads the latest release and verifies its checksum',
  { skip: process.platform === 'win32' ? 'the stand-in binary is a shell script' : false },
  async (t) => {
    // A throwaway home: the binary goes to <home>/.autter/bin.
    const home = fs.mkdtempSync(path.join(os.tmpdir(), 'autter-npm-test-'));
    withEnv(t, { HOME: home, AUTTER_RELEASE_TAG: undefined, AUTTER_NO_INSTALL_PING: '1' });
    const realFetch = globalThis.fetch;
    t.after(() => {
      globalThis.fetch = realFetch;
      fs.rmSync(home, { recursive: true, force: true });
    });
    assert.ok(binaryDest().startsWith(home + path.sep), `refusing to install into ${binaryDest()}`);
    assert.equal(pkg.version, DEV_VERSION);

    const asset = assetName();
    const binary = Buffer.from('#!/bin/sh\necho 9.9.9\n');
    const sha256 = crypto.createHash('sha256').update(binary).digest('hex');
    const requested = [];
    globalThis.fetch = async (url) => {
      requested.push(String(url));
      switch (String(url)) {
        case `${RELEASES}/latest`:
          return new Response(null, { status: 302, headers: { location: `${RELEASES}/tag/v9.9.9` } });
        case `${RELEASES}/download/v9.9.9/${asset}`:
          return new Response(binary);
        case `${RELEASES}/download/v9.9.9/checksums.txt`:
          return new Response(`${sha256}  ${asset}\n`);
        default:
          throw new Error(`unexpected request: ${url}`);
      }
    };

    const result = await ensureBinary();
    assert.deepEqual(result, { bin: path.join(home, '.autter', 'bin', 'autter'), downloaded: true });
    assert.deepEqual(fs.readFileSync(result.bin), binary);
    // The binary and its checksums come from the one release that "latest"
    // named, not from /releases/latest/download/, which has no version to
    // decide the checksum policy with.
    assert.deepEqual(requested, [
      `${RELEASES}/latest`,
      `${RELEASES}/download/v9.9.9/${asset}`,
      `${RELEASES}/download/v9.9.9/checksums.txt`,
    ]);

    // A binary that is already there is left alone: the background service
    // keeps it current.
    requested.length = 0;
    assert.deepEqual(await ensureBinary(), { bin: result.bin, downloaded: false });
    assert.deepEqual(requested, []);
  },
);

test('rejects invalid release tags rather than guessing checksum policy', async () => {
  await assert.rejects(
    verifyChecksum(Buffer.from('binary'), 'autter-linux-x64', 'latest'),
    /invalid release tag/,
  );
});

test('checksum policy parser accepts current semantic release tags', async () => {
  const source = fs.readFileSync(require.resolve('./install'), 'utf8');
  assert.match(source, /checksumsRequired = major > 1/);
  assert.match(source, /patch >= 8/);
});

test('checksum digest format uses SHA-256', () => {
  const digest = crypto.createHash('sha256').update('binary').digest('hex');
  assert.match(digest, /^[a-f0-9]{64}$/);
});

const WIN_BIN = 'C:\\Users\\dev\\.autter\\bin\\autter.exe';

function spawnError(code) {
  const err = new Error(`spawnSync ${WIN_BIN} ${code}`);
  err.code = code;
  return err;
}

test('Windows: "spawnSync autter.exe UNKNOWN" names the causes and the fixes', () => {
  const help = spawnFailureHelp(spawnError('UNKNOWN'), { platform: 'win32', bin: WIN_BIN }).join('\n');
  assert.match(help, /antivirus/);
  assert.match(help, /download is incomplete/);
  assert.match(help, /another processor type/);
  // The exact command that shows Windows' own error, with the real path.
  assert.ok(help.includes(`& '${WIN_BIN}' --version`), help);
  assert.ok(help.includes('exclusion for C:\\Users\\dev\\.autter\\bin'), help);
  assert.match(help, /irm https:\/\/api\.autter\.dev\/install\.ps1 \| iex/);
});

test('Windows: access denied, a vanished file and a busy file each get their own advice', () => {
  const opts = { platform: 'win32', bin: WIN_BIN };
  for (const code of ['EACCES', 'EPERM']) {
    const help = spawnFailureHelp(spawnError(code), opts).join('\n');
    assert.match(help, /denied access/, code);
    assert.match(help, /Administrator window/, code);
  }
  assert.match(spawnFailureHelp(spawnError('ENOENT'), opts).join('\n'), /quarantines/);
  const busy = spawnFailureHelp(spawnError('EBUSY'), opts);
  assert.equal(busy.length, 1);
  assert.match(busy[0], /in use/);
  // An error with no known code adds nothing rather than guessing.
  assert.deepEqual(spawnFailureHelp(spawnError('EMFILE'), opts), []);
  assert.deepEqual(spawnFailureHelp(new Error('boom'), opts), []);
  assert.deepEqual(spawnFailureHelp(undefined, opts), []);
});

test('other platforms get no Windows advice', () => {
  for (const platform of ['darwin', 'linux']) {
    const opts = { platform, bin: '/home/dev/.autter/bin/autter' };
    assert.deepEqual(spawnFailureHelp(spawnError('UNKNOWN'), opts), []);
    assert.deepEqual(spawnFailureHelp(spawnError('ENOENT'), opts), []);
    const denied = spawnFailureHelp(spawnError('EACCES'), opts);
    assert.equal(denied.length, 1);
    assert.match(denied[0], /chmod \+x "\/home\/dev\/\.autter\/bin\/autter"/);
  }
});

test('the npm install marker is written to ~/.autter and read back', (t) => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'autter-npm-marker-'));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  // The path the Rust side reads (NPM_INSTALL_MARKER in src/commands/upgrade.rs).
  assert.equal(installMarkerPath(home), path.join(home, '.autter', 'npm-install.json'));
  assert.equal(readInstallMarker(home), null);

  assert.ok(writeInstallMarker({ home, packageVersion: '3.8.0' }));
  assert.deepEqual(readInstallMarker(home), {
    installer: 'npm',
    package: '@autter/cli',
    package_version: '3.8.0',
  });

  // A marker that is not JSON reads as absent rather than crashing the launcher.
  fs.writeFileSync(installMarkerPath(home), 'not json');
  assert.equal(readInstallMarker(home), null);
});

test('the launcher installs when npm skipped the postinstall of a new version', () => {
  // No binary yet: always install.
  assert.equal(launcherNeedsInstall({ binaryExists: false, marker: null, packageVersion: '3.8.0' }), true);
  assert.equal(launcherNeedsInstall({ binaryExists: false, marker: null, packageVersion: DEV_VERSION }), true);
  // `npm install -g @autter/cli@3.8.0` with scripts off: the binary is still
  // the one from before, and no marker names 3.8.0.
  assert.equal(launcherNeedsInstall({ binaryExists: true, marker: null, packageVersion: '3.8.0' }), true);
  assert.equal(
    launcherNeedsInstall({ binaryExists: true, marker: { package_version: '3.7.0' }, packageVersion: '3.8.0' }),
    true,
  );
  // This version already installed its binary: the fast path.
  assert.equal(
    launcherNeedsInstall({ binaryExists: true, marker: { package_version: '3.8.0' }, packageVersion: '3.8.0' }),
    false,
  );
  // A checkout (the dev placeholder) never re-downloads an existing binary.
  assert.equal(launcherNeedsInstall({ binaryExists: true, marker: null, packageVersion: DEV_VERSION }), false);
});

test(
  'the launcher runs the installed binary with its arguments and exit code',
  { skip: process.platform === 'win32' ? 'the stand-in binary is a shell script' : false },
  (t) => {
    const home = fs.mkdtempSync(path.join(os.tmpdir(), 'autter-npm-launcher-'));
    t.after(() => fs.rmSync(home, { recursive: true, force: true }));
    const bin = path.join(home, '.autter', 'bin', 'autter');
    fs.mkdirSync(path.dirname(bin), { recursive: true });
    fs.writeFileSync(bin, '#!/bin/sh\necho "args: $*"\nexit 3\n', { mode: 0o755 });
    const { spawnSync } = require('node:child_process');
    const run = spawnSync(process.execPath, [path.join(__dirname, 'bin', 'autter.js'), 'doctor', '--json'], {
      env: { ...process.env, HOME: home, USERPROFILE: home, AUTTER_NO_INSTALL_PING: '1' },
      encoding: 'utf8',
    });
    assert.equal(run.status, 3, run.stderr);
    assert.equal(run.stdout.trim(), 'args: doctor --json');
    // A checkout's launcher with a binary already there downloads nothing
    // and leaves no npm marker behind.
    assert.equal(readInstallMarker(home), null);
  },
);

// --- replacing a binary that may be running ---------------------------------
//
// A stand-in for the Windows file system: a file in `running` is the image of
// a live process, so it can be renamed but not replaced or deleted (EPERM); a
// file in `locked` is open without delete sharing (an antivirus scan, another
// installer), so it cannot even be renamed (EBUSY).
function windowsFs(files, { running = [], locked = [], failRename = () => null } = {}) {
  const live = new Set(running);
  const held = new Set(locked);
  const calls = [];
  const fail = (code, op, p) => Object.assign(new Error(`${code}: operation not permitted, ${op} '${p}'`), { code });
  return {
    files,
    calls,
    live,
    existsSync: (p) => files.has(p),
    readdirSync: (dir) => [...files.keys()].filter((p) => path.dirname(p) === dir).map((p) => path.basename(p)),
    renameSync(from, to) {
      calls.push(`rename ${path.basename(from)} -> ${path.basename(to)}`);
      const injected = failRename(from, to);
      if (injected) throw injected;
      if (!files.has(from)) throw fail('ENOENT', 'rename', from);
      if (held.has(from)) throw fail('EBUSY', 'rename', from);
      if (files.has(to) && (live.has(to) || held.has(to))) throw fail('EPERM', 'rename', to);
      files.set(to, files.get(from));
      files.delete(from);
      if (live.delete(from)) live.add(to);
    },
    rmSync(p) {
      calls.push(`rm ${path.basename(p)}`);
      if (live.has(p) || held.has(p)) throw fail('EPERM', 'unlink', p);
      files.delete(p);
    },
  };
}

const DIR = path.join(path.sep, 'home', 'dev', '.autter', 'bin');
const DEST = path.join(DIR, 'autter.exe');
const TMP = path.join(DIR, 'autter.tmp.42');

// The process calls replaceBinary makes, recorded in order.
function serviceDeps(fsImpl, { running }) {
  const events = [];
  return {
    events,
    deps: {
      fs: fsImpl,
      platform: 'win32',
      pid: 42,
      log: () => {},
      warn: (msg) => events.push(`warn ${msg}`),
      stopService: (bin) => {
        events.push(`stop ${path.basename(bin)}`);
        return running;
      },
      startService: (bin) => {
        events.push(`start ${path.basename(bin)} (${fsImpl.files.get(bin)})`);
        return null;
      },
      waitForRelease: () => {
        events.push('wait');
        return false;
      },
      verify: () => {},
    },
  };
}

test('Windows: a binary the background service still runs is renamed aside, and the service restarts on the new one', () => {
  const fsImpl = windowsFs(new Map([[DEST, 'old'], [TMP, 'new']]), { running: [DEST] });
  const { events, deps } = serviceDeps(fsImpl, { running: true });

  const swap = replaceBinary(TMP, DEST, deps);
  assert.deepEqual(swap, { serviceWasRunning: true, aside: `${DEST}.old` });
  assert.equal(fsImpl.files.get(DEST), 'new');
  assert.equal(fsImpl.files.get(`${DEST}.old`), 'old');
  assert.deepEqual(fsImpl.calls, [
    'rename autter.tmp.42 -> autter.exe', // EPERM: still running
    'rename autter.exe -> autter.exe.old',
    'rename autter.tmp.42 -> autter.exe',
  ]);

  finishInstall(DEST, swap, deps);
  // Stopped before the swap, started after it, on the new binary.
  assert.deepEqual(events, ['stop autter.exe', 'wait', 'start autter.exe (new)']);
  // The old process still runs the aside file, so it stays for now ...
  assert.ok(fsImpl.files.has(`${DEST}.old`));

  // ... and the next install deletes it once that process is gone.
  fsImpl.live.clear();
  removeLeftoverBinaries(DEST, fsImpl);
  assert.equal(fsImpl.files.has(`${DEST}.old`), false);
});

test('Windows: a stopped service lets go of the file, and nothing is left behind', () => {
  const fsImpl = windowsFs(new Map([[DEST, 'old'], [TMP, 'new']]));
  const { events, deps } = serviceDeps(fsImpl, { running: true });
  const swap = replaceBinary(TMP, DEST, deps);
  assert.deepEqual(swap, { serviceWasRunning: true, aside: null });
  finishInstall(DEST, swap, deps);
  assert.deepEqual([...fsImpl.files.keys()], [DEST]);
  assert.equal(fsImpl.files.get(DEST), 'new');
  assert.deepEqual(events, ['stop autter.exe', 'wait', 'start autter.exe (new)']);
});

test('Windows: a service that was not running is not started', () => {
  const fsImpl = windowsFs(new Map([[DEST, 'old'], [TMP, 'new']]));
  const { events, deps } = serviceDeps(fsImpl, { running: false });
  finishInstall(DEST, replaceBinary(TMP, DEST, deps), deps);
  assert.equal(fsImpl.files.get(DEST), 'new');
  assert.deepEqual(events, ['stop autter.exe']);
});

test('Windows: a file in use that cannot be moved fails loudly and leaves the old binary working', () => {
  const fsImpl = windowsFs(new Map([[DEST, 'old'], [TMP, 'new']]), { locked: [DEST] });
  const { events, deps } = serviceDeps(fsImpl, { running: true });

  assert.throws(
    () => replaceBinary(TMP, DEST, deps),
    (err) => {
      assert.ok(err instanceof SwapError);
      assert.equal(err.code, 'AUTTER_SWAP_FAILED');
      assert.equal(err.inUse, true);
      assert.match(err.message, /could not replace .*autter\.exe: the file is in use \(EBUSY\)/);
      assert.match(err.message, /previous autter is still installed and works/);
      assert.match(err.message, /Fix: run `autter bg shutdown`, then run the install again: npm install -g @autter\/cli/);
      return true;
    },
  );
  // The old binary is where it was, the download is gone, and the service
  // that was stopped runs again on the old binary.
  assert.deepEqual([...fsImpl.files.entries()], [[DEST, 'old']]);
  assert.deepEqual(events, ['stop autter.exe', 'wait', 'start autter.exe (old)']);
});

test('Windows: when the new file cannot be moved in after the old one went aside, the old one is put back', () => {
  let attempts = 0;
  const failRename = (from, to) => {
    if (from === TMP && to === DEST && ++attempts === 2) {
      return Object.assign(new Error('EIO: i/o error'), { code: 'EIO' });
    }
    return null;
  };
  const fsImpl = windowsFs(new Map([[DEST, 'old'], [TMP, 'new']]), { running: [DEST], failRename });
  const { events, deps } = serviceDeps(fsImpl, { running: true });
  assert.throws(() => replaceBinary(TMP, DEST, deps), SwapError);
  assert.deepEqual([...fsImpl.files.entries()], [[DEST, 'old']]);
  assert.deepEqual(events, ['stop autter.exe', 'wait', 'start autter.exe (old)']);
});

test('Windows: a new binary that does not run is replaced by the previous one', () => {
  const fsImpl = windowsFs(new Map([[DEST, 'old'], [TMP, 'new']]), { running: [DEST] });
  const { events, deps } = serviceDeps(fsImpl, { running: true });
  const swap = replaceBinary(TMP, DEST, deps);
  fsImpl.live.clear(); // the old service process has exited
  deps.verify = () => {
    throw new Error('The autter binary could not run on this system');
  };
  assert.throws(() => finishInstall(DEST, swap, deps), /previous autter is still installed/);
  assert.deepEqual([...fsImpl.files.entries()], [[DEST, 'old']]);
  assert.deepEqual(events, ['stop autter.exe', 'wait', 'start autter.exe (old)']);
});

test('leftovers of earlier swaps are found and removed; other files stay', () => {
  const files = new Map([
    [DEST, 'current'],
    [`${DEST}.old`, 'x'],
    [`${DEST}.1234.old`, 'x'],
    [path.join(DIR, 'git.exe'), 'shim'],
    [path.join(DIR, 'autter.exe.bak'), 'kept'],
  ]);
  const fsImpl = windowsFs(files, { running: [`${DEST}.1234.old`] });
  assert.deepEqual(leftoverBinaries(DEST, fsImpl), [`${DEST}.old`, `${DEST}.1234.old`]);
  removeLeftoverBinaries(DEST, fsImpl);
  // The one a process still runs stays until the next install.
  assert.deepEqual([...fsImpl.files.keys()], [DEST, `${DEST}.1234.old`, path.join(DIR, 'git.exe'), path.join(DIR, 'autter.exe.bak')]);
});

test('other platforms: a failed rename is reported, and the background service is not touched', () => {
  const failRename = () => Object.assign(new Error('EACCES: permission denied'), { code: 'EACCES' });
  const fsImpl = windowsFs(new Map([[DEST, 'old'], [TMP, 'new']]), { failRename });
  const { events, deps } = serviceDeps(fsImpl, { running: true });
  deps.platform = 'linux';
  assert.throws(() => replaceBinary(TMP, DEST, deps), SwapError);
  assert.deepEqual([...fsImpl.files.entries()], [[DEST, 'old']]);
  assert.deepEqual(events, []);

  // A rename that works is one rename, as before.
  const ok = windowsFs(new Map([[DEST, 'old'], [TMP, 'new']]), { running: [DEST] });
  const okDeps = serviceDeps(ok, { running: true });
  okDeps.deps.platform = 'darwin';
  ok.live.clear(); // a running binary is replaced by rename on unix
  assert.deepEqual(replaceBinary(TMP, DEST, okDeps.deps), { serviceWasRunning: false, aside: null });
  assert.deepEqual(ok.calls, ['rename autter.tmp.42 -> autter.exe']);
  assert.deepEqual(okDeps.events, []);
});

test('the postinstall fails when the binary cannot be replaced, and still defers other errors to the launcher', async (t) => {
  withEnv(t, { AUTTER_NPM_SKIP_DOWNLOAD: undefined });
  const quiet = (name) => {
    const original = console[name];
    console[name] = () => {};
    t.after(() => {
      console[name] = original;
    });
  };
  quiet('log');
  quiet('warn');
  const inUse = new SwapError('could not replace autter.exe: the file is in use (EPERM).', { code: 'EPERM' });
  await assert.rejects(main({ ensure: async () => { throw inUse; } }), (err) => err === inUse);
  // A network failure still lets npm finish; the launcher downloads later.
  await main({ ensure: async () => { throw new Error('download failed (HTTP 502)'); } });
});

test(
  'an npm upgrade over a running binary swaps it on the real file system and restarts the service',
  { skip: process.platform === 'win32' ? 'the stand-in binary is a shell script' : false },
  async (t) => {
    const home = fs.mkdtempSync(path.join(os.tmpdir(), 'autter-npm-swap-'));
    withEnv(t, { HOME: home, AUTTER_RELEASE_TAG: 'v9.9.9', AUTTER_NO_INSTALL_PING: '1' });
    const realFetch = globalThis.fetch;
    t.after(() => {
      globalThis.fetch = realFetch;
      fs.rmSync(home, { recursive: true, force: true });
    });
    const dest = binaryDest();
    assert.ok(dest.startsWith(home + path.sep), `refusing to install into ${dest}`);
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    fs.writeFileSync(dest, '#!/bin/sh\necho 1.0.0\n', { mode: 0o755 });

    const asset = assetName();
    const binary = Buffer.from('#!/bin/sh\necho 9.9.9\n');
    const sha256 = crypto.createHash('sha256').update(binary).digest('hex');
    globalThis.fetch = async (url) => {
      switch (String(url)) {
        case `${RELEASES}/download/v9.9.9/${asset}`:
          return new Response(binary);
        case `${RELEASES}/download/v9.9.9/checksums.txt`:
          return new Response(`${sha256}  ${asset}\n`);
        default:
          throw new Error(`unexpected request: ${url}`);
      }
    };

    // Windows' answer to renaming over a running binary, once.
    let refused = false;
    const fsImpl = {
      ...fs,
      renameSync(from, to) {
        if (to === dest && !refused) {
          refused = true;
          throw Object.assign(new Error(`EPERM: operation not permitted, rename '${from}' -> '${to}'`), { code: 'EPERM' });
        }
        return fs.renameSync(from, to);
      },
    };
    const started = [];
    const result = await ensureBinary({
      fs: fsImpl,
      platform: 'win32',
      log: () => {},
      stopService: () => true,
      waitForRelease: () => false,
      startService: (bin) => {
        started.push([bin, fs.readFileSync(bin, 'utf8')]);
        return null;
      },
    });
    assert.deepEqual(result, { bin: dest, downloaded: true });
    assert.deepEqual(fs.readFileSync(dest), binary);
    assert.deepEqual(started, [[dest, binary.toString()]]);
    // Nothing runs the aside file here, so it is gone already.
    assert.deepEqual(fs.readdirSync(path.dirname(dest)), [path.basename(dest)]);
  },
);
