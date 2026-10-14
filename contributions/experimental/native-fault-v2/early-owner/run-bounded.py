#!/usr/bin/env python3
"""Parent-only bounded fresh-process constructor boundary cases."""
import argparse
import datetime
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import source_guard
ROOT=Path(__file__).resolve().parent
CASES=['baseline-observe','natural-early','prior-custom-segv','prior-custom-bus',
       'prior-ignore-segv','prior-ignore-bus','late-custom','late-owned-default','late-same-owner-reset']

def stop_and_drain(process):
    errors = []
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError as error:
        errors.append("process-group kill: " + type(error).__name__)
        try:
            process.kill()
        except ProcessLookupError:
            pass
        except OSError as kill_error:
            errors.append("direct kill: " + type(kill_error).__name__)
    try:
        stdout, stderr = process.communicate(timeout=2)
    except subprocess.TimeoutExpired as error:
        errors.append("pipe drain timeout")
        stdout = error.stdout or ""
        stderr = error.stderr or ""
        if isinstance(stdout, bytes):
            stdout = stdout.decode(errors="replace")
        if isinstance(stderr, bytes):
            stderr = stderr.decode(errors="replace")
        for pipe in (process.stdout, process.stderr):
            if pipe is not None:
                pipe.close()
        try:
            process.kill()
        except ProcessLookupError:
            pass
        except OSError as kill_error:
            errors.append("drain direct kill: " + type(kill_error).__name__)
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            errors.append("reap timeout")
    return stdout, stderr, errors

def run_case(build,name,timeout,output):
    started=time.monotonic();stdout,stderr='',''
    process=subprocess.Popen([str(build/('early-owner-'+name))],stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                             text=True,start_new_session=True,close_fds=True)
    timed_out=False;cleanup_errors=[]
    try:
        stdout,stderr=process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out=True;stdout,stderr,cleanup_errors=stop_and_drain(process)
    except BaseException as error:
        try:stdout,stderr,cleanup_errors=stop_and_drain(process)
        finally:
            (output/(name+'.stdout')).write_text(stdout);(output/(name+'.stderr')).write_text(stderr)
            abort={'case':name,'pass':False,'exception_type':type(error).__name__,'returncode':process.returncode,
                   'cleanup_errors':cleanup_errors,'elapsed_seconds':round(time.monotonic()-started,6)}
            (output/(name+'.supervisor-abort.json')).write_text(json.dumps(abort,indent=2)+'\n')
        raise
    (output/(name+'.stdout')).write_text(stdout);(output/(name+'.stderr')).write_text(stderr)
    observations=[]
    for line in stdout.splitlines():
        try:
            item=json.loads(line)
            if isinstance(item,dict):observations.append(item)
        except json.JSONDecodeError:pass
    final=[o for o in observations if o.get('case')==name and o.get('mode')=='early-owner' and 'pass' in o]
    passed=(not timed_out and not cleanup_errors and process.returncode==0 and len(final)==1
            and final[0]['pass'] is True and final[0].get('production_ready') is False
            and final[0].get('guest_resume') is False and final[0].get('performance_measured') is False)
    record={'case':name,'mode':'early-owner','pass':passed,'returncode':process.returncode,'timed_out':timed_out,
            'timeout_seconds':timeout,'elapsed_seconds':round(time.monotonic()-started,6),'observations':observations,
            'stdout':str(output/(name+'.stdout')),'stderr':str(output/(name+'.stderr')),
            'kind':'private-constructor-prior-and-visible-owner-boundary'}
    if cleanup_errors:record['supervisor_errors']=cleanup_errors
    print(('PASS ' if passed else 'FAIL ')+name,flush=True);return record

def evidence_snapshot(build):
    identity=source_guard.verify();path=build/'build-info.json';info=json.loads(path.read_text())
    for key in ('native_source_manifest_sha256','probe_source_manifest_sha256','public_package_manifest_sha256'):
        if info.get(key)!=identity[key]:raise RuntimeError('built/source mismatch: '+key)
    expected={'early-owner-'+case for case in CASES}
    if {i['name'] for i in info['binaries']}!=expected:raise RuntimeError('build cases mismatch')
    assets={}
    for item in info['binaries']+info['compiler_files']+info['inspection_files']:
        path=build/item['name'];digest=source_guard.sha256(path)
        if digest!=item['sha256']:raise RuntimeError('build asset mismatch: '+str(path))
        assets[item['name']]=digest
    return {'source_identity':identity,'build_info_sha256':source_guard.sha256(build/'build-info.json'),'assets':assets},info

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--timeout',type=float,default=20)
    parser.add_argument('--build',type=Path,default=ROOT/'build');parser.add_argument('--output',type=Path)
    parser.add_argument('--case',action='append');args=parser.parse_args()
    if not 0<args.timeout<=20:parser.error('--timeout must be >0 and <=20s')
    if args.case and set(args.case)-set(CASES):parser.error('unknown case')
    cases=[c for c in CASES if not args.case or c in args.case]
    before,info=evidence_snapshot(args.build)
    timestamp=datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
    output=args.output or ROOT/('results-'+timestamp);output.mkdir(parents=True,exist_ok=False)
    records=[];unchanged=False
    try:
        for name in cases:
            records.append(run_case(args.build,name,args.timeout,output))
            (output/'results.json').write_text(json.dumps(records,indent=2)+'\n')
    finally:
        try:
            after,_=evidence_snapshot(args.build);unchanged=after==before
            check={'before':before,'after':after,'unchanged':unchanged,
                   'scope':'start/end snapshots only; no claim of continuous immutability'}
        except Exception as error:
            check={'before':before,'unchanged':False,'end_verification_error':type(error).__name__+': '+str(error),
                   'scope':'start/end snapshots only; no claim of continuous immutability'}
        branches=[o for r in records for o in r['observations'] if o.get('constructor_finished_before_user_main') is True]
        coverage={'original_both_default_runs':sum(o['original_both_default'] for o in branches),
                  'installed_private_runs':sum(o['decision']==1 for o in branches),
                  'unsupported_prior_runs':sum(o['decision']==2 for o in branches),
                  'private_prior_fixtures_applied':sum(o['private_prior_fixture'] for o in branches),
                  'private_post_fixtures_applied':sum(o['private_post_fixture'] for o in branches),
                  'note':'An external prior prevents private fixture application; a PASS of its disabled branch is not intended-fixture coverage.'}
        summary={'scope':'private constructor, unknown-prior refusal and visible-owner startup boundary only',
                 'source_identity':before['source_identity'],'build_identity':info,'evidence_identity_check':check,
                 'selected_cases':len(cases),'completed_cases':len(records),'passed_cases':sum(r['pass'] for r in records),
                 'failed_cases':[r['case'] for r in records if not r['pass']],
                 'recorded_branch_coverage':coverage,
                 'production_ready':False,'unknown_prior_reset':False,'native_primitive_called':False,
                 'stackoverflow_triggered':False,'guest_resume_implemented':False,'performance_measured':False,
                 '95_percent_target_achieved':False}
        (output/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');print('Results: '+str(output),flush=True)
    return 0 if unchanged and len(records)==len(cases) and all(r['pass'] for r in records) else 1
if __name__=='__main__':raise SystemExit(main())
