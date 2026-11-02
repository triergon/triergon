#!/usr/bin/env python3
"""Independent exact mathematical oracle. Standard library only, no Rust execution.

Expected rational answers come from fractions.Fraction, not a port/model of Time
or Wide. Arbitrary-precision integer operators supply Wide primitive answers.
Source files are hashed for provenance only, never parsed to calculate answers.
"""
from pathlib import Path
from fractions import Fraction
from collections import Counter
import datetime
import hashlib
import json
import math
import platform
import random
import sys
import fractions
import argparse

REPO = Path(__file__).resolve().parents[1]
ROOT = REPO / ".research/core-time-oracle"
I128_MIN, I128_MAX = -(1 << 127), (1 << 127) - 1
I64_MIN, I64_MAX = -(1 << 63), (1 << 63) - 1
U64_MAX, U128_MAX, U256_MAX = (1 << 64) - 1, (1 << 128) - 1, (1 << 256) - 1
MODES = ('TowardZero', 'AwayFromZero', 'Floor', 'Ceil', 'NearestAway')
SEED = 0x54494D455F4F5241434C455F31
RNG = random.Random(SEED)
CASES = {'time': {}, 'wide': {}}
FOCUS = {}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def json_file(name, data):
    (ROOT / name).write_text(json.dumps(data, indent=2, ensure_ascii=False) + '\n')


def rational_result(value):
    if not (I128_MIN <= value.numerator <= I128_MAX and 1 <= value.denominator <= U128_MAX):
        return ('overflow',)
    return ('ok', str(value.numerator), str(value.denominator))


def int_result(value, bits):
    low, high = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
    return ('ok', str(value)) if low <= value <= high else ('overflow',)


def round_exact(value, mode):
    # Definitions on an exact Fraction; never a float and never Python round()
    # (which would impose a different tie rule).
    if mode == 'TowardZero':
        answer = math.trunc(value)
    elif mode == 'AwayFromZero':
        answer = math.ceil(value) if value >= 0 else math.floor(value)
    elif mode == 'Floor':
        answer = math.floor(value)
    elif mode == 'Ceil':
        answer = math.ceil(value)
    elif mode == 'NearestAway':
        answer = math.floor(value + Fraction(1, 2)) if value >= 0 else math.ceil(value - Fraction(1, 2))
    else:
        raise ValueError(mode)
    if mode == 'Floor':
        assert answer <= value < answer + 1
    elif mode == 'Ceil':
        assert answer - 1 < value <= answer
    elif mode == 'NearestAway':
        distance = abs(value - answer)
        assert distance <= Fraction(1, 2)
        if distance == Fraction(1, 2):
            assert abs(answer) > abs(value)
    return answer


def ordering(a, b):
    return 'less' if a < b else 'greater' if a > b else 'equal'


def wide_text(number):
    assert 0 <= number <= U256_MAX
    # Output encoding only, not limb arithmetic: expected numeric answers have
    # already been computed using Python's arbitrary-precision operators.
    return 'Wide([' + ', '.join(str((number >> shift) & U64_MAX) for shift in (0, 64, 128, 192)) + '])'


def time_answer(op, args):
    if op == 'new':
        n, d = map(int, args)
        return ('invalid',) if d == 0 else rational_result(Fraction(n, d))
    if op == 'base':
        n, d = map(int, args)
        if n == 0 or d == 0:
            return ('invalid',)
        f = Fraction(n, d)
        return ('ok', str(f.numerator), str(f.denominator))
    if op == 'from_ticks':
        ticks, bn, bd = map(int, args)
        return ('invalid',) if bn == 0 or bd == 0 else rational_result(ticks * Fraction(bn, bd))
    if op in ('add', 'sub', 'cmp', 'eq'):
        an, ad, bn, bd = map(int, args)
        if ad == 0 or bd == 0:
            return ('invalid',)
        a, b = Fraction(an, ad), Fraction(bn, bd)
        if op == 'add':
            return rational_result(a + b)
        if op == 'sub':
            return rational_result(a - b)
        if op == 'eq':
            return ('ok', str(int(a == b)))
        return ('ok', ordering(a, b))
    if op in ('ticks128', 'ticks64'):
        n, d, bn, bd = map(int, args[:4])
        if d == 0 or bn == 0 or bd == 0:
            return ('invalid',)
        value = Fraction(n, d) / Fraction(bn, bd)
        return int_result(round_exact(value, args[4]), 128 if op == 'ticks128' else 64)
    if op == 'ns64':
        n, d = map(int, args[:2])
        if d == 0:
            return ('invalid',)
        return int_result(round_exact(Fraction(n, d) * 1_000_000_000, args[2]), 64)
    if op == 'rescale64':
        ticks, sn, sd, dn, dd = map(int, args[:5])
        if 0 in (sn, sd, dn, dd):
            return ('invalid',)
        return int_result(round_exact(ticks * Fraction(sn, sd) / Fraction(dn, dd), args[5]), 64)
    if op == 'sort':
        values = [Fraction(s) for s in args[0].split(';')]
        return ('ok', ','.join(str(i) for i in sorted(range(len(values)), key=values.__getitem__)))
    raise ValueError(op)


