#!/usr/bin/env node
'use strict';

// Launcher for the native autter binary at ~/.autter/bin. The fast path is a
// single existence check; the download in install.js only runs when the
// postinstall was skipped (--ignore-scripts, offline CI) or the binary was
// removed.

const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
const {
  binaryDest,
  ensureBinary,
  launcherNeedsInstall,
  readInstallMarker,
  spawnFailureHelp,
  writeInstallMarker,
} = require('../install.js');

async function main() {
  let bin = binaryDest();
  const binaryExists = fs.existsSync(bin);
  if (launcherNeedsInstall({ binaryExists, marker: readInstallMarker() })) {
    try {
      bin = (await ensureBinary()).bin;
      writeInstallMarker();
    } catch (err) {
      if (binaryExists) {
        // The binary from before still runs; install this version next time.
        const [first, ...rest] = String(err.message).split('\n');
        console.error(`autter: could not install this package's version of the binary: ${first}`);
        for (const line of rest) console.error(`autter: ${line}`);
        console.error('autter: running the one already installed');
      } else {
        console.error(`autter: could not install the native binary: ${err.message}`);
        console.error('autter: alternative installer: curl -fsSL https://api.autter.dev/install.sh | bash');
        process.exit(1);
      }
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
    for (const line of spawnFailureHelp(result.error, { bin })) {
      console.error(`autter: ${line}`);
    }
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
