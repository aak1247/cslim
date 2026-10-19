#!/usr/bin/env python3
"""安装固定版本 sccache，下载后校验 SHA-256；仅支持 Linux x86_64。"""
import hashlib
import io
from pathlib import Path
import platform
import tarfile
import urllib.request

assert platform.system() == 'Linux' and platform.machine() == 'x86_64', '其他平台请自行安装 sccache'
version = '0.18.0'
expected = '45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89'
dest = Path.home()/'.local/bin/sccache'
if dest.exists():
    raise SystemExit(f'{dest} 已存在，保留现有二进制')
url = f'https://github.com/mozilla/sccache/releases/download/v{version}/sccache-v{version}-x86_64-unknown-linux-musl.tar.gz'
with urllib.request.urlopen(url, timeout=60) as response:
    data = response.read()
assert hashlib.sha256(data).hexdigest() == expected, 'SHA-256 不匹配'
with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
    member = next(m for m in archive.getmembers() if m.name.endswith('/sccache') and m.isfile())
    binary = archive.extractfile(member).read()
dest.parent.mkdir(parents=True, exist_ok=True)
with dest.open('xb') as output:
    output.write(binary)
dest.chmod(0o755)
print(f'已安装 sccache {version}: {dest}（SHA-256 已验证）')
