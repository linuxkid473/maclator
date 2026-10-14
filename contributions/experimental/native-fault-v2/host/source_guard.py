#!/usr/bin/env python3
"""Public package source identity only. No historical logs, build or runtime invocation."""
import argparse, hashlib, json
from pathlib import Path
ROOT=Path(__file__).resolve().parent
PACKAGE=ROOT.parent
def sha256(path):return hashlib.sha256(path.read_bytes()).hexdigest()
def verify():
    manifest=PACKAGE/'MANIFEST.json'
    entries=json.loads(manifest.read_text())
    for item in entries:
        relative=Path(item['path'])
        if relative.is_absolute() or '..' in relative.parts:raise RuntimeError('invalid manifest path')
        if sha256(PACKAGE/relative)!=item['sha256']:raise RuntimeError('public source changed: '+str(relative))
    native=[item for item in entries if item['path'].startswith('native-fault/') or item['path']=='native_fault.rs']
    own=[item for item in entries if item['path'].startswith(ROOT.name+'/')]
    digest=lambda value:hashlib.sha256(json.dumps(value,sort_keys=True).encode()).hexdigest()
    return {'native_source_manifest_sha256':digest(native),'probe_source_manifest_sha256':digest(own),
            'public_package_manifest_sha256':sha256(manifest),'scope':'public relocated source identity; historical artifacts excluded'}
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--verify',action='store_true');parser.add_argument('--build-info',type=Path);parser.add_argument('--public-only',action='store_true')
    args=parser.parse_args();record=verify()
    if args.build_info:
        if ROOT.name=='host':
            names=['host-fault-probe'] if args.public_only else ['host-fault-probe','internal-rejection-probe','install-fault-probe']
        elif ROOT.name=='rust-adapter':names=['adapter-direct-probe','adapter-observed-probe']
        else:names=['early-owner-'+case for case in ['baseline-observe','natural-early','prior-custom-segv','prior-custom-bus','prior-ignore-segv','prior-ignore-bus','late-custom','late-owned-default','late-same-owner-reset']]
        record['binaries']=[{'name':n,'sha256':sha256(args.build_info/n)} for n in names]
        if ROOT.name!='host':record['compiler_files']=[{'name':n,'sha256':sha256(args.build_info/n),'text':(args.build_info/n).read_text()} for n in ['rustc-version.txt','clang-version.txt']]
        if ROOT.name=='early-owner':
            version=(args.build_info/'rustc-version.txt').read_text()
            if not version.startswith('rustc 1.99.0 ') or 'host: x86_64-apple-darwin' not in version:raise RuntimeError('early-owner assertions require reviewed Rust 1.99.0 Darwin toolchain')
            inspections=[]
            for name in names:
                case=name.removeprefix('early-owner-');loads=(args.build_info/(case+'.macho-loads.txt')).read_text()
                if 'LC_MAIN' not in loads or not ('__mod_init_func' in loads or '__init_offsets' in loads):raise RuntimeError('unsupported initializer metadata')
                inspections += [{'name':case+suffix,'sha256':sha256(args.build_info/(case+suffix))} for suffix in ['.macho-loads.txt','.bridge-disassembly.txt','.undefined.txt']]
            record['inspection_files']=inspections
        (args.build_info/'build-info.json').write_text(json.dumps(record,indent=2)+'\n')
    print(json.dumps(record,indent=2))
if __name__=='__main__':main()