def wide_answer(op, args):
    ns = list(map(int, args))
    if op == 'w_product':
        return ('ok', wide_text(ns[0] * ns[1]))
    a = ns[0] + (ns[1] << 128)
    if op == 'w_build':
        return ('ok', wide_text(a))
    if op == 'w_to128':
        return ('ok', str(a)) if a <= U128_MAX else ('overflow',)
    b = ns[2] + (ns[3] << 128)
    if op == 'w_add':
        return ('ok', wide_text(a + b)) if a + b <= U256_MAX else ('overflow',)
    if op == 'w_sub':
        assert a >= b
        return ('ok', wide_text(a - b))
    if op == 'w_cmp':
        return ('ok', ordering(a, b))
    if op == 'w_div':
        assert b > 0
        q, r = divmod(a, b)
        assert q * b + r == a and 0 <= r < b
        return ('ok', wide_text(q), wide_text(r))
    if op == 'w_gcd':
        g = math.gcd(a, b)
        assert g == 0 or (a % g == 0 and b % g == 0)
        return ('ok', wide_text(g))
    raise ValueError(op)


def add(suite, family, op, *args, focus=None):
    args = tuple(str(a) for a in args)
    key = (op, *args)
    if key not in CASES[suite]:
        ident = op + '-' + sha(('\t'.join(key)).encode())[:20]
        expected = time_answer(op, args) if suite == 'time' else wide_answer(op, args)
        CASES[suite][key] = dict(id=ident, op=op, args=args, expected=expected, families=set())
    row = CASES[suite][key]
    row['families'].add(family)
    if focus:
        FOCUS[focus] = row['id']
    return row


def signed_random(bits=128):
    n = RNG.getrandbits(bits)
    return n - (1 << bits) if n >= 1 << (bits - 1) else n


def random_rat():
    return signed_random(), RNG.getrandbits(128) or 1


def split_wide(value):
    assert 0 <= value <= U256_MAX
    return value & U128_MAX, value >> 128


def wide_case(family, op, a, b=None, focus=None):
    # Every non-product operand has a separately checked construction case. A
    # defective construction helper must be visible before dependent operations
    # can be regarded as evidence.
    for value in (a,) if b is None else (a, b):
        add('wide', 'operand-construction', 'w_build', *split_wide(value))
    return add('wide', family, op, *split_wide(a), *(() if b is None else split_wide(b)), focus=focus)


