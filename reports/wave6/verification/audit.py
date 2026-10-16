from pathlib import Path
import hashlib
import json
import os
import re
import subprocess

root = Path('/Users/revenge/code/jsonmax-rs')
out = root / 'reports/wave6/verification'

def inventory(directory):
    files = {}
    for parent, dirs, names in os.walk(directory):
        dirs[:] = sorted(d for d in dirs if d not in ('.git', 'target'))
        for name in sorted(names):
            path = Path(parent) / name
            if path.is_file():
                files[str(path.relative_to(directory))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return files

preserved = json.loads((out / 'preserved-source-start.json').read_text())
for directory, expected in preserved.items():
    actual = inventory(directory)
    assert actual == expected, f'protected source changed: {directory}'
    print(f'UNCHANGED {directory}: {len(actual)} files')
assert (root / 'target').is_dir()
print('PRESERVED target/: build artifacts added; no clean/removal command used')

for directory in [root / 'crates', Path('/tmp/jmax-vs-parsers/src'), Path('/Users/revenge/code/ps-pikkr/src')]:
    hits = []
    for path in sorted(directory.rglob('*.rs')):
        if re.search(r'\.\s*expect\s*\(|\b(?:todo|unimplemented)\s*!\s*[({\[]', path.read_text()):
            hits.append(str(path))
    assert not hits, hits
    print(f'ZERO expect/todo/unimplemented: {directory}')

for source in ['jmax-bench/src/bin/jmax_bench.rs', 'jmax-validator/src/main.rs', 'jmax-runner/src/main.rs', 'jmax-gen-cli/src/main.rs']:
    text = (root / 'crates' / source).read_text()
    assert text.count('#[global_allocator]') == 1, source
    assert 'rusty_alloc::RustyAlloc' in text, source
    print(f'ONE global allocator with RustyAlloc default: {source}')
assert all(not re.search(r'(?m)^\s*#\[global_allocator\]', p.read_text()) for p in (root / 'crates/jsonmax/src').rglob('*.rs'))
print('NO library-installed global allocator: jsonmax')

snapshot = root / 'reports/wave6/benchmark/harness-snapshot'
for relative in ['Cargo.toml', 'Cargo.lock', 'src/main.rs']:
    assert (snapshot / relative).read_bytes() == (Path('/tmp/jmax-vs-parsers') / relative).read_bytes()
print('MATCH harness snapshot: Cargo.toml, Cargo.lock, src/main.rs')

source_files = [root / 'Cargo.toml', root / 'Cargo.lock']
source_files += sorted((root / 'crates').rglob('*.rs'))
source_files += sorted((root / 'crates').rglob('Cargo.toml'))
source_files += sorted(Path('/Users/revenge/code/ps-pikkr/src').glob('*.rs'))
source_files += sorted(Path('/tmp/jmax-vs-parsers/src').glob('*.rs'))
(out / 'source-final.sha256').write_text(''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p}\n' for p in source_files))
corpora = sorted(Path('/tmp/jmax-bench-data').glob('*.json'))
(out / 'corpus.sha256').write_text(''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p}\n' for p in corpora))
print(f'HASHED {len(source_files)} source/manifest files and {len(corpora)} corpus/provenance files')
subprocess.run(['git', 'diff', '--check'], cwd=root, check=True)
print('PASS git diff --check')
