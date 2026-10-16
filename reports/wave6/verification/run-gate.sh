#!/bin/zsh
set -u
export PATH="/Users/revenge/code/ps-pikkr/.tooling/bin:/Users/revenge/.cargo/bin:$PATH"
export RUSTC=/Users/revenge/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rustc
export RUSTDOC=/Users/revenge/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rustdoc
name=$1
work=$2
shift 2
log=/Users/revenge/code/jsonmax-rs/reports/wave6/verification/$name.log
print -r -- "cwd=$work" > "$log"
print -r -- "command=$*" >> "$log"
cd "$work" || exit 1
"$@" >> "$log" 2>&1
result=$?
print -r -- "exit=$result" >> "$log"
print -r -- "$name exit=$result log=$log"
tail -n 55 "$log"
exit $result
