#!/usr/bin/env python3
"""D4 paired TIME collection (research tooling).

The frozen H0 primitives (harness/runner.py: execute, cpu_snapshot/active_cpu,
competing_builds, ratio_interval, benchmark lock, module-absence guard) with the
same 30 randomised pairs and seed 280. Equal work for decoders means equal output
picture/block counts and byte counts; the output digest is recorded for both sides
(equal only where the outputs are bit-exact, which the correctness gate grades
separately). Each claim key is measured once; results stay first-clean.
"""
import argparse, hashlib, json, random, sys, time
from datetime import datetime, timezone
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('--h0-root', type=Path, required=True)
p.add_argument('--candidate', type=Path, required=True)
p.add_argument('--reference', type=Path, required=True)
p.add_argument('--workloads', type=Path, required=True)
p.add_argument('--root', type=Path, required=True, help='directory the workload paths are relative to')
p.add_argument('--ledger', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
p.add_argument('--label', required=True)
p.add_argument('--only', default=None)
p.add_argument('--recollect-contaminated', default=None,
               help='label whose contaminated claims are recollected once on seed 281 (declared policy)')
a = p.parse_args()
sys.path.insert(0, str(a.h0_root.resolve()))
from harness import runner  # noqa: E402
assert hashlib.sha256((a.h0_root / 'harness/runner.py').read_bytes()).hexdigest() == \
    '3bbbfc5daf4466afcd588831f2142cd6e5f6c9cd6d008a81273dd594791a9dfe'
sha = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
PAIRS, SEED = 30, 280
workloads = json.loads(a.workloads.read_text())['workloads']
ledger = json.loads(a.ledger.read_text()) if a.ledger.exists() else {}
a.out.mkdir(parents=True, exist_ok=True)
workers = {'candidate': str(a.candidate), 'reference': str(a.reference)}
hashes = {k: sha(v) for k, v in workers.items()}
assert runner.timing_module_state()['status'] in ('module-absent', 'not-applicable')
for w in workloads:
    if a.only and w['id'] not in a.only.split(','):
        continue
    path = a.root / w['path']
    assert sha(path) == w['sha256'], w['id']
    key = hashlib.sha256(json.dumps(dict(workers=hashes, input=w['sha256'], operation=w['operation'],
                                         iterations=w['iterations'], kind='d4-time'), sort_keys=True).encode()).hexdigest()
    previous, seed = None, SEED
    if key in ledger:
        old = ledger[key]
        if not (a.recollect_contaminated and old.get('label') == a.recollect_contaminated
                and old.get('status') == 'contaminated' and 'contaminated_previous' not in old):
            print('SKIP', w['id'], flush=True)
            continue
        previous, seed = dict(old), SEED + 1
    ledger[key] = dict(label=a.label, id=w['id'], status='claimed', seed=seed, utc=datetime.now(timezone.utc).isoformat())
    if previous is not None:
        ledger[key]['contaminated_previous'] = previous
    a.ledger.write_text(json.dumps(ledger, indent=1, sort_keys=True) + '\n')
    rng = random.Random(seed)
    samples, status = [], 'measured'
    with runner.benchmark_lock():
        if busy := runner.competing_builds():
            status = 'contaminated'
        else:
            for name, worker in workers.items():
                warm = runner.execute([worker, w['operation'] + '-bench', str(path), w['iterations']], timeout=600)
                if warm['status'] != 'ok':
                    status = 'warmup-failed:' + name
            for index in range(PAIRS if status == 'measured' else 0):
                order = list(workers); rng.shuffle(order)
                before = runner.cpu_snapshot()
                row = dict(pair=index, order=order, cpu_before=before)
                for name in order:
                    row[name] = runner.execute([workers[name], w['operation'] + '-bench', str(path), w['iterations']], timeout=600)
                row['cpu_after'] = runner.cpu_snapshot()
                samples.append(row)
                if runner.active_cpu(before, row['cpu_after']) or runner.timing_module_state()['status'] == 'module-loaded':
                    status = 'contaminated'; break
    result = dict(id=w['id'], key=key, label=a.label, seed=seed, workers=hashes, input_sha256=w['sha256'], operation=w['operation'],
                  iterations=w['iterations'], status=status, samples=samples)
    if status == 'measured':
        if any(r[k]['status'] != 'ok' for r in samples for k in workers):
            result['status'] = 'worker-failure'
        else:
            work = {(r['candidate']['data']['packets'], r['candidate']['data']['bytes'],
                     r['reference']['data']['packets'], r['reference']['data']['bytes']) for r in samples}
            c_pk, c_by, r_pk, r_by = next(iter(work))
            result['equal_work'] = len(work) == 1 and c_pk == r_pk and c_by == r_by
            result['digest_equal'] = all(r['candidate']['data']['digest'] == r['reference']['data']['digest'] for r in samples)
            result['work'] = dict(candidate=[c_pk, c_by], reference=[r_pk, r_by])
            if result['equal_work']:
                result['ratio'] = runner.ratio_interval([(r['candidate']['data']['elapsed_ns'], r['reference']['data']['elapsed_ns']) for r in samples], seed)
                result['delivery'] = result['ratio']['upper_95'] <= 1.25
                result['optimization'] = result['ratio']['upper_95'] <= 0.80
            else:
                result['status'] = 'unequal-work'
    raw = a.out / (w['id'].replace('/', '__') + ('' if seed == SEED else '-seed%d' % seed) + '.json')
    assert not raw.exists(), raw
    raw.write_text(json.dumps(result, indent=1) + '\n')
    ledger[key].update(status=result['status'], raw=str(raw), raw_sha256=sha(raw), ratio=result.get('ratio'),
                       delivery=result.get('delivery'), digest_equal=result.get('digest_equal'))
    a.ledger.write_text(json.dumps(ledger, indent=1, sort_keys=True) + '\n')
    print('D4TIME', w['id'], result['status'], result.get('ratio', {}).get('upper_95'), result.get('delivery'),
          'digest_equal' if result.get('digest_equal') else '', flush=True)
print('DONE')
