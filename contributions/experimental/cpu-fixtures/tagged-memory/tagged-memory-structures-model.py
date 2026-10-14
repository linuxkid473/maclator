#!/usr/bin/env python3
"""Source/data-only independent Python oracle; never compiles or runs C/ARM."""
from pathlib import Path
from collections import Counter
import hashlib
import json
import re
import struct

BASE = Path(__file__).resolve().parent
SOURCE = BASE / 'tagged-memory-structures.c'
MASK64 = (1 << 64)-1


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    assert not (BASE / 'tagged-memory-structures-source.json').exists()
    source_sha = sha(SOURCE)
    cases = re.findall(r'^ X\((\w+), "([^"]+)", (\w+),(\d+),(-?\d+),(-?\d+),(\d+),(\d+),(\d+),(\d+)\)',
                       SOURCE.read_text(), re.M)
    assert len(cases) == 117
    oracle_hash = 1469598103934665603
    executions = 0
    ranges = []
    for ci, c in enumerate(cases):
        name, instruction, kind = c[:3]
        width, offset, wb, lane, nr, vb, operand = map(int, c[3:])
        size = vb*4 if kind in ('FOURLOAD','FOURSTORE') else width*nr if kind in (
            'PLOAD','PSTORE','PSIGNED','QPLOAD','QPSTORE','MLOAD','MSTORE') else width
        assert 0 <= 96+offset <= 96+offset+size <= 256
        assert 0 <= 96+wb < 256
        flags = Counter()
        for tag in range(256):
            memory = bytearray((j*37+0x91+tag)&255 for j in range(256))
            vec = bytearray((0xe3+j*11)&255 for j in range(64))
            original_vec = bytes(vec)
            p = 96+offset
            a, b = 0x8f765432fedcba91, 0xbadc0ffe87654321
            flag = (tag+ci)&15
            flags[flag] += 1
            low_mask = (1 << (width*8))-1
            def load(position=p, count=width):
                return int.from_bytes(memory[position:position+count], 'little')
            def store(value, position=p, count=width):
                memory[position:position+count] = (value & ((1<<(count*8))-1)).to_bytes(count, 'little')
            def signed(value):
                return value if value < 1 << (width*8-1) else value-(1 << (width*8))
            if operand in (1,2):
                a = load() ^ (1 if operand==2 else 0)
            elif operand==3:
                a = 64
            result_a, result_b = a, b
            if kind=='GLOAD': result_a=load()
            elif kind=='GSIGNED': result_a=signed(load())&MASK64
            elif kind=='GSTORE': store(a)
            elif kind=='CAS':
                old=load()
                if a & low_mask == old: store(b)
                result_a=old
            elif kind in ('ADD','CLR','EOR','SET','SWAP'):
                old=load()
                value = old+a if kind=='ADD' else old&~a if kind=='CLR' else old^a if kind=='EOR' else old|a if kind=='SET' else a
                store(value)
                result_b=old
            elif kind=='VLOAD':
                vec[:16]=bytes(16)
                vec[:width]=memory[p:p+width]
            elif kind=='VSTORE': memory[p:p+width]=original_vec[:width]
            elif kind in ('PLOAD','PSIGNED'):
                values=[load(p+j*width) for j in range(2)]
                if kind=='PSIGNED': values=[signed(v)&MASK64 for v in values]
                result_a,result_b=values
            elif kind=='PSTORE': store(a); store(b,p+width)
            elif kind in ('QPLOAD','MLOAD'):
                for j in range(nr):
                    vec[j*16:j*16+16]=bytes(16)
                    vec[j*16:j*16+width]=memory[p+j*width:p+(j+1)*width]
            elif kind in ('QPSTORE','MSTORE'):
                for j in range(nr): memory[p+j*width:p+(j+1)*width]=original_vec[j*16:j*16+width]
            elif kind=='LLOAD': vec[lane*width:(lane+1)*width]=memory[p:p+width]
            elif kind=='LSTORE': memory[p:p+width]=original_vec[lane*width:(lane+1)*width]
            elif kind=='REPLICATE': vec[:16]=memory[p:p+width]*(16//width)
            elif kind=='FOURLOAD':
                vec[:]=bytes(64)
                for j in range(vb//width):
                    for k in range(4):
                        vec[k*16+j*width:k*16+(j+1)*width]=memory[p+(j*4+k)*width:p+(j*4+k+1)*width]
            elif kind=='FOURSTORE':
                for j in range(vb//width):
                    for k in range(4):
                        memory[p+(j*4+k)*width:p+(j*4+k+1)*width]=original_vec[k*16+j*width:k*16+(j+1)*width]
            else: raise AssertionError(kind)
            base=(tag<<56)|0x500000060
            assert ((base+wb)&MASK64)>>56==tag
            payload=struct.pack('<IIQQQQ',ci,tag,wb&MASK64,result_a,result_b,flag<<28)+vec+memory
            for byte in payload:
                oracle_hash=((oracle_hash^byte)*1099511628211)&MASK64
            executions += 1
        assert flags == Counter({j:16 for j in range(16)})
        ranges.append({'name':name,'arm_instruction':instruction,'semantic_kind':kind,
                       'arena_interval':[96+offset,96+offset+size],
                       'writeback_offset':96+wb,'all_writebacks_preserve_top_byte':True})
    assert sha(SOURCE)==source_sha
    expected = f'PASS tagged-memory cases=117 tags=256 nzcv-patterns-per-case=16 executions={executions} checksum={oracle_hash:016x}'
    result = {'status':'SOURCE_ONLY_NOT_COMPILED_NOT_RUN', 'source_sha256':source_sha,
              'oracle_source_sha256':sha(Path(__file__)),
              'python_semantic_oracle_only':True, 'c_or_arm_processes_started':False,
              'version':2, 'original_cases_preserved':103, 'new_nonpost_positive_cases':14, 'cases':len(cases), 'tags_per_case':256, 'executions':executions,
              'nzcv_patterns_per_case':16, 'tags_for_each_nzcv_pattern_per_case':16,
              'full_tag_x_nzcv_cartesian_product':False,
              'expected_stdout_from_independent_python_oracle':expected,
              'checksum_observables':['case index','tag','full-width tagged base writeback delta',
                  'x2/x3 result values','NZCV','q0..q3 bytes','entire 256-byte memory arena'],
              'all_gprs_checked_before_after_in_arm_fixture':list(range(31))+['SP'],
              'architecture_dynamic_inputs':['x18 platform register','x29 frame','x30 return','SP'],
              'kind_counts':dict(Counter(c[2] for c in cases)),
              'static_memory_and_writeback_ranges':ranges,
              'limits':['No compilation or execution result is asserted.',
                  'New nonpost LD1/LD4 positive cases are single subjects. No loop, fixed vector-cache residency or every-encoding coverage is asserted.',
                  'ARM instructions can be natively translated or fall back to the interpreter. Passing semantic checks alone proves neither native TBI coverage nor speed.',
                  'Latest parent prefix review reports non-post-index LD1/LD4 load fallback holes; ST4 is reachable through simd_more. This source-only preparation did not independently rerun that path measurement and does not modify ISA decoding.',
                  'Eight commpage_fix callsite families are represented by instruction subjects; actual candidate native coverage must be measured separately.',
                  'No faults, protected mappings, concurrent atomic races, all ARM encodings, all operand values, all vector registers, or full tag-by-NZCV Cartesian product are covered.',
                  'The native x86 product is a semantic C reference and does not execute ARM instructions or provide a translation performance benchmark.',
                  'The 29952 single-subject executions are bounded in design; runtime under 10 seconds remains unmeasured and requires the parent supervisor.']}
    (BASE / 'tagged-memory-structures-source.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps({'status':result['status'],'source_sha256':source_sha,'expected_stdout':expected}))


if __name__=='__main__':
    main()
