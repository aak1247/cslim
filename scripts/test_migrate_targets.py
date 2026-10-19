#!/usr/bin/env python3
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('migration',Path(__file__).with_name('migrate-targets.py'))
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)

def no_references(paths): return {str(p):[] for p in paths}

class MigrationTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory(prefix='cslim-import-')
        self.addCleanup(self.tmp.cleanup)
        self.base=Path(self.tmp.name)
        self.repo=self.base/'repo'; self.repo.mkdir()
        subprocess.run(['git','init','-q',str(self.repo)],check=True)
        self.source=self.repo/'target'
        (self.source/'debug/.fingerprint').mkdir(parents=True)
        (self.source/'debug/.cargo-lock').touch()
        (self.source/'debug/output').write_text('precious rebuildable data')
        self.root=self.base/'managed'
        for name in ('targets','locks'): (self.root/'v1'/name).mkdir(mode=0o700,parents=True)
        os.chmod(self.root/'v1',0o700)
        self.info={'root':str(self.root),'id':'abc','target_dir':str(self.root/'v1/targets/abc/artifacts'),'workspace':str(self.repo),'compiler':'rustc-fixture'}
    def move(self,**kw): return m.move_target(self.source,self.info,min_idle_hours=0,check_refs=no_references,**kw)
    def test_same_filesystem_transfer_is_recorded(self):
        result=self.move()
        self.assertFalse(self.source.exists())
        dest=Path(result['destination'])
        self.assertEqual((dest/'debug/output').read_text(),'precious rebuildable data')
        self.assertEqual(dest.stat().st_mode & 0o777,0o700)
        self.assertEqual(json.loads((dest.parent/'entry.json').read_text())['kind'],'targets')
        self.assertEqual(json.loads(Path(result['journal']).read_text())['state'],'complete')
    def test_copy_path_checks_contents_and_preserves_symlinks(self):
        outside=self.base/'outside'; outside.write_text('outside')
        (self.source/'debug/link').symlink_to(outside)
        result=self.move(copy_always=True)
        self.assertTrue(result['copy_verified'])
        self.assertTrue((Path(result['destination'])/'debug/link').is_symlink())
        self.assertEqual(outside.read_text(),'outside')
    def test_copy_failure_restores_original_and_removes_partial_stage(self):
        def broken(source,destination,**kw):
            Path(destination).mkdir()
            (Path(destination)/'partial').write_text('partial copy')
            raise RuntimeError('simulated disk failure')
        with patch.object(m.shutil,'copytree',side_effect=broken):
            with self.assertRaises(RuntimeError): self.move(copy_always=True)
        self.assertTrue((self.source/'debug/output').exists())
        self.assertFalse(Path(self.info['target_dir']).parent.exists())
    def test_live_process_reference_stops_migration(self):
        def busy(paths): return {str(p):['active'] for p in paths}
        with self.assertRaises(RuntimeError):
            m.move_target(self.source,self.info,min_idle_hours=0,check_refs=busy)
        self.assertTrue(self.source.exists())
    def test_tracked_files_and_recent_changes_are_protected(self):
        with self.assertRaises(RuntimeError):
            m.move_target(self.source,self.info,min_idle_hours=48,check_refs=no_references)
        subprocess.run(['git','-C',str(self.repo),'add','target/debug/output'],check=True)
        with self.assertRaises(RuntimeError): self.move()
        self.assertTrue(self.source.exists())
    def test_existing_managed_target_is_never_overwritten(self):
        destination=Path(self.info['target_dir']);destination.mkdir(parents=True)
        (destination/'keep').write_text('already managed')
        with self.assertRaises(RuntimeError): self.move()
        self.assertEqual((destination/'keep').read_text(),'already managed')
    def test_recovery_restores_a_quarantined_source(self):
        # Simulate interruption after quarantine + partial NVMe copy, before commit.
        import uuid
        journal_id=uuid.uuid4().hex
        quarantine=self.source.with_name('.cslim-migration-'+journal_id)
        before=m.snapshot(self.source)
        os.rename(self.source,quarantine)
        entry=Path(self.info['target_dir']).parent;entry.mkdir()
        imports=self.root/'v1/imports';imports.mkdir(mode=0o700)
        (self.root/'v1/locks/targets-abc.lock').touch()
        journal=imports/(journal_id+'.json')
        m.atomic_json(journal,{'source':str(self.source),'quarantine':str(quarantine),
            'destination':self.info['target_dir'],'id':'abc','state':'copying','source_signature':before['signature']})
        m.atomic_json(entry/'importing.json',{'journal':str(journal)})
        Path(self.info['target_dir']).mkdir()
        (Path(self.info['target_dir'])/'partial').write_text('partial')
        result=m.recover_journal(journal,self.root,check_refs=no_references)
        self.assertEqual(result,'rolled_back')
        self.assertTrue((self.source/'debug/output').exists())
        self.assertFalse(entry.exists())

    def test_cargo_lock_is_respected(self):
        import fcntl
        with (self.source/'debug/.cargo-lock').open('r') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            with self.assertRaises(BlockingIOError): self.move()
        self.assertTrue(self.source.exists())

if __name__=='__main__': unittest.main()
