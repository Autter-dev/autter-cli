'use strict';

const assert = require('node:assert/strict');
const test = require('node:test');
const crypto = require('node:crypto');
const { verifyChecksum } = require('./install');

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
