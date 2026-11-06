#!/usr/bin/env python3
"""Render the D4 per-workload TIME/MEM outcome table from the first-clean ledgers.

usage: report_tables.py WORKLOADS.json TIME-LEDGER.json MEM-LEDGER.json LABEL [LABEL...]
Every ledger record of each label is shown; nothing is filtered or re-derived.
"""
import json, sys

workloads = [w['id'] for w in json.load(open(sys.argv[1]))['workloads']]
time_rows = list(json.load(open(sys.argv[2])).values())
mem_rows = list(json.load(open(sys.argv[3])).values())
f = lambda v: '' if v is None else f'{v:.3f}'
for label in sys.argv[4:]:
    t = {r['id']: r for r in time_rows if r.get('label') == label}
    m = {r['case']: r for r in mem_rows if r.get('label') == label}
    print(f'\n### {label}\n')
    print('| workload | TIME upper95 | median | delivery <=1.25 | target <=0.80 | digest equal | heap upper95 | RSS upper95 | MEM pass | all gates |')
    print('|---|---|---|---|---|---|---|---|---|---|')
    counts = dict(time=0, opt=0, mem=0, all=0)
    for w in workloads:
        tr, mr = t.get(w, {}), m.get(w, {})
        ratio = tr.get('ratio') or {}
        delivery, mem_pass = tr.get('delivery'), bool(mr.get('heap_pass') and mr.get('rss_pass'))
        opt = ratio.get('upper_95') is not None and ratio['upper_95'] <= 0.80
        both = bool(delivery) and mem_pass
        counts['time'] += bool(delivery); counts['opt'] += opt; counts['mem'] += mem_pass; counts['all'] += both
        print(f"| {w} | {f(ratio.get('upper_95'))} | {f(ratio.get('median'))} | {delivery} | {opt} | {tr.get('digest_equal')} | "
              f"{f(mr.get('heap_upper95'))} | {f(mr.get('rss_upper95'))} | {mem_pass if mr else ''} | {'PASS' if both else 'FAIL'} |")
    print(f"\n{label}: TIME delivery {counts['time']}/{len(workloads)}, target {counts['opt']}/{len(workloads)}, "
          f"MEM {counts['mem']}/{len(workloads)}, all gates {counts['all']}/{len(workloads)}")
