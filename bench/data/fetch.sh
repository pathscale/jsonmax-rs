#!/bin/sh
# Fetch the standard corpora (not vendored) from simdjson-data at a pinned commit and verify sha256.
# Usage: bench/data/fetch.sh   (writes next to this script; the files are git-ignored)
set -eu
cd "$(dirname "$0")"
base=https://raw.githubusercontent.com/simdjson/simdjson-data/4197c425e857f0ec38e89822fdd0bd9ea21f4daf/jsonexamples
while read -r name sum; do
  [ -f "$name" ] || curl -fsSL -o "$name" "$base/$name"
  echo "$sum  $name" | sha256sum -c - 2>/dev/null || echo "$sum  $name" | shasum -a 256 -c -
done <<LIST
twitter.json 30721e496a8d73cfc50658923c34eb2c0fbe15ee6835005e43ee624d8dedf200
citm_catalog.json a73e7a883f6ea8de113dff59702975e60119b4b58d451d518a929f31c92e2059
canada.json f83b3b354030d5dd58740c68ac4fecef64cb730a0d12a90362a7f23077f50d78
github_events.json c9eebb2cf2d46649059e9d48700919bacb3e8e0fb58452065a1a9de7778fd22e
apache_builds.json f8e3422ac7d3c3550674afcb37e979e4e9bbeccffdb66933423495d55b6f5c74
LIST
echo "endpoint_batch.json, rpc_small.json, binance_small.json are vendored here."
