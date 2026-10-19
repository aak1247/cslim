#!/usr/bin/env python3
"""Linux 只读引用检查；仅返回命中的 PID/进程名，不输出命令行或环境内容。"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def process_references(paths):
    paths = [str(p).rstrip('/') for p in paths]
    hits = {p: [] for p in paths}
    own = set()
    pid = os.getpid()
    while pid > 1:
        own.add(pid)
        try:
            pid = int(next(line for line in Path(f'/proc/{pid}/status').read_text().splitlines() if line.startswith('PPid:')).split()[1])
        except (OSError, StopIteration):
            break
    for directory in Path('/proc').iterdir():
        if not directory.name.isdigit() or int(directory.name) in own:
            continue
        refs = []
        try:
            comm = (directory/'comm').read_text().strip()
            for name in ('cwd', 'exe', 'root'):
                try: refs.append(os.readlink(directory/name))
                except FileNotFoundError: pass
            for fd in (directory/'fd').iterdir():
                try: refs.append(os.readlink(fd))
                except FileNotFoundError: pass
            for name in ('cmdline', 'environ'):
                refs.extend((directory/name).read_bytes().decode(errors='replace').split('\0'))
            for line in (directory/'maps').read_text().splitlines():
                fields = line.split(None, 5)
                if len(fields) == 6: refs.append(fields[5])
        except FileNotFoundError:
            continue
        except (ProcessLookupError, PermissionError):
            if directory.exists():
                raise RuntimeError(f'无法完整检查进程 {directory.name}，未允许迁移')
            continue
        for path in paths:
            if any(path == ref or ref.startswith(path+'/') or path in ref for ref in refs):
                hits[path].append(f'process {directory.name} {comm}')
    return hits


def references(paths, *, privileged=True):
    paths = [str(p) for p in paths]
    args = [sys.executable, str(Path(__file__).resolve()), '--process-probe']
    if privileged and os.geteuid() != 0:
        args = ['sudo', '-n', '/usr/bin/python3', str(Path(__file__).resolve()), '--process-probe']
    result = subprocess.run(args, input=json.dumps(paths), capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError('进程引用检查不完整: '+result.stderr.strip())
    hits = json.loads(result.stdout)
    inventory = subprocess.check_output(['docker', 'ps', '-aq', '--no-trunc'], text=True).split()
    if inventory:
        result = subprocess.run(['docker', 'inspect', *inventory], capture_output=True, text=True)
        containers = json.loads(result.stdout)
        if result.returncode:
            seen = {c['Id'] for c in containers}
            current = set(subprocess.check_output(['docker', 'ps', '-aq', '--no-trunc'], text=True).split())
            if (set(inventory)-seen) & current:
                raise RuntimeError('Docker 挂载清单不完整')
        for container in containers:
            for mount in container.get('Mounts', []):
                source = mount.get('Source', '').rstrip('/')
                if source:
                    for path in paths:
                        if source == path or source.startswith(path+'/') or path.startswith(source+'/'):
                            hits[path].append('container '+container['Name']+' mount')
    for line in Path('/proc/self/mountinfo').read_text().splitlines():
        mount = re.sub(r'\\([0-7]{3})', lambda m: chr(int(m[1], 8)), line.split()[4])
        for path in paths:
            if mount == path or mount.startswith(path+'/'):
                hits[path].append('mountpoint inside target')
    return hits


if __name__ == '__main__':
    if sys.argv[1:] != ['--process-probe']:
        raise SystemExit('仅供迁移工具调用')
    print(json.dumps(process_references(json.load(sys.stdin))))
