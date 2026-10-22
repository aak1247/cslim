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
        result = subprocess.run([str(BINARY), *args], env=self.env,
                                capture_output=True, text=True)
        if result.returncode:
            self.fail(f'command {args!r} failed: {result.stderr}')
        return result

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

    def test_persistent_pin_protects_target_until_matching_owner_unpins(self):
        entry, _runner = self.target('cc22', 15)
        target_dir = entry / 'artifacts'
        for path in (target_dir, target_dir / 'debug', target_dir / 'debug/deps'):
            path.chmod(0o700)
        result = self.call('pin', '--target-dir', str(target_dir),
                           '--owner', 'container-adapter',
                           '--reason', 'container build target',
                           '--metadata', '{"container_id":"sha256:deadbeef"}')
        pin = json.loads(result.stdout)
        self.assertEqual(pin['target_id'], 'cc22')
        self.assertEqual(pin['owner'], 'container-adapter')
        self.assertEqual(pin['metadata']['container_id'], 'sha256:deadbeef')
        self.assertEqual(len(pin['id']), 32)

        rows = json.loads(self.call('status', '--json').stdout)
        row = next(item for item in rows if item['id'] == 'cc22')
        self.assertEqual(row['pins'][0]['id'], pin['id'])
        self.assertEqual(json.loads(self.call('pins', '--json').stdout)[0]['id'], pin['id'])

        preview = self.call('gc')
        self.assertIn('pin 保护 target cc22', preview.stderr)
        self.call('gc', '--apply')
        self.assertTrue(entry.exists())

        wrong_owner = subprocess.run(
            [str(BINARY), 'unpin', pin['id'], '--owner', 'wrong-owner'],
            env=self.env, capture_output=True, text=True)
        self.assertNotEqual(wrong_owner.returncode, 0)
        self.call('unpin', pin['id'], '--owner', 'container-adapter')
        self.call('gc', '--apply')
        self.assertFalse(entry.exists())


if __name__ == '__main__':
    unittest.main()
