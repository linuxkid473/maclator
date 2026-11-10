#!/usr/bin/env python3
"""Check aligned/unaligned ARM pairs and CAS/store atomicity on a built runtime."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', type=Path, required=True)
    parser.add_argument('--sysroot', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    source = Path(__file__).parent / 'compatibility_matrix'
    results = []
    variants = [('pair-access', 'pair_access.c', []),
                ('atomics-apple', 'atomics.c', ['-pthread']),
                ('atomics-llsc', 'atomics.c', ['-pthread', '-mcpu=generic', '-march=armv8-a']),
                ('atomics-lse', 'atomics.c', ['-pthread', '-mcpu=generic', '-march=armv8.1-a'])]
    for name, filename, flags in variants:
        binary = args.output.resolve() / name
        subprocess.run(['/usr/bin/clang', '-arch', 'arm64', '-O2', *flags,
                        str(source / filename), '-o', str(binary)], check=True, timeout=30)
        subprocess.run(['/usr/bin/codesign', '-f', '-s', '-', str(binary)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=10)
        cases = [([], 'default')]
        if name.startswith('atomics'):
            cases.append((['20000', 'store'], 'store'))
        for extra, label in cases:
            command = ['/usr/bin/nice', '-n', '10', str(args.runtime.resolve()),
                       '--no-aot', '--sysroot', str(args.sysroot.resolve()), str(binary), *extra]
            env = dict(os.environ, MACLATOR_JIT_THRESHOLD='1')
            child = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     env=env, start_new_session=True)
            timeout = False
            try:
                log, _ = child.communicate(timeout=30)
            except subprocess.TimeoutExpired:
                timeout = True
                os.killpg(child.pid, signal.SIGKILL)
                log, _ = child.communicate()
            except BaseException:
                os.killpg(child.pid, signal.SIGKILL)
                child.communicate()
                raise
            text = log.decode(errors='replace')
            passed = not timeout and child.returncode == 0 and ' PASS' in text
            result = dict(case=f'{name}-{label}', passed=passed, timeout=timeout,
                          returncode=child.returncode)
            results.append(result)
            (args.output / f'{name}-{label}.log').write_text(text)
            (args.output / 'results.json').write_text(json.dumps(results, indent=2)+'\n')
            print(result, flush=True)
    return 0 if all(row['passed'] for row in results) else 1


if __name__ == '__main__':
    raise SystemExit(main())
