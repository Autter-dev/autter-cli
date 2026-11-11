'use strict';

const assert = require('node:assert/strict');
const test = require('node:test');
const crypto = require('node:crypto');
const { spawnFailureHelp, verifyChecksum } = require('./install');

test('rejects invalid release tags rather than guessing checksum policy', async () => {
  await assert.rejects(
    verifyChecksum(Buffer.from('binary'), 'autter-linux-x64', 'latest'),
    /invalid release tag/,
  );
});

test('checksum policy parser accepts current semantic release tags', async () => {
  const source = require('node:fs').readFileSync(require.resolve('./install'), 'utf8');
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