def corpus():
    signed_edges = [I128_MIN, I128_MIN+1, -(1<<126), -(1<<64), I64_MIN, -3, -2, -1, 0, 1, 2, 3,
                    I64_MAX, (1<<64)-1, 1<<64, 1<<126, I128_MAX-1, I128_MAX]
    den_edges = [0, 1, 2, 3, 5, (1<<63)-1, 1<<63, U64_MAX, 1<<64, (1<<64)+1,
                 (1<<126)-1, 1<<126, (1<<127)-1, 1<<127, (1<<127)+1, U128_MAX-2, U128_MAX-1, U128_MAX]
    for n in signed_edges:
        for d in den_edges:
            add('time', 'constructor-boundaries', 'new', n, d)
    for bit in range(128):
        for d in sorted({max(1,(1<<bit)-1),1<<bit,min(U128_MAX,(1<<bit)+1)}):
            for n in [0, 1, -1, I128_MIN, I128_MAX]:
                add('time', 'every-denominator-bit', 'new', n, d)
        for n in [-(1<<bit), (1<<bit) if bit < 127 else I128_MAX]:
            for d in [1, 3, U128_MAX]:
                add('time', 'every-numerator-bit', 'new', n, d)
    for _ in range(300):
        add('time', 'random-full-width-constructor', 'new', *random_rat())

    bases = [(1,1),(1,3),(1,90_000),(1,1_000_000_000),(1001,30_000),(48_000,1001),
             (U64_MAX,1),(1,U64_MAX),(U64_MAX,U64_MAX-2),(U64_MAX-2,U64_MAX),
             (1<<63,U64_MAX),(U64_MAX,1<<63),(U64_MAX,U64_MAX)]
    base_edges = [0,1,2,3,1<<31,(1<<32)-1,1<<63,U64_MAX-1,U64_MAX]
    for n in base_edges:
        for d in base_edges:
            add('time','timebase-boundaries','base',n,d)
    for ticks in signed_edges:
        for bn,bd in bases:
            add('time','from-ticks-boundaries','from_ticks',ticks,bn,bd)
    for _ in range(700):
        add('time','from-ticks-full-width','from_ticks',signed_random(),RNG.getrandbits(64) or 1,RNG.getrandbits(64) or 1)
    for ticks in [I128_MIN,0,I128_MAX]:
        for bn,bd in [(0,1),(1,0),(0,0)]:
            add('time','invalid-timebase','from_ticks',ticks,bn,bd)

    pool = [(n,d) for n in [I128_MIN,I128_MIN+1,-1,0,1,I128_MAX-1,I128_MAX]
            for d in [1,2,1<<127,U128_MAX]]
    pool += [(I128_MAX, U128_MAX-1),(-I128_MAX,U128_MAX-1),(1,2),(2,4),(0,U128_MAX)]
    for a in pool:
        for b in pool:
            for op in ('add','sub','cmp'):
                add('time','pair-boundaries',op,*a,*b)
    for _ in range(750):
        a,b=random_rat(),random_rat()
        for op in ('add','sub','cmp'):
            add('time','random-full-width-pairs',op,*a,*b)
    for a in pool:
        for op in ('add','sub','eq'):
            add('time','same-value-identity',op,*a,*a)
        if a[0] != I128_MIN:
            add('time','opposite-value-cancellation','add',*a,-a[0],a[1])
    add('time','focused-reduction','add',I128_MAX,1<<127,I128_MAX,1<<127,focus='sum-over-i128-reduces-to-fit')
    add('time','focused-reduction','sub',I128_MIN,U128_MAX,I128_MIN,U128_MAX,focus='minimum-minus-itself-is-zero')
    add('time','focused-overflow','sub',0,1,I128_MIN,1,focus='zero-minus-minimum-overflows')
    add('time','focused-overflow','add',I128_MIN,1,-1,1,focus='minimum-minus-one-overflows')

    # Independent modular construction: choose fractions with shared denominator
    # factor g so their sum cancels g, despite a >128-bit least-common denominator.
    # Fraction calculates every expected answer; this is input generation only.
    accepted=0
    while accepted < 220:
        g=1<<RNG.choice([63,64,65,96])
        maximum=U128_MAX//g
        width=maximum.bit_length()-1
        u=(RNG.getrandbits(width)|(1<<(width-1))|1)
        v=(RNG.getrandbits(width)|(1<<(width-1))|1)
        if u==v or math.gcd(u,v)!=1:
            continue
        b=(-v*pow(u,-1,g))%g
        if b==0 or math.gcd(b,v)!=1:
            continue
        left,right=(1,g*u),(b,g*v)
        result=Fraction(*left)+Fraction(*right)
        if rational_result(result)[0]!='ok' or math.lcm(left[1],right[1]).bit_length()<=128:
            continue
        for sign in [1,-1]:
            add('time','wide-denominator-reduces-to-fit','add',sign,*left[1:],sign*b,right[1],
                focus='wide-denominator-reduction-example' if accepted==0 and sign==1 else None)
            add('time','wide-denominator-reduces-to-fit','sub',sign,*left[1:],-sign*b,right[1])
        accepted+=1

    # Nearly cancelling full-width products: neighboring fractions with opposite
    # signs, and unreduced argument variants, are intentional distinct inputs.
    for _ in range(250):
        n,d=random_rat()
        for op in ('add','sub'):
            other=n+1 if n<I128_MAX else n-1
            add('time','near-cancellation',op,n,d,other,d)
    for a,b in [((1,U128_MAX),(1,U128_MAX-1)),((I128_MIN,3),(I128_MIN,3))]:
        add('time','focused-overflow','add',*a,*b,focus='denominator-overflow' if a[0]==1 else 'negative-numerator-overflow')

    rounding_pool=[(n,d) for n in [I128_MIN,I128_MIN+1,-3,-1,0,1,3,I128_MAX-1,I128_MAX]
                   for d in [1,2,3,1<<127,U128_MAX]]
    rounding_pool += [(1,3),(-1,3),(I128_MAX,U128_MAX-3),(-I128_MAX,U128_MAX-3)]
    for n,d in rounding_pool:
        for bn,bd in bases:
            for mode in MODES:
                for op in ('ticks128','ticks64'):
                    add('time','rounding-boundaries',op,n,d,bn,bd,mode)
        for mode in MODES:
            add('time','nanosecond-boundaries','ns64',n,d,mode)
    # Exact half-neighbors at each target width and both signs. The 128-bit
    # negative outer half uses a factorization checked as an integer identity.
    factor=59_649_589_127_497_217
    complement=5_704_689_200_685_129_054_721
    assert factor*complement==(1<<128)+1 and factor<=U64_MAX and complement<=I128_MAX
    for op,bits in [('ticks64',64),('ticks128',128)]:
        limit=(1<<(bits-1))-1
        if bits==64:
            edge_values=[(2*limit-1,2,1,1),(2*limit+1,2,1,1),
                         (-(1<<bits)+1,2,1,1),(-(1<<bits)-1,2,1,1)]
        else:
            edge_values=[(U128_MAX//3,2,1,3),(-U128_MAX//3,2,1,3),
                         (-complement,2,1,factor),(complement,2,1,factor)]
        for n,d,bn,bd in edge_values:
            for mode in MODES:
                add('time','rounding-at-signed-output-limits',op,n,d,bn,bd,mode,
                    focus=f'{bits}-bit-edge-{n<0}-{mode}-{bd}')
    for _ in range(250):
        n,d=random_rat()
        for bn,bd in [(1,1),(1,90_000),(U64_MAX,U64_MAX-2),
                      (RNG.getrandbits(64) or 1,RNG.getrandbits(64) or 1)]:
            for mode in MODES:
                for op in ('ticks128','ticks64'):
                    add('time','random-full-width-rescale',op,n,d,bn,bd,mode)
        for mode in MODES:
            add('time','random-full-width-nanoseconds','ns64',n,d,mode)
    for ticks in [I64_MIN,I64_MIN+1,-1,0,1,I64_MAX-1,I64_MAX]:
        for source in bases:
            for target in bases:
                for mode in MODES:
                    add('time','timebase-rescale64','rescale64',ticks,*source,*target,mode)
    for mode in MODES:
        add('time','wide-rational-rescale','ticks128',I128_MAX,U128_MAX,U64_MAX,U64_MAX-2,mode,
            focus='both-rescale-products-wider-than-128-'+mode)
        add('time','wide-rational-rescale','ticks128',I128_MIN,U128_MAX,U64_MAX,U64_MAX-2,mode)
        add('time','invalid-timebase','ticks128',1,1,0,1,mode)
        add('time','invalid-timebase','ticks64',1,1,1,0,mode)

    # Adjacent continued fractions (Fibonacci ratios) need many comparison
    # refinements; Fraction ordering is still an independent exact comparator.
    fib=[1,1]
    while fib[-1]+fib[-2] <= I128_MAX:
        fib.append(fib[-1]+fib[-2])
    close=[(fib[-i],fib[-i-1]) for i in range(1,9)]
    for a in close:
        for b in close:
            for sign in [1,-1]:
                add('time','long-comparison-chains','cmp',sign*a[0],a[1],sign*b[0],b[1])
    for a,b in [((1,U128_MAX),(1,U128_MAX-1)),((I128_MAX,U128_MAX),(I128_MAX-1,U128_MAX-2)),
                ((-1,U128_MAX),(-1,U128_MAX-1)),((1,2),(2,4))]:
        add('time','nearly-equal-order','cmp',*a,*b)
        add('time','canonical-equality','eq',*a,*b)
    sort_pool=pool+close+[(-n,d) for n,d in close]+[(1,U128_MAX),(1,U128_MAX-1)]
    for group in [sort_pool,list(reversed(sort_pool)),sort_pool+[(0,7),(0,1),(0,U128_MAX),(2,4),(1,2)]]:
        add('time','extreme-sort','sort',';'.join(f'{n}/{d}' for n,d in group))
    for _ in range(40):
        group=[random_rat() for _ in range(48)]+sort_pool
        RNG.shuffle(group)
        add('time','random-full-width-sort','sort',';'.join(f'{n}/{d}' for n,d in group))

    edge128=[0,1,2,3,(1<<32)-1,1<<32,(1<<63)-1,1<<63,U64_MAX-1,U64_MAX,1<<64,
             (1<<64)+1,(1<<127)-1,1<<127,(1<<127)+1,U128_MAX-1,U128_MAX]
    for a in edge128:
        for b in edge128:
            add('wide','product-limb-boundaries','w_product',a,b,
                focus='maximum-u128-product' if a==b==U128_MAX else None)
    for _ in range(500):
        add('wide','random-full-width-products','w_product',RNG.getrandbits(128),RNG.getrandbits(128))
    values={0,1,2,3,U128_MAX,U128_MAX+1,U256_MAX-1,U256_MAX}
    for bit in range(256):
        values.update([max(0,(1<<bit)-1),1<<bit,min(U256_MAX,(1<<bit)+1)])
    for a in sorted(values):
        wide_case('all-wide-bits','w_to128',a)
        for b in [0,1,U256_MAX]:
            wide_case('wide-edge-pairs','w_add',a,b)
            wide_case('wide-edge-pairs','w_sub',max(a,b),min(a,b))
            wide_case('wide-edge-pairs','w_cmp',a,b)
        if a:
            wide_case('equal-division','w_div',a,a)
            wide_case('gcd-with-zero','w_gcd',a,0)
    for bit in range(256):
        scale=1<<bit
        a=3*scale if 3*scale<=U256_MAX else scale
        b=5*scale if 5*scale<=U256_MAX else scale
        wide_case('gcd-common-shifts','w_gcd',a,b)
    for a,b in [(U256_MAX,1<<255),(U256_MAX,(1<<255)+1),(U256_MAX,U128_MAX),
                (1<<255,U256_MAX),(U256_MAX,U256_MAX),(0,U256_MAX)]:
        wide_case('division-top-bit','w_div',a,b,focus=f'wide-division-{a}-{b}')
    wide_case('focused-wide-overflow','w_add',U256_MAX,1,focus='wide-carry-out-overflow')
    wide_case('focused-wide-borrow','w_sub',1<<192,1,focus='wide-borrow-crosses-three-limbs')
    wide_case('gcd-both-zero','w_gcd',0,0)
    for _ in range(500):
        a,b=RNG.getrandbits(256),RNG.getrandbits(256)
        for op in ('w_add','w_gcd','w_cmp'):
            wide_case('random-full-width-wide',op,a,b)
        wide_case('random-full-width-wide','w_sub',max(a,b),min(a,b))
        if b:
            wide_case('random-full-width-wide','w_div',a,b)


def emit():
    file_manifest=[]
    index=[]
    coverage={}
    for suite, keyed in CASES.items():
        rows=list(keyed.values())
        assert len({row['id'] for row in rows})==len(rows)
        inputs=''.join('\t'.join([r['id'],r['op'],*r['args']])+'\n' for r in rows)
        expected=''.join('\t'.join([r['id'],*r['expected']])+'\n' for r in rows)
        for kind,data in [('vectors',inputs),('expected',expected)]:
            name=f'{kind}-{suite}.tsv'
            (ROOT/name).write_text(data)
            file_manifest.append(dict(path=name,sha256=sha(data.encode()),bytes=len(data.encode()),cases=len(rows)))
        coverage[suite]=dict(cases=len(rows),operations=dict(Counter(r['op'] for r in rows)),
                             outcomes=dict(Counter(r['expected'][0] for r in rows)),
                             families=dict(Counter(f for r in rows for f in r['families'])))
        for row in rows:
            copy={**row,'suite':suite,'families':sorted(row['families'])}
            if suite=='time' and row['op'] in ('add','sub'):
                an,ad,bn,bd=map(int,row['args'])
                if ad and bd:
                    a,b=Fraction(an,ad),Fraction(bn,bd)
                    result=a+b if row['op']=='add' else a-b
                    denominator=math.lcm(a.denominator,b.denominator)
                    common_numerator=result*denominator
                    assert common_numerator.denominator==1
                    copy['mathematical_shape']=dict(lcm_denominator_bits=denominator.bit_length(),
                        lcm_numerator_magnitude_bits=abs(common_numerator.numerator).bit_length(),
                        reduced_numerator_magnitude_bits=abs(result.numerator).bit_length(),reduced_denominator_bits=result.denominator.bit_length())
            index.append(copy)
    path=ROOT/'case-index.jsonl'
    path.write_text(''.join(json.dumps(r,sort_keys=True)+'\n' for r in index))
    file_manifest.append(dict(path=path.name,sha256=sha(path.read_bytes()),bytes=path.stat().st_size,cases=len(index)))
    focused=[]
    by_id={r['id']:r for r in index}
    for name,ident in FOCUS.items():
        focused.append(dict(name=name,**by_id[ident],classification='targeted regression candidate; not an observed Rust failure'))
    json_file('targeted-cases.json',focused)
    json_file('coverage.json',coverage)
    counts=Counter()
    for row in index:
        shape=row.get('mathematical_shape')
        if shape and row['expected'][0]=='ok':
            if shape['lcm_denominator_bits']>128:
                counts['successful_addsub_with_lcm_over_128_bits']+=1
            if shape['lcm_numerator_magnitude_bits']>127:
                counts['successful_addsub_with_numerator_over_127_bits']+=1
    assert counts['successful_addsub_with_lcm_over_128_bits']>=220
    json_file('manifest.json',dict(schema_version=1,seed_hex=hex(SEED),expected_engine='Python fractions.Fraction and arbitrary-precision integers; no Time/Wide model',
        generation_status='oracle generated; no Rust compilation/execution or performance measurement',
        python=sys.version,platform=platform.platform(),stdlib_fraction_sha256=sha(Path(fractions.__file__).read_bytes()),
        generator_sha256=sha(Path(__file__).read_bytes()),time_rounding_modes=MODES,
        bounds=dict(i128_min=str(I128_MIN),i128_max=str(I128_MAX),u128_max=str(U128_MAX),u64_max=str(U64_MAX),u256_max=str(U256_MAX)),
        row_counts={s:len(v) for s,v in CASES.items()},wide_reduction_coverage=dict(counts),files=file_manifest,
        scope='Boundary/bit-basis/structured/seeded full-width sampling, not exhaustive enumeration of the i128/u128 domains',
        no_third_party_dependencies=True))
    return coverage,dict(counts)


if __name__=='__main__':
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--output', type=Path, default=ROOT)
    args = parser.parse_args()
    ROOT = args.output.resolve()
    ROOT.mkdir(parents=True, exist_ok=True)
    paths=['crates/core/src/time.rs','crates/core/src/wide.rs','crates/core/src/error.rs','crates/core/src/lib.rs','crates/core/tests/time.rs']
    before=[dict(path=p,sha256=sha((REPO/p).read_bytes()),bytes=(REPO/p).stat().st_size) for p in paths]
    corpus()
    summary,shapes=emit()
    after=[dict(path=p,sha256=sha((REPO/p).read_bytes()),bytes=(REPO/p).stat().st_size) for p in paths]
    json_file('source-snapshot.json',dict(recorded_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),before=before,after=after,
        files_unchanged_during_generation=before==after,git_revision='unavailable: safe-media repository has no HEAD commit',
        limitations='Hashes are observations, not a filesystem freeze. Parent must recheck before compiling. Production source was read-only.',
        expected_generation_independent_of_source_contents=True))
    assert before==after, 'Production source moved during oracle generation; review and refresh source observation only'
    print(json.dumps(dict(cases={s:v['cases'] for s,v in summary.items()},wide_reduction_coverage=shapes),indent=2))
