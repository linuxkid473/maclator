#!/usr/bin/env python3
"""Parent-only math checker; does not run guest, translator or benchmark.

This file was prepared but not executed by the source/design agent.
"""
import json
from pathlib import Path
patterns = [0, 1 << 17, 1 << 31, 1 << 32, 1 << 33, 1 << 63,
            0xAB00000000000000, 0xFFFFFFFFFFFE0000]
base_patterns = [0, 0x100000000, 0xFFFFFFFFFFFFF000]
checks = 0
checksum = 0
for high in patterns:
    assert high & 0x1FFFF == 0
    for low in range(1 << 17):
        target = high | low
        old = (((target & 0xFFFFFFFF) >> 2) & 0x7FFF) << 4
        new = (target & 0x1FFFC) << 2
        insert = ((target >> 2) & (32768 - 1)) * 16
        assert old == new == insert, (target, old, new, insert)
        assert 0 <= old <= 524272
        for base in base_patterns:
            assert (base + old) & ((1 << 64) - 1) == (base + new) & ((1 << 64) - 1)
        checksum ^= old ^ target
        checks += 1
result = {'status': 'PASS', 'scope': 'integer_model_only_not_generated_machine_code',
          'target_cases': checks, 'base_cases_per_target': len(base_patterns),
          'low17_bits_exhaustive': True, 'high_patterns': patterns,
          'full_tag_note': 'candidate keeps comparison of unmodified RAX; model does not execute it',
          'checksum': format(checksum, '016x')}
path = Path(__file__).with_name('index-model-results.json')
path.write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result, indent=2))
