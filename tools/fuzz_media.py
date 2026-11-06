#!/usr/bin/env python3
"""Run a pinned, deterministic JSON-worker mutation campaign; never qualification."""
from collections import Counter
from pathlib import Path
import argparse
import json
import platform
import random
import re
import shutil
import sys

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT))
from harness.common import digest as sha, digest_bytes, save_json
from harness.fuzz import compare, json_decimal, mutation, observe, strict_json


def resolve(path):
    candidate=Path(path)
    return candidate.resolve() if candidate.is_absolute() else (ROOT/candidate).resolve()


def write(path,data):
    save_json(path,data,default=json_decimal,allow_nan=False)


def label(value):
    return isinstance(value,str) and re.fullmatch(r'[a-z][a-z0-9_-]{0,63}',value) is not None


def input_identity(path,expected,expected_bytes,maximum):
    try:
        size=path.stat().st_size
        if size>maximum:
            return {'matches':False,'bytes':size,'sha256':None,'error':'input-size-limit'}
        actual=sha(path)
        return {'matches':actual==expected and size==expected_bytes,'bytes':size,'sha256':actual}
    except OSError as error:
        return {'matches':False,'error':type(error).__name__,'sha256':None}


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--rounds',type=int,default=4,help='mutations per seed, plus one unmodified baseline')
    parser.add_argument('--seed',type=int,default=280)
    parser.add_argument('--maximum-input-bytes',type=int,default=4*1024*1024)
    parser.add_argument('--maximum-output-bytes',type=int,default=8*1024*1024)
    parser.add_argument('--timeout-seconds',type=float,default=3)
    args=parser.parse_args()
    if not 1<=args.rounds<=10000 or not 1<=args.maximum_input_bytes<=64*1024*1024:
        parser.error('invalid finite mutation bounds')
    if not 1<=args.maximum_output_bytes<=64*1024*1024 or not 0<args.timeout_seconds<=60:
        parser.error('invalid finite observation bounds')
    output=args.output.resolve()
    if not output.is_relative_to(ROOT/'.research') or output.exists():
        parser.error('output must be a new directory under .research')
    manifest_bytes=args.manifest.read_bytes()
    manifest_digest=digest_bytes(manifest_bytes)
    manifest=strict_json(manifest_bytes)
    if not isinstance(manifest,dict) or type(manifest.get('schema')) is not int or manifest['schema']!=1:
        parser.error('unknown mutation manifest schema')
    workers,operations,seeds=manifest['workers'],manifest['operations'],manifest['seeds']
    if not workers or not operations or not seeds:
        parser.error('workers, operations and seeds must be nonempty')
    if len({s['id'] for s in seeds})!=len(seeds) or len({o['id'] for o in operations})!=len(operations):
        parser.error('duplicate seed or operation identities')
    if len(workers)>8 or len(operations)>16 or not all(label(role) for role in workers):
        parser.error('bounded worker/operation counts and safe role labels are required')
    if not all(label(operation['id']) for operation in operations):
        parser.error('operation IDs must be simple bounded labels')
    worker_paths={role:resolve(w['path']) for role,w in workers.items()}
    for role,path in worker_paths.items():
        if sha(path)!=workers[role]['sha256']:parser.error('worker hash mismatch: '+role)
    sources=manifest.get('source_files',{})
    if not sources:parser.error('explicit source/input-tool provenance hashes are required')
    for path,digest in sources.items():
        if sha(resolve(path))!=digest:parser.error('source provenance mismatch: '+path)
    seed_bytes={}
    for row in seeds:
        data=resolve(row['path']).read_bytes()
        if digest_bytes(data)!=row['sha256'] or len(data)>args.maximum_input_bytes:
            parser.error('seed hash or input-size mismatch: '+row['id'])
        if not row.get('rights'):parser.error('seed rights/provenance must be recorded: '+row['id'])
        expected=row.get('baseline_statuses',{})
        if set(expected)!={operation['id'] for operation in operations}:
            parser.error('every seed must declare each baseline operation')
        for operation in operations:
            statuses=expected[operation['id']]
            if set(statuses)!=set(operation['roles']) or any(v not in ('ok','rejected') for v in statuses.values()):
                parser.error('every baseline worker outcome must be declared')
        seed_bytes[row['id']]=data
    for operation in operations:
        roles=operation['roles']
        if 'candidate' not in roles or len(roles)<2 or not set(roles)<=set(workers):
            parser.error('operation needs candidate and at least one pinned comparator')
        for command in roles.values():
            if not isinstance(command,list) or not all(isinstance(v,str) for v in command) or command.count('{input}')!=1:
                parser.error('worker arguments need exactly one literal {input} slot')
    output.mkdir(parents=True)
    (output/'manifest.json').write_bytes(manifest_bytes)
    controller={str(p.relative_to(ROOT)):sha(p) for p in
                [Path(__file__).resolve(),ROOT/'harness/common.py',ROOT/'harness/fuzz.py',ROOT/'harness/runner.py']}
    for relative,digest in controller.items():
        source=ROOT/relative
        destination=output/'controller-source'/relative
        destination.parent.mkdir(parents=True,exist_ok=True)
        shutil.copy2(source,destination)
        if sha(destination)!=digest:raise ValueError('controller changed during preservation: '+relative)
    identity={'manifest_sha256':manifest_digest,'controller_sha256':controller,'workers':workers,
              'source_files':sources,'python':sys.version,'platform':platform.platform(),'machine':platform.machine(),
              'parameters':vars(args)|{'manifest':str(args.manifest),'output':str(output)},
              'qualification':False,'time_or_memory_measurement':False}
    write(output/'identity.json',identity)
    if sha(args.manifest)!=manifest_digest:
        raise ValueError('campaign manifest changed after its initial read')
    rng=random.Random(args.seed)
    counts,finding_counts=Counter(),Counter()
    cases,finding_total,worker_executions=0,0,0
    case_inputs_unchanged=True
    index=output/'findings.jsonl'
    index.write_bytes(b'')
    for row in seeds:
        original=seed_bytes[row['id']]
        for round_number in range(args.rounds+1):
            data,recipe=(original,None) if round_number==0 else mutation(original,rng,args.maximum_input_bytes)
            case=output/('case-'+str(cases).zfill(6));case.mkdir()
            path=case/'input.bin';path.write_bytes(data)
            (case/'initial-input.bin').write_bytes(data)
            result={'ordinal':cases,'seed_id':row['id'],'seed_sha256':row['sha256'],'rights':row['rights'],
                    'round':round_number,'input_sha256':digest_bytes(data),'recipe':recipe,'operations':[],'qualification':False}
            for operation in operations:
                observed={}
                directory=case/operation['id']
                # Operation IDs are labels, never allowed to escape this case.
                if not directory.resolve().is_relative_to(case):raise ValueError('operation ID escapes output')
                directory.mkdir()
                for role,arguments in operation['roles'].items():
                    command=[str(worker_paths[role]),*[str(path) if a=='{input}' else a for a in arguments]]
                    before=input_identity(path,result['input_sha256'],len(data),args.maximum_input_bytes)
                    if before['matches']:
                        record,stdout,stderr=observe(command,args.timeout_seconds,args.maximum_output_bytes)
                        worker_executions+=1
                    else:
                        stdout,stderr=b'',b''
                        record={'status':'input-identity-changed','returncode':None,'command':command,
                                'stdout_sha256':digest_bytes(stdout),'stderr_sha256':digest_bytes(stderr),'skipped':True}
                    after=input_identity(path,result['input_sha256'],len(data),args.maximum_input_bytes)
                    record['input_before'],record['input_after']=before,after
                    if not before['matches'] or not after['matches']:
                        case_inputs_unchanged=False
                        if record['status']!='input-identity-changed':
                            record['worker_status']=record['status']
                            record['status']='input-identity-changed'
                    (directory/(role+'.stdout')).write_bytes(stdout)
                    (directory/(role+'.stderr')).write_bytes(stderr)
                    observed[role]=record;counts[role+':'+record['status']]+=1
                comparison=compare(observed)
                if round_number==0:
                    for role,status in row['baseline_statuses'][operation['id']].items():
                        if observed[role]['status']!=status:
                            comparison['findings'].append({'kind':'baseline-status-changed','role':role,
                                'expected':status,'actual':observed[role]['status']})
                # Raw streams remain authoritative; avoid retaining every parsed
                # packet/metadata value across operations or the whole campaign.
                for record in observed.values():record.pop('data',None)
                entry={'id':operation['id'],'workers':observed,**comparison}
                result['operations'].append(entry)
                for finding in comparison['findings']:
                    finding_counts[finding['kind']]+=1;finding_total+=1
                    with index.open('a') as stream:
                        stream.write(json.dumps({'case':case.name,'operation':operation['id'],**finding},default=json_decimal,allow_nan=False)+'\n')
            result['final_input_identity']=input_identity(path,result['input_sha256'],len(data),args.maximum_input_bytes)
            case_inputs_unchanged &= result['final_input_identity']['matches']
            write(case/'observation.json',result);cases+=1
            write(output/'progress.json',{'completed_cases':cases,'worker_outcomes':dict(counts),
                  'findings_count':finding_total,'findings_by_kind':dict(finding_counts),
                  'collection_status':'partial','qualification':False})
        print(json.dumps(row['id']),args.rounds+1,'original/mutated inputs retained',flush=True)
    for role,path in worker_paths.items():
        if sha(path)!=workers[role]['sha256']:raise ValueError('worker changed during campaign: '+role)
    for path,digest in sources.items():
        if sha(resolve(path))!=digest:raise ValueError('source provenance changed during campaign: '+path)
    for row in seeds:
        if sha(resolve(row['path']))!=row['sha256']:raise ValueError('seed changed during campaign: '+row['id'])
    for path,digest in controller.items():
        if sha(ROOT/path)!=digest:raise ValueError('controller changed during campaign: '+path)
    if sha(args.manifest)!=identity['manifest_sha256']:raise ValueError('campaign manifest changed during execution')
    report={'collection_status':'complete' if case_inputs_unchanged else 'input-identity-failed','seeds':len(seeds),'operations':len(operations),'cases':cases,
            'worker_outcomes':dict(counts),'findings_count':finding_total,'findings_by_kind':dict(finding_counts),
            'worker_executions':worker_executions,'case_inputs_unchanged':case_inputs_unchanged,
            'findings_index':{'path':'findings.jsonl','sha256':sha(index)},'source_workers_and_seeds_unchanged':True,
            'qualification':False,'time_or_memory_measurement':False,
            'rejections_are_ungraded':True,'scope':'wire mutation/differential observations; stateful scheduling remains separate'}
    write(output/'report.json',report)
    print(json.dumps(report,indent=2))
    return int(bool(finding_total) or not case_inputs_unchanged)


if __name__=='__main__':raise SystemExit(main())
