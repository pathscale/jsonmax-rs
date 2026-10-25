#!/bin/sh
# Full metrics emitted by target/release/time; all columns are mandatory.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bun "$here/../tables.ts" "$1"
