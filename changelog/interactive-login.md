# CLI 1.7.3: Interactive login

Release date: September 12, 2026.

## Changed

- `autter login` signs in through the browser. It opens the device-authorization page, shows a one-time code, and waits until you approve the machine. You do not paste a token.
- `autter login --token <PAT>` remains the non-interactive fallback for CI and for when the browser flow cannot complete.

## Fixed

- Cloud sync could report an expired login after a concurrent token refresh from the git proxy and the background service. Refresh is serialized across processes, and a lost race reloads the session the other process already stored.

## Update

Install this release with the normal CLI installer or npm package:

```bash
npm install -g @autter/cli
```

Then run `autter login`, approve the device in the browser, and confirm with `autter whoami` and `autter doctor`.
