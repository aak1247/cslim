#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import shutil

BINARY = Path(sys.argv.pop(1)).resolve() if len(sys.argv)>1 else Path.home()/'.local/bin/cargo-cslim'

class WorktreeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='cslim-worktrees-')
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.main = self.base/'main'
        (self.main/'packages/demo/src').mkdir(parents=True)
        (self.main/'packages/demo/Cargo.toml').write_text('[package]\nname="worktree-fixture"\nversion="0.1.0"\nedition="2024"\n')
        (self.main/'packages/demo/src/lib.rs').write_text('pub fn value() -> u32 { 42 }\n')
        self.git('init', '-q', '-b', 'main')
        self.git('add', 'packages')
        self.git('-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','fixture')
        self.linked = self.base/'old worktree'
        self.git('worktree','add','-q','-b','historical',str(self.linked))
        config = self.base/'config.toml'
        sccache = shutil.which('sccache')
        self.assertIsNotNone(sccache, 'sccache is required for worktree integration tests')
        config.write_text(f'root={json.dumps(str(self.base/"cache"))}\nsccache={json.dumps(sccache)}\nauto_gc=false\n[projects]\nfixture={json.dumps(str(self.main/"packages/demo/Cargo.toml"))}\n')
        self.env = dict(os.environ, CSLIM_CONFIG=str(config))
        self.env.pop('CSLIM_HOME', None)
    def git(self,*args):
        return subprocess.run(['git','-C',str(self.main),*args],capture_output=True,text=True,check=True)
    def call(self,*args):
        return subprocess.run([str(BINARY),*args],cwd=self.base,env=self.env,capture_output=True,text=True)
    def test_lists_and_selects_historical_worktree(self):
        result = self.call('worktrees','--project','fixture','--json')
        self.assertEqual(result.returncode,0,result.stderr)
        entries = json.loads(result.stdout)
        self.assertEqual(len(entries),2)
        self.assertTrue(all(e['manifest_exists'] for e in entries))
        target = self.call('project','fixture','--worktree',str(self.linked),'target-dir')
        current = self.call('project','fixture','target-dir')
        self.assertEqual(target.returncode,0,target.stderr)
        self.assertNotEqual(target.stdout,current.stdout)
        built = self.call('project','fixture','--worktree',str(self.linked),'build','--offline')
        self.assertEqual(built.returncode,0,built.stderr)
        info = self.call('target-info','--manifest-path',str(self.linked/'packages/demo/Cargo.toml'))
        self.assertEqual(json.loads(info.stdout)['target_dir'],target.stdout.strip())
    def test_rejects_unrelated_repository_and_subdirectory(self):
        other=self.base/'other'; other.mkdir()
        subprocess.run(['git','init','-q',str(other)],check=True)
        for path in (other,self.linked/'packages'):
            result=self.call('project','fixture','--worktree',str(path),'target-dir')
            self.assertNotEqual(result.returncode,0)
    def test_missing_worktree_is_listed_only_with_all(self):
        import shutil
        shutil.rmtree(self.linked)
        visible=json.loads(self.call('worktrees','--project','fixture','--json').stdout)
        all_entries=json.loads(self.call('worktrees','--project','fixture','--all','--json').stdout)
        self.assertEqual(len(visible),1)
        self.assertEqual(len(all_entries),2)
        self.assertFalse(next(e for e in all_entries if e['path']==str(self.linked))['available'])

if __name__=='__main__': unittest.main()
