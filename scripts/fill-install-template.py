#!/usr/bin/env python3
"""Fill the install.sh release placeholders with concrete values.

The committed install.sh ships with placeholders so the same file works both as
the always-latest template (served at autter.dev/install.sh) and as a
version-pinned copy attached to a GitHub Release. The release workflow runs this
to produce the pinned copy.

Usage:
    fill-install-template.py SRC DEST --repo R --version V --checksums C

  --checksums is the pipe-separated "sha256  filename" string install.sh parses
  (e.g. "abc...  autter-macos-arm64|def...  autter-linux-x64").
"""

import argparse


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("src")
    p.add_argument("dest")
    p.add_argument("--repo", required=True)
    p.add_argument("--version", required=True)
    p.add_argument("--checksums", required=True)
    args = p.parse_args()

    with open(args.src, encoding="utf-8") as f:
        text = f.read()

    replacements = {
        "__REPO_PLACEHOLDER__": args.repo,
        "__VERSION_PLACEHOLDER__": args.version,
        "__CHECKSUMS_PLACEHOLDER__": args.checksums,
    }
    for placeholder, value in replacements.items():
        if placeholder not in text:
            raise SystemExit(f"error: placeholder {placeholder} not found in {args.src}")
        text = text.replace(placeholder, value)

    with open(args.dest, "w", encoding="utf-8") as f:
        f.write(text)


if __name__ == "__main__":
    main()
