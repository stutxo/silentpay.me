#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${CC_wasm32_unknown_unknown:-}" && -x /opt/homebrew/opt/llvm/bin/clang ]]; then
  export CC_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/clang
fi

wasm-pack build --target web --release --locked
rm -f pkg/.gitignore
node --input-type=module -e 'import fs from "node:fs"; const path = "pkg/package.json"; const pkg = JSON.parse(fs.readFileSync(path, "utf8")); pkg.private = true; fs.writeFileSync(path, `${JSON.stringify(pkg, null, 2)}\n`);'
