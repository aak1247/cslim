#!/usr/bin/env python3
"""盘点/迁移注册仓库各工作树的默认 target；默认预览，所有产物进入 cslim 配置根目录。"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import time
import tomllib
import uuid
from legacy_probe import references

MIGRATION_PREFIX = '.cslim-migration-'
LEGACY_MIGRATION_PREFIX = '.cargo-harbor-migration-'


def atomic_json(path, value):
    temporary = path.with_name(path.name+'.tmp-'+uuid.uuid4().hex)
    with temporary.open('x') as output:
        os.chmod(temporary, 0o600)
        json.dump(value, output, ensure_ascii=False, indent=2)
        output.flush(); os.fsync(output.fileno())
    os.replace(temporary, path)


def snapshot(path, *, hashes=False):
    root = path.lstat()
    if not stat.S_ISDIR(root.st_mode) or path.resolve() != path or root.st_uid != os.getuid():
        raise RuntimeError('target 必须为当前用户所有且路径不经过符号链接')
    newest = 0
    size = 0
    signature = hashlib.sha256()
    contents = {}
    for directory, dirs, files in os.walk(path, followlinks=False):
        dirs.sort(); files.sort()
        if '.git' in dirs or '.git' in files:
            raise RuntimeError('target 内含 Git 源码/工作树，拒绝迁移')
        for item in [Path(directory), *(Path(directory)/name for name in files),
                     *(Path(directory)/name for name in dirs if (Path(directory)/name).is_symlink())]:
            metadata = item.lstat()
            if metadata.st_dev != root.st_dev:
                raise RuntimeError('target 内含跨文件系统目录')
            if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode)):
                raise RuntimeError('target 内含特殊文件')
            newest = max(newest, metadata.st_mtime, metadata.st_ctime)
            size += metadata.st_blocks*512
            rel = str(item.relative_to(path))
            # Renaming the root changes its ctime only. All other metadata is checked.
            key = (rel, metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_size,
                   metadata.st_mtime_ns, 0 if item == path else metadata.st_ctime_ns)
            signature.update(repr(key).encode()+b'\n')
            if hashes:
                if item.is_symlink(): contents[rel] = ['link', os.readlink(item)]
                elif item.is_file():
                    with item.open('rb') as stream:
                        contents[rel] = ['file', metadata.st_size, hashlib.file_digest(stream, 'sha256').hexdigest()]
                else: contents[rel] = ['directory']
    return {'newest': newest, 'bytes': size, 'signature': signature.hexdigest(), 'contents': contents}


def cargo_locks(path):
    descriptors = []
    try:
        for directory, dirs, files in os.walk(path, followlinks=False):
            dirs[:] = [d for d in dirs if d not in ('deps', 'incremental', 'build', '.fingerprint', 'examples') and not (Path(directory)/d).is_symlink()]
            if '.cargo-lock' in files:
                descriptor = os.open(Path(directory)/'.cargo-lock', os.O_RDONLY|os.O_NOFOLLOW)
                descriptors.append(descriptor)
                fcntl.flock(descriptor, fcntl.LOCK_EX|fcntl.LOCK_NB)
        if not descriptors:
            raise RuntimeError('缺少 Cargo 构建锁文件，无法确认迁移边界')
        return descriptors
    except BaseException:
        for descriptor in descriptors: os.close(descriptor)
        raise


def require_markers(path):
    if not ((path/'.rustc_info.json').is_file() or
            any((path/profile/'.fingerprint').is_dir() for profile in ('debug', 'release'))):
        raise RuntimeError('缺少 Rust 构建标记')


def require_untracked(path):
    root = subprocess.check_output(['git', '-C', str(path.parent), 'rev-parse', '--show-toplevel'], text=True).strip()
    relative = str(path.relative_to(Path(root).resolve()))
    result = subprocess.run(['git', '-C', root, 'ls-files', '-z', '--', relative], capture_output=True, check=True)
    if result.stdout: raise RuntimeError('target 含 Git 跟踪文件')


def move_target(source, info, *, check_refs=references, min_idle_hours=48, copy_always=False, max_bytes=0):
    """可恢复事务；检查失败恢复 source；SIGKILL 后保留 journal 与 quarantine。"""
    source = Path(source)
    destination = Path(info['target_dir'])
    base = Path(info['root'])/'v1'
    entry_dir = destination.parent
    if destination != base/'targets'/info['id']/'artifacts' or source.name != 'target':
        raise RuntimeError('迁移路径不符合 cslim 目录边界')
    # Core CLI must have validated the root before this helper receives metadata.
    for part in (base, base/'targets', base/'locks'):
        m = part.lstat()
        if not stat.S_ISDIR(m.st_mode) or m.st_uid != os.getuid() or m.st_mode & 0o022:
            raise RuntimeError('cslim 根目录权限不安全')
    if source.resolve() == destination: raise RuntimeError('源目录已经受管')
    lock_path = base/'locks'/f"targets-{info['id']}.lock"
    lock = os.open(lock_path, os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW, 0o600)
    descriptors = []
    quarantine = source.with_name(MIGRATION_PREFIX+uuid.uuid4().hex)
    journal_dir = base/'imports'
    journal_dir.mkdir(mode=0o700, exist_ok=True)
    if journal_dir.is_symlink() or journal_dir.stat().st_uid != os.getuid():
        os.close(lock); raise RuntimeError('imports 目录不安全')
    journal = journal_dir/(quarantine.name.removeprefix(MIGRATION_PREFIX)+'.json')
    state = {'source': str(source), 'quarantine': str(quarantine), 'destination': str(destination),
             'id': info['id'], 'started': time.time(), 'state': 'checking'}
    committed = False
    created = False
    moved = False
    try:
        fcntl.flock(lock, fcntl.LOCK_EX|fcntl.LOCK_NB)
        if entry_dir.exists(): raise RuntimeError('该 workspace/工具链已有托管目录，保留旧 target')
        require_markers(source); require_untracked(source)
        descriptors = cargo_locks(source)
        original_mode = stat.S_IMODE(source.stat().st_mode)
        before = snapshot(source)
        if time.time()-before['newest'] < min_idle_hours*3600:
            raise RuntimeError('近期有文件变化')
        if max_bytes and before['bytes'] > max_bytes:
            raise RuntimeError('超出迁移容量预算')
        if shutil.disk_usage(base).free < before['bytes']+20*1024**3:
            raise RuntimeError('NVMe 可用空间不足，需保留 20 GiB')
        if any(check_refs([str(source)]).values()): raise RuntimeError('存在进程、容器或挂载引用')
        if snapshot(source)['signature'] != before['signature']: raise RuntimeError('检查期间 target 已改变')
        state.update(state='prepared', source_signature=before['signature'], bytes=before['bytes'])
        atomic_json(journal, state)
        entry_dir.mkdir(mode=0o700); created = True
        atomic_json(entry_dir/'importing.json', {'journal': str(journal), 'source': str(source)})
        os.rename(source, quarantine)
        state['state'] = 'quarantined'; atomic_json(journal, state)
        if any(check_refs([str(source), str(quarantine)]).values()): raise RuntimeError('暂存后出现活动引用')
        if snapshot(quarantine)['signature'] != before['signature']: raise RuntimeError('暂存期间 target 已改变')
        cross_device = copy_always or quarantine.stat().st_dev != base.stat().st_dev
        if cross_device:
            state['state'] = 'copying'; atomic_json(journal, state)
            expected = snapshot(quarantine, hashes=True)
            shutil.copytree(quarantine, destination, symlinks=True)
            copied = snapshot(destination, hashes=True)
            if copied['contents'] != expected['contents'] or snapshot(quarantine)['signature'] != before['signature']:
                raise RuntimeError('复制完整性校验失败或源目录有变化')
        else:
            os.rename(quarantine, destination); moved = True
        active = check_refs([str(source), str(destination), str(quarantine)])
        if any(active.values()): raise RuntimeError('提交前发现活动引用')
        os.chmod(destination, 0o700)
        imported = {'schema': 1, 'kind': 'targets', 'id': info['id'], 'workspace': info['workspace'],
                    'compiler': info['compiler'], 'created_at': int(time.time()), 'last_used': int(time.time()),
                    'imported_from': str(source), 'imported_source_last_modified': before['newest']}
        # A committed entry is the ownership transfer point. GC cannot acquire our lock yet.
        atomic_json(entry_dir/'entry.json', imported)
        committed = True
        state['state'] = 'committed'; atomic_json(journal, state)
        (entry_dir/'importing.json').unlink()
        if cross_device:
            if any(check_refs([str(quarantine)]).values()):
                raise RuntimeError('已复制并纳管；旧暂存目录出现引用，保留以待恢复处理')
            shutil.rmtree(quarantine)
        state['state'] = 'complete'; state['finished'] = time.time(); atomic_json(journal, state)
        return {'source': str(source), 'destination': str(destination), 'bytes': before['bytes'],
                'copy_verified': cross_device, 'journal': str(journal)}
    except BaseException as error:
        if not committed:
            if moved and destination.exists() and not quarantine.exists(): os.rename(destination, quarantine)
            if quarantine.exists() and not os.path.lexists(source):
                os.rename(quarantine, source)
                os.chmod(source, original_mode)
            if created and entry_dir.exists() and not quarantine.exists(): shutil.rmtree(entry_dir)
            state['state'] = 'rolled_back' if not quarantine.exists() else 'recovery_required'
        else: state['state'] = 'cleanup_required'
        state['error'] = str(error)
        if journal.exists(): atomic_json(journal, state)
        raise
    finally:
        for descriptor in descriptors: os.close(descriptor)
        os.close(lock)


def recover_journal(journal, root, *, check_refs=references):
    journal = Path(journal).resolve()
    root = Path(root).resolve()
    if journal.parent != root/'v1/imports' or journal.stat().st_uid != os.getuid():
        raise RuntimeError('恢复日志不属于当前 cslim 根目录')
    state = json.loads(journal.read_text())
    source, quarantine, destination = map(Path, (state['source'], state['quarantine'], state['destination']))
    valid_quarantine = {prefix + journal.stem for prefix in (MIGRATION_PREFIX, LEGACY_MIGRATION_PREFIX)}
    if (quarantine.parent != source.parent or quarantine.name not in valid_quarantine
            or destination != root/'v1/targets'/state['id']/'artifacts'):
        raise RuntimeError('恢复日志路径不符合边界')
    entry = destination.parent
    lock = os.open(root/'v1/locks'/f"targets-{state['id']}.lock",os.O_RDWR|os.O_NOFOLLOW)
    descriptors=[]
    try:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        if (entry/'entry.json').exists():
            if quarantine.exists():
                descriptors=cargo_locks(quarantine)
                if snapshot(quarantine)['signature'] != state['source_signature']:
                    raise RuntimeError('旧暂存数据有变化，保留以人工检查')
                if any(check_refs([str(quarantine)]).values()): raise RuntimeError('旧暂存目录仍有引用')
                shutil.rmtree(quarantine)
            state['state']='complete'
        else:
            if os.path.lexists(source): raise RuntimeError('原路径已有目录，未覆盖；保留迁移副本以待人工检查')
            origin = quarantine if quarantine.exists() else destination
            if not origin.exists(): raise RuntimeError('没有可恢复副本')
            marker=json.loads((entry/'importing.json').read_text())
            if marker.get('journal') != str(journal): raise RuntimeError('迁移归属标记不匹配')
            descriptors=cargo_locks(origin)
            if any(check_refs([str(origin),str(source)]).values()): raise RuntimeError('恢复目录存在活动引用')
            os.rename(origin,source)
            if entry.exists(): shutil.rmtree(entry)
            state['state']='rolled_back'
        state['recovered_at']=time.time();atomic_json(journal,state)
        return state['state']
    finally:
        for descriptor in descriptors: os.close(descriptor)
        os.close(lock)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default=str(Path.home()/'.local/bin/cargo-cslim'))
    parser.add_argument('--project', action='append', help='重复指定注册项目；默认扫描全部注册项目')
    parser.add_argument('--apply', action='store_true')
    parser.add_argument('--min-idle-hours', type=float, default=48)
    parser.add_argument('--report', type=Path)
    parser.add_argument('--recover', type=Path, help='恢复中断的迁移日志，不覆盖原路径')
    args = parser.parse_args()
    if args.min_idle_hours < 0: parser.error('min-idle-hours 不可为负')
    cfg = tomllib.loads(subprocess.check_output([args.binary, 'config'], text=True))
    root = Path(cfg['root'])
    if args.recover:
        print(recover_journal(args.recover, root)); return
    if not args.report: parser.error('盘点或迁移必须指定 --report')
    root_real = root.resolve()
    if root_real != root: raise SystemExit('cslim root 不可经过链接')
    projects = args.project or list(cfg['projects'])
    found = {}
    report = {'root': str(root), 'started': time.time(), 'mode': 'apply' if args.apply else 'dry-run',
              'min_idle_hours': args.min_idle_hours, 'worktrees': {}, 'targets': [], 'imported': []}
    args.report.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    worktree_cache = {}
    for project in projects:
        registered = Path(cfg['projects'][project])
        repo = Path(subprocess.check_output(['git','-C',str(registered.parent),'rev-parse','--show-toplevel'],text=True).strip()).resolve()
        relative = registered.relative_to(repo)
        key = str(repo)
        if key not in worktree_cache:
            print('扫描工作树', repo, flush=True)
            worktree_cache[key] = json.loads(subprocess.check_output([args.binary, 'worktrees', '--project', project, '--all', '--json'], text=True))
        entries = worktree_cache[key]
        report['worktrees'][project] = {'registered': len(entries), 'present': sum(e['available'] for e in entries)}
        for entry in entries:
            if not entry['available']: continue
            manifest = Path(entry['path'])/relative
            source = manifest.parent/'target'
            if os.path.lexists(source) and manifest.is_file() and str(source) not in found:
                found[str(source)] = manifest
    print('批量检查进程和容器引用', flush=True)
    initial_references = references(list(found)) if found else {}
    used = sum(snapshot(p)['bytes'] for p in (root/'v1/targets').glob('*/artifacts') if p.is_dir())
    limit = cfg['target_max_gib']*1024**3
    print(f'cslim root: {root}；发现 {len(found)} 个历史 target', flush=True)
    for source_text, manifest in sorted(found.items()):
        source = Path(source_text)
        row = {'source': source_text, 'manifest': str(manifest)}
        try:
            require_markers(source); require_untracked(source)
            data = snapshot(source)
            row.update(bytes=data['bytes'], newest=data['newest'])
            if time.time()-data['newest'] < args.min_idle_hours*3600: raise RuntimeError('近期有文件变化')
            info = json.loads(subprocess.check_output([args.binary, 'target-info', '--manifest-path', str(manifest)], cwd=manifest.parent, text=True))
            row['destination'] = info['target_dir']
            if Path(info['target_dir']).parent.exists(): raise RuntimeError('已有托管目录或未完成迁移')
            if limit and used+data['bytes'] > limit: raise RuntimeError('超出 target 总容量预算，未迁移')
            fds = cargo_locks(source)
            try:
                hits = {source_text: initial_references[source_text]}
                row['protections'] = hits[source_text]
                if any(hits.values()): raise RuntimeError('存在活动引用')
            finally:
                for fd in fds: os.close(fd)
            row['status'] = 'eligible'
            if args.apply:
                imported = move_target(source, info, min_idle_hours=args.min_idle_hours,
                                       max_bytes=max(0, limit-used) if limit else 0)
                report['imported'].append(imported)
                row['status'] = 'imported'
            used += data['bytes']
        except Exception as error:
            row.update(status='kept', reason=str(error))
        report['targets'].append(row)
        atomic_json(args.report, report)
        print(row['status'], f"{row.get('bytes', 0)/1024**3:.2f} GiB", source_text, row.get('reason', ''), flush=True)
    report['finished'] = time.time(); atomic_json(args.report, report)
    print(f"完成：{len(report['imported'])} 个已迁移；报告 {args.report}", flush=True)


if __name__ == '__main__':
    main()
