#!/usr/bin/env python3
"""Check GC preview/apply agreement using isolated managed fixtures, without Cargo."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

BINARY = Path(sys.argv.pop(1) if len(sys.argv) > 1 else 'target/debug/cargo-cslim').resolve()


class GcPreviewTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='cslim-gc-preview-')
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.root = self.base / 'managed'
        self.config = self.base / 'config.toml'
        self.config.write_text(f'root={json.dumps(str(self.root))}\n'
                               'target_max_gib=1\nsccache="/bin/false"\n')
        self.env = dict(os.environ, CSLIM_CONFIG=str(self.config))
        self.env.pop('CSLIM_HOME', None)
        self.call('status')  # Let the binary create private owned store directories.

    def call(self, *args):
        return subprocess.run([str(BINARY), *args], env=self.env,
                              capture_output=True, text=True, check=True)

    def target(self, target_id, age_days):
        entry = self.root / 'v1/targets' / target_id
        entry.mkdir(mode=0o700)
        deps = entry / 'artifacts/debug/deps'
        deps.mkdir(parents=True, mode=0o700)
        now = int(time.time())
        (entry / 'entry.json').write_text(json.dumps(dict(
            schema=1, kind='targets', id=target_id, workspace='/fixture',
            compiler='fixture', created_at=now, last_used=now - age_days * 86400)))
        runner = deps / 'test-1111111111111111'
        runner.write_bytes(b'\x7fELF' + b'\0' * 12)
        runner.chmod(0o700)
        runner.with_suffix('.d').write_text('test: tests/example.rs\n')
        for index in (1, 2):
            lib = deps / f'libdemo-{str(index) * 16}.rlib'
            lib.write_text('variant')
            os.utime(lib, (now - 10 + index, now - 10 + index))
        return entry, runner

    def test_capacity_preview_stops_after_sufficient_pruning(self):
        older, runner = self.target('aa11', 2)
        younger, other_runner = self.target('bb22', 1)
        with runner.open('r+b') as handle:
            os.posix_fallocate(handle.fileno(), 0, (1 << 30) + 4096)
        result = self.call('gc', '--dry-run')
        self.assertIn('可回收 1.00 GiB', result.stderr)
        self.assertNotRegex(result.stderr, r'(?m)^预览 targets .*\(容量限制\)$')
        self.assertNotIn('targets bb22 prune', result.stderr)
        self.assertTrue(runner.exists())
        self.assertTrue(other_runner.exists())
        result = self.call('gc', '--apply')
        self.assertIn('已回收 1.00 GiB', result.stderr)
        self.assertTrue(older.exists())
        self.assertTrue(younger.exists())
        self.assertFalse(runner.exists())
        self.assertTrue(other_runner.exists())

    def test_expiry_preview_counts_each_byte_only_once(self):
        expired, runner = self.target('aa11', 15)
        original_bytes = json.loads(self.call('status', '--json').stdout)[0]['bytes']
        expected = f'{original_bytes / (1 << 20):.2f} MiB'
        result = self.call('gc', '--dry-run')
        self.assertIn('预览 targets aa11', result.stderr)
        self.assertIn(f'可回收 {expected}', result.stderr)
        self.assertTrue(runner.exists())
        result = self.call('gc', '--apply')
        self.assertIn(f'已回收 {expected}', result.stderr)
        self.assertFalse(expired.exists())


if __name__ == '__main__':
    unittest.main()
