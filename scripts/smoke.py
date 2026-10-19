#!/usr/bin/env python3
"""真实 Cargo/sccache 集成验证，无 registry 下载；退出时停止测试缓存服务。"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import shlex

binary = Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/cargo-cslim').resolve()
sccache = shutil.which('sccache')
assert sccache, 'install sccache first'

def run(args, cwd, env, expected=0):
    result = subprocess.run([str(binary), *args], cwd=cwd, env=env, capture_output=True, text=True)
    if result.returncode != expected:
        raise AssertionError(f'{args}: code={result.returncode}\n{result.stdout}\n{result.stderr}')
    return result

with tempfile.TemporaryDirectory(prefix='cslim-') as directory:
    base = Path(directory)
    root = base / 'cache'
    config = base / 'config.toml'
    config.write_text(f'root = {json.dumps(str(root))}\nsccache = {json.dumps(sccache)}\nauto_gc = false\n')
    env = dict(os.environ, CSLIM_CONFIG=str(config))
    for key in ('CSLIM_HOME', 'RUSTC_WRAPPER', 'CARGO_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR'):
        env.pop(key, None)
    dep = base / 'shared'
    (dep / 'src').mkdir(parents=True)
    (dep / 'Cargo.toml').write_text('[package]\nname="cslim-smoke-dependency"\nversion="1.0.0"\nedition="2024"\n')
    (dep / 'src/lib.rs').write_text('pub fn answer() -> u32 { 42 }\n')
    projects = []
    for name in ('a', 'b', 'c'):
        project = base / name
        (project / 'src').mkdir(parents=True)
        (project / 'Cargo.toml').write_text(f'[package]\nname="smoke-{name}"\nversion="0.1.0"\nedition="2024"\n[dependencies]\ncslim-smoke-dependency={{path="../shared"}}\nitoa="=1.0.18"\n')
        (project / 'src/main.rs').write_text('fn main() { println!("{}", itoa::Buffer::new().format(cslim_smoke_dependency::answer())); }\n')
        projects.append(project)
    try:
        a, b, c = projects
        run(['build', '--offline', '--message-format=json'], a, env)
        # Named entry points work from outside the repo and select that project's
        # .cargo configuration instead of the caller's current directory.
        config.write_text(config.read_text()+'[projects]\nsmoke-a='+json.dumps(str(a/'Cargo.toml'))+'\nsmoke-b='+json.dumps(str(b/'Cargo.toml'))+'\n')
        named_dir = run(['project', 'smoke-a', 'target-dir'], base, env).stdout.strip()
        direct_dir = run(['target-dir', '--manifest-path', str(a/'Cargo.toml')], base, env).stdout.strip()
        assert named_dir == direct_dir
        assert str(root/'v1/targets') in named_dir
        (a/'.cargo').mkdir()
        (a/'.cargo/config.toml').write_text('[env]\nCSLIM_NAMED_PROJECT_PROBE="loaded"\n')
        (a/'src/main.rs').write_text('const _: &str = env!("CSLIM_NAMED_PROJECT_PROBE"); fn main() { println!("{}", cslim_smoke_dependency::answer()); }\n')
        run(['project', 'smoke-a', 'check', '--offline'], base, env)
        (a/'.cargo/config.toml').unlink()
        (a/'src/main.rs').write_text('fn main() { println!("{}", itoa::Buffer::new().format(cslim_smoke_dependency::answer())); }\n')
        run(['project', 'smoke-a', 'build', '--manifest-path', str(b/'Cargo.toml')], base, env, expected=1)
        run(['project', 'unknown', 'build'], base, env, expected=1)
        print('PASS: 命名项目入口在外部目录可用，读取项目 Cargo 配置，目录查询一致')
        # Different target directories must still reuse the common compiled dependency.
        result = run(['build', '--offline', '--message-format=json'], b, env)
        for line in result.stdout.splitlines():
            json.loads(line)  # supervisor logging must not corrupt Cargo JSON output
        entries = json.loads(run(['status', '--json'], a, env).stdout)
        targets = [e for e in entries if e['kind'] == 'targets']
        caches = [e for e in entries if e['kind'] == 'caches']
        assert len(targets) == 2 and len(caches) == 1, entries
        cache = caches[0]
        cache_env = {k: v for k, v in env.items() if not k.startswith('SCCACHE_')}
        cache_env.update(SCCACHE_SERVER_UDS=str(root/'v1/sockets'/f"{cache['id']}.sock"),
                         SCCACHE_DIR=str(root/'v1/caches'/cache['id']/'artifacts'),
                         SCCACHE_CONF=str(root/'v1/sccache.toml'))
        stats = subprocess.check_output([sccache, '--show-stats', '--stats-format=json'], env=cache_env, text=True)
        data = json.loads(stats)
        hits = sum(data['stats']['cache_hits']['counts'].values())
        assert hits >= 1, data
        print(f'PASS: 两个独立 workspace 使用各自 target，共享缓存命中 {hits} 次')
        # Changing a dependency version must not reuse the incompatible output.
        (dep/'Cargo.toml').write_text((dep/'Cargo.toml').read_text().replace('1.0.0', '2.0.0'))
        (dep/'src/lib.rs').write_text('pub fn answer() -> u32 { 84 }\n')
        result = run(['run', '--offline'], c, env)
        assert result.stdout.strip() == '84', result.stdout
        result = run(['run', '--offline', '--', '--target-dir', 'application-argument'], a, env)
        assert result.stdout.strip() == '84'
        print('PASS: 依赖升级输出正确，应用参数原样转发')
        (b/'src/main.rs').write_text('invalid rust syntax !\n')
        run(['build', '--offline'], b, env, expected=101)
        run(['build', '--target-dir', '/tmp/not-managed'], a, env, expected=1)
        run(['build', '--config', 'build.target-dir="/tmp/not-managed"'], a, env, expected=1)
        print('PASS: Cargo 失败码保留，拒绝绕过托管路径')
        # A backend version change creates a distinct namespace, while preserving targets.
        backend = base/'sccache-version-probe'
        backend.write_text('#!/bin/sh\nif [ "$1" = --version ]; then echo "sccache smoke-version"; else exec '+shlex.quote(sccache)+' "$@"; fi\n')
        backend.chmod(0o755)
        original_config = config.read_text()
        config.write_text(original_config.replace(json.dumps(sccache), json.dumps(str(backend))))
        run(['build', '--offline'], a, env)
        version_entries = json.loads(run(['status', '--json'], a, env).stdout)
        assert len([e for e in version_entries if e['kind'] == 'caches']) == 2
        assert len([e for e in version_entries if e['kind'] == 'targets']) == 3
        config.write_text(original_config)
        print('PASS: 缓存后端版本变化创建独立命名空间')
        # Force expiry, check preview then apply. External legacy targets are untouched.
        legacy = a/'target'
        legacy.mkdir()
        (legacy/'keep').write_text('legacy')
        for entry in (root/'v1/targets').glob('*/entry.json'):
            data = json.loads(entry.read_text()); data['last_used'] = 0
            entry.write_text(json.dumps(data))
        run(['gc', '--dry-run'], a, env)
        assert len(list((root/'v1/targets').iterdir())) == 3
        run(['gc', '--apply'], a, env)
        assert not list((root/'v1/targets').iterdir())
        assert (legacy/'keep').exists()
        assert (root/'v1/caches'/cache['id']/'artifacts').exists()
        print('PASS: 过期回收、dry-run、历史 target 保留、共享缓存保留')
        # Cache namespace expiry stops its daemon and deletes only this namespace.
        for entry in (root/'v1/caches').glob('*/entry.json'):
            data = json.loads(entry.read_text()); data['last_used'] = 0
            entry.write_text(json.dumps(data))
        run(['gc', '--apply'], a, env)
        assert not list((root/'v1/caches').iterdir())
        print('PASS: 过期缓存版本停止服务后回收')
        # Concurrent workspaces must not serialize on the shared cache lock. Killing
        # the supervisor must leave its Cargo child protected from garbage collection.
        markers = base/'markers'
        markers.mkdir()
        long_build = r'''fn main() {
            let base = std::path::PathBuf::from(std::env::var("CSLIM_SMOKE_MARKERS").unwrap());
            let name = std::env::var("CARGO_PKG_NAME").unwrap();
            std::fs::write(base.join(format!("{name}.started")), "started").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(5));
            std::fs::write(base.join(format!("{name}.done")), "done").unwrap();
        }'''
        for project in (a, c):
            (project/'build.rs').write_text(long_build)
        concurrent_env = dict(env, CSLIM_SMOKE_MARKERS=str(markers))
        with (base/'concurrent.log').open('w') as log:
            processes = [subprocess.Popen([str(binary), 'build', '--offline'], cwd=project,
                         env=concurrent_env, stdout=log, stderr=log) for project in (a, c)]
            try:
                deadline = time.monotonic()+15
                while not all((markers/f'smoke-{name}.started').exists() for name in ('a','c')):
                    assert time.monotonic() < deadline, (base/'concurrent.log').read_text()
                    assert all(p.poll() is None for p in processes), (base/'concurrent.log').read_text()
                    time.sleep(0.05)
                assert not (markers/'smoke-a.done').exists(), 'cross-workspace builds were serialized'
                processes[0].kill()
                processes[0].wait(timeout=5)
                for entry in (root/'v1/targets').glob('*/entry.json'):
                    data = json.loads(entry.read_text()); data['last_used'] = 0
                    entry.write_text(json.dumps(data))
                run(['gc', '--apply'], a, env)
                assert len(list((root/'v1/targets').iterdir())) == 2
                processes[1].wait(timeout=20)
                assert processes[1].returncode == 0, (base/'concurrent.log').read_text()
                deadline = time.monotonic()+15
                while True:
                    entries = json.loads(run(['status','--json'], a, env).stdout)
                    if not any(e['busy'] for e in entries if e['kind']=='targets'):
                        break
                    assert time.monotonic() < deadline, 'child build lock leaked'
                    time.sleep(0.05)
                assert (markers/'smoke-a.done').exists()
                run(['gc', '--apply'], a, env)
                assert len(list((root/'v1/targets').iterdir())) == 1
                print('PASS: 并发构建互不阻塞；supervisor 被 SIGKILL 后子构建仍受回收锁保护')
            finally:
                for process in processes:
                    if process.poll() is None:
                        process.kill(); process.wait(timeout=5)
    finally:
        for socket in (root/'v1/sockets').glob('*.sock'):
            stop_env = {k: v for k,v in env.items() if not k.startswith('SCCACHE_')}
            stop_env.update(SCCACHE_SERVER_UDS=str(socket), SCCACHE_CONF=str(root/'v1/sccache.toml'))
            subprocess.run([sccache, '--stop-server'], env=stop_env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
