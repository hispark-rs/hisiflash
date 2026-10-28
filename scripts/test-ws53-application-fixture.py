#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Offline candidate02 fixture checks using private caller-supplied artifacts.

Never opens a real serial port. Vendor Flash bytes are not included as fixtures.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile


def run(a):
    rows = []
    with tempfile.TemporaryDirectory(prefix='ws53-plan-test-') as folder:
        work = Path(folder)
        def execute(loader=a.loader, backup=a.backup, plan=a.plan, expected=None):
            output = work/'must-not-create'
            result = subprocess.run([str(a.writer), '/nonexistent-ws53-port', str(loader),
                str(backup), str(plan), str(output), 'check'], capture_output=True, text=True, timeout=10)
            assert not output.exists(), 'offline check created hardware output'
            if expected:
                assert result.returncode != 0 and expected in result.stderr, result
            else:
                assert result.returncode == 0 and 'PLAN_VALIDATED' in result.stdout, result
            rows.append({'expected_rejection': expected, 'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr})
        execute()
        for name, source in [('loader', a.loader), ('backup', a.backup)]:
            data = bytearray(source.read_bytes())
            data[0] ^= 1
            changed = work/name
            changed.write_bytes(data)
            execute(**{name: changed}, expected='unqualified loader/backup')
        for name in ('manifest', 'original', 'replacement', 'missing'):
            plan = work/name
            shutil.copytree(a.plan, plan)
            filename = 'sectors.tsv' if name == 'manifest' else f'05d000-{name if name != "missing" else "replacement"}.bin'
            file = plan/filename
            if name == 'missing':
                file.unlink()
                error = 'No such file'
            else:
                data = bytearray(file.read_bytes())
                data[0] ^= 1
                file.write_bytes(data)
                error = 'unqualified candidate plan' if name == 'manifest' else 'plan payload hash/original mismatch'
            execute(plan=plan, expected=error)
    return {'passed': True, 'hardware_accessed': False, 'cases': rows}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('writer', 'loader', 'backup', 'plan'):
        parser.add_argument('--'+name, type=lambda p: Path(p).resolve(), required=True)
    print(json.dumps(run(parser.parse_args()), indent=2))
