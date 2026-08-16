#!/usr/bin/env node
'use strict';

// Launcher for the native autter binary at ~/.autter/bin. The fast path is a
// single existence check; the download in install.js only runs when the
// postinstall was skipped (--ignore-scripts, offline CI) or the binary was
// removed.

const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
const { binaryDest, ensureBinary } = require('../install.js');

async function main() {
  let bin = binaryDest();
  if (!fs.existsSync(bin)) {
    try {
      bin = (await ensureBinary()).bin;
    } catch (err) {
      console.error(`autter: could not install the native binary: ${err.message}`);
      console.error('autter: alternative installer: curl -fsSL https://api.autter.dev/install.sh | bash');
      process.exit(1);
    }
  }

  // Let the child own Ctrl-C: without this the node wrapper dies on SIGINT
  // while autter (e.g. `autter onboard`) is still restoring its terminal.
  const noop = () => {};
  process.on('SIGINT', noop);
  process.on('SIGTERM', noop);

  const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
  if (result.error) {
    console.error(`autter: failed to run ${bin}: ${result.error.message}`);
    process.exit(1);
  }
  if (result.signal) {
    process.removeListener('SIGINT', noop);
    process.removeListener('SIGTERM', noop);
    process.kill(process.pid, result.signal);
    return;
  }
  process.exit(result.status === null ? 1 : result.status);
}

main();
