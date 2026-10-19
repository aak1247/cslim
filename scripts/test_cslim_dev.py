#!/usr/bin/env python3
"""Exercise the opt-in PATH shim without touching a real Cargo target."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

repo = Path(__file__).resolve().parent.parent
with tempfile.TemporaryDirectory(prefix='cslim-dev-') as directory:
    root = Path(directory)
    home = root / 'home'
    shim_dir = home / '.local/libexec/cslim'
    shim_dir.mkdir(parents=True)
    shutil.copy2(repo / 'adapters/cargo-shim', shim_dir / 'cargo')
    (shim_dir / 'cargo').chmod(0o755)
    fake_bin = root / 'bin'
    fake_bin.mkdir()
    log = root / 'calls.jsonl'
    script = '''#!/usr/bin/env python3
import json, os, sys
with open(os.environ['CSLIM_DEV_TEST_LOG'], 'a') as f:
    f.write(json.dumps([os.path.basename(sys.argv[0]), sys.argv[1:], os.environ['PATH']]) + '\\n')
'''
    for name in ('cargo', 'cargo-cslim'):
        executable = fake_bin / name
        executable.write_text(script)
        executable.chmod(0o755)
    env = dict(os.environ, HOME=str(home),
               PATH=str(fake_bin) + os.pathsep + os.environ['PATH'],
               CSLIM_BIN=str(fake_bin / 'cargo-cslim'),
               CSLIM_DEV_TEST_LOG=str(log))
    for key in ('CSLIM_DEV_ACTIVE', 'CSLIM_DEV_ORIGINAL_PATH',
                'CSLIM_DEV_REAL_CARGO', 'CSLIM_DEV_BIN'):
        env.pop(key, None)
    command = [str(repo / 'adapters/cslim-dev'), 'sh', '-c',
               'cargo +stable check --locked; cargo fmt --all --check; cargo test -- --nocapture']
    subprocess.run(command, env=env, check=True)
    calls = [json.loads(line) for line in log.read_text().splitlines()]
    assert [(name, args) for name, args, _ in calls] == [
        ('cargo-cslim', ['+stable', 'check', '--locked']),
        ('cargo', ['fmt', '--all', '--check']),
        ('cargo-cslim', ['test', '--', '--nocapture']),
    ], calls
    assert all(path == env['PATH'] for _, _, path in calls), calls
    rejected = subprocess.run([str(repo / 'adapters/cslim-dev'), 'cargo', '--color', 'always', 'check'],
                              env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    assert rejected.returncode == 2 and '全局选项' in rejected.stderr, rejected
    assert len(log.read_text().splitlines()) == 3
    assert subprocess.run([str(repo / 'adapters/cslim-dev')], env=env,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 2
print('PASS: 开发命令的构建调用走 cslim，其他 Cargo 命令直通，子进程恢复原 PATH')
