#!/usr/bin/env python3
"""Versioned, bounded supplemental exact-arithmetic oracle; no Rust execution.

Independent Fraction/int expectations only. Does not import, modify or invoke
the baseline generator, test driver, production arithmetic or native probe.
Writes only within .research/core-time-edge-oracle/. Standard library only.
Only the public baseline generator/driver are required. Older ignored artifacts
are optional provenance and never determine whether the corpus can be generated.
"""
import argparse
from collections import Counter
from fractions import Fraction
import hashlib
import json
import math
from pathlib import Path
import random
import sys

REPO = Path(__file__).resolve().parents[1]
LANE = REPO / '.research/core-time-edge-oracle'
VERSION = 'core-time-edge-v2'
# V2 fixes reproducibility/provenance only. Preserve the seed and mathematical
# case IDs so all numerical vectors/answers remain byte-identical to v1.
CASE_ID_PREFIX = 'edge-v1-'
SEED = 0x4354494D455F454447455F5631
I128_MIN, I128_MAX = -(1 << 127), (1 << 127) - 1
U64_MAX, U128_MAX, U256_MAX = (1 << 64) - 1, (1 << 128) - 1, (1 << 256) - 1
G_BITS = (63, 64, 65, 80, 96)
ACCEPTED_PER_FAMILY_G = 16
MAX_ATTEMPTS_PER_SLOT = 256
FAMILIES = ('same-sign-wide-reduction', 'opposite-sign-wide-cancellation')
EXPECTED_COUNTS = {'time': 1280, 'wide': 429}
EXPECTED_OPERATIONS = {'time': {'add': 640, 'sub': 640}, 'wide': {'w_build': 203, 'w_div': 226}}
DIVISORS = (1, 2, 3, (1 << 64) - 1, 1 << 64, (1 << 64) + 1,
            (1 << 128) - 1, 1 << 128, (1 << 128) + 1)
EXPECTED_DIVISOR_COUNTS = (8, 16, 22, 29, 32, 29, 29, 32, 29)
REQUIRED_BASELINE_PATHS = ('tools/generate-time-oracle.py', 'tools/test-core-time.py')
OPTIONAL_PRIOR_ARTIFACT_PATHS = (
    '.research/core-time-oracle/generate.py', '.research/core-time-oracle/manifest.json',
    '.research/core-time-oracle/vectors-time.tsv', '.research/core-time-oracle/expected-time.tsv',
    '.research/core-time-oracle/vectors-wide.tsv', '.research/core-time-oracle/expected-wide.tsv',
)


class CandidateRejected(Exception):
    """A bounded mathematical search rejection, not a production failure."""


def require(condition, description):
    # These source-shape checks remain active even under python -O.
    if not condition:
        raise ValueError(description)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def json_bytes(value):
    return (json.dumps(value, sort_keys=True, indent=2) + '\n').encode()


def split(number):
    require(0 <= number <= U256_MAX, 'Wide operand outside u256')
    return number & U128_MAX, number >> 128


def wide_text(number):
    require(0 <= number <= U256_MAX, 'Wide result outside u256')
    return 'Wide([' + ', '.join(str((number >> bit) & U64_MAX) for bit in (0, 64, 128, 192)) + '])'


def time_shape(op, args, family):
    an, ad, bn, bd = map(int, args)
    for n, d in ((an, ad), (bn, bd)):
        require(I128_MIN <= n <= I128_MAX and 1 <= d <= U128_MAX, 'Time operand outside public domain')
        require(math.gcd(abs(n), d) == 1, 'Time input is not canonical')
    left, right = Fraction(an, ad), Fraction(bn, bd)
    value = left + right if op == 'add' else left - right
    require(I128_MIN <= value.numerator <= I128_MAX and value.denominator <= U128_MAX,
            'Time exact reduced result is not representable')
    common = math.lcm(left.denominator, right.denominator)
    lterm = left.numerator * (common // left.denominator)
    rterm = right.numerator * (common // right.denominator) * (1 if op == 'add' else -1)
    numerator = lterm + rterm
    require(Fraction(numerator, common) == value, 'LCM shape disagrees with Fraction result')
    require(common.bit_length() > 128 and abs(numerator).bit_length() > 128,
            'Time case does not have BOTH requested wide pre-reduction quantities')
    require(abs(lterm).bit_length() > 128 and abs(rterm).bit_length() > 128,
            'Time case lost the independently wide source products')
    opposite = lterm * rterm < 0
    lost_bits = max(abs(lterm).bit_length(), abs(rterm).bit_length()) - abs(numerator).bit_length()
    if family == 'opposite-sign-wide-cancellation':
        require(opposite and lost_bits >= 12, 'Cancellation case lacks opposite products or meaningful cancellation')
    else:
        require(not opposite, 'Same-sign family accidentally contains opposite effective products')
    return value, dict(lcm_denominator=str(common), lcm_denominator_bits=common.bit_length(),
        pre_reduction_numerator=str(numerator), pre_reduction_numerator_magnitude_bits=abs(numerator).bit_length(),
        left_effective_product=str(lterm), right_effective_product=str(rterm),
        left_product_magnitude_bits=abs(lterm).bit_length(), right_product_magnitude_bits=abs(rterm).bit_length(),
        opposite_effective_signs=opposite, cancellation_lost_bits=lost_bits,
        reduction_gcd=str(math.gcd(abs(numerator), common)), reduced_numerator=str(value.numerator),
        reduced_denominator=str(value.denominator), result_representable=True)


def candidate(rng, family, gbits):
    g = 1 << gbits
    width = min(64, 128 - gbits)
    u = rng.getrandbits(width) | (1 << (width - 1)) | 1
    v = rng.getrandbits(width) | (1 << (width - 1)) | 1
    if u == v or math.gcd(u, v) != 1:
        raise CandidateRejected('non-coprime-or-equal-denominator-factors')
    if family == 'same-sign-wide-reduction':
        a = ((1 << 122) + rng.getrandbits(122)) | 1
        residue = (-a * v * pow(u, -1, g)) % g
        target = (1 << 122) + rng.getrandbits(122)
        b = residue + (target // g) * g
    else:
        # Construct a nonzero residual g*k, with bit length 136 (>128),
        # while each individual product is much larger and signs cancel.
        kbits = 136 - gbits
        k = rng.getrandbits(kbits) | (1 << (kbits - 1)) | 1
        if math.gcd(k, u * v) != 1:
            raise CandidateRejected('residual-not-coprime-to-denominator-factors')
        target = (1 << 123) + rng.getrandbits(122)
        residue = (g * k * pow(v, -1, u)) % u
        a = (target // u) * u + residue
        if a % 2 == 0:
            a += u
        require((g * k - a * v) % u == 0, 'Diophantine input construction was not exact')
        b = (g * k - a * v) // u
        require(a > 0 and b < 0, 'Cancellation input did not have opposite signs')
    if math.gcd(abs(a), g * u) != 1 or math.gcd(abs(b), g * v) != 1:
        raise CandidateRejected('source-operands-not-canonical')
    # Final expected answer always comes from Fraction, not the construction's
    # modular residual or a model of Rust's reduction algorithm.
    time_shape('add', (a, g * u, b, g * v), family)
    return (a, g * u), (b, g * v)


class Corpus:
    def __init__(self):
        self.rows = {'time': {}, 'wide': {}}
        self.ids = set()
        self.attempts = []
        self.rejections = Counter()

    def add(self, suite, op, args, expected, family, shape):
        args = tuple(str(v) for v in args)
        key = (op, *args)
        if key in self.rows[suite]:
            raise ValueError('Duplicate edge input would reduce the frozen coverage count')
        ident = CASE_ID_PREFIX + op + '-' + sha(('\t'.join(key)).encode())[:20]
        require(ident not in self.ids, 'Edge case ID collision')
        self.ids.add(ident)
        row = dict(id=ident, suite=suite, op=op, args=args, expected=expected, family=family, shape=shape)
        self.rows[suite][key] = row
        return row

    def time(self, op, a, b, family, seed_slot):
        value, shape = time_shape(op, (*a, *b), family)
        shape['seed_slot'] = seed_slot
        return self.add('time', op, (*a, *b), ('ok', str(value.numerator), str(value.denominator)), family, shape)

    def generate_time(self, out):
        rng = random.Random(SEED)
        for family in FAMILIES:
            for gbits in G_BITS:
                for slot in range(ACCEPTED_PER_FAMILY_G):
                    identity = dict(family=family, common_factor_bits=gbits, slot=slot)
                    recent = []
                    for attempt in range(1, MAX_ATTEMPTS_PER_SLOT + 1):
                        try:
                            a, b = candidate(rng, family, gbits)
                            self.attempts.append(dict(**identity, attempts=attempt))
                            break
                        except CandidateRejected as exc:
                            self.rejections[str(exc)] += 1
                            recent = (recent + [str(exc)])[-8:]
                        except Exception as exc:
                            preserve_failure(out, 'unexpected-source-shape', dict(**identity,attempt=attempt,error=repr(exc),
                                rng_state=repr(rng.getstate()),classification='oracle construction/shape failure; no native source execution'))
                            raise
                    else:
                        preserve_failure(out, 'attempt-cap', dict(**identity,attempt_limit=MAX_ATTEMPTS_PER_SLOT,recent_rejections=recent,
                            seed_hex=hex(SEED),classification='bounded oracle search exhausted; not a Rust arithmetic repro'))
                        raise ValueError(f'Bounded candidate search exhausted at {identity}')
                    neg_a, neg_b = (-a[0], a[1]), (-b[0], b[1])
                    # Two operand orders, both result signs, both operations.
                    variants = [('add',a,b),('add',b,a),('sub',a,neg_b),('sub',b,neg_a),
                                ('add',neg_a,neg_b),('add',neg_b,neg_a),('sub',neg_a,b),('sub',neg_b,a)]
                    for op, left, right in variants:
                        self.time(op, left, right, family, identity)

    def generate_wide(self):
        divisions = []
        for d, required in zip(DIVISORS, EXPECTED_DIVISOR_COUNTS):
            qmax = U256_MAX // d
            bits = qmax.bit_length() - 1
            ones = (1 << bits) - 1
            alternating = sum(1 << bit for bit in range(bits) if bit % 2 == 0) | (1 << (bits - 1))
            quotients = (qmax, ones, ones ^ 1, ones ^ (1 << 63), ones ^ (1 << 64),
                         ones ^ (1 << 126), alternating, 1 << (qmax.bit_length() - 1))
            require(len(set(quotients)) == 8, 'Quotient patterns unexpectedly overlap')
            remainders = sorted(r for r in {0, 1, d // 2, d - 1} if 0 <= r < d)
            start = len(divisions)
            for chosen_q in quotients:
                for chosen_r in remainders:
                    n = chosen_q * d + chosen_r
                    if n > U256_MAX:
                        # Exactly specified unrepresentable source candidates;
                        # expected per-divisor counts below make omissions visible.
                        continue
                    q, r = divmod(n, d)
                    require((q,r) == (chosen_q,chosen_r), 'Wide divmod disagrees with independently constructed source')
                    require(n.bit_length() > 128, 'Division does not reach the wide-numerator path')
                    require(0 <= r < d and q*d+r == n, 'Wide division invariant failure')
                    popcount = q.bit_count()
                    shape = dict(numerator=str(n), divisor=str(d), quotient=str(q), remainder=str(r),
                        numerator_bits=n.bit_length(), divisor_bits=d.bit_length(), quotient_bits=q.bit_length(),
                        quotient_popcount=popcount, dense=popcount*2 >= q.bit_length(),
                        dense_over_128=q.bit_length() > 128 and popcount*2 >= q.bit_length(),
                        divisor_class='small' if d <= 3 else 'u64-boundary' if d < 1 << 65 else 'u128-boundary')
                    divisions.append((n,d,q,r,shape))
            require(len(divisions)-start == required, f'Frozen division count changed for divisor {d}')
            local = [entry for entry in divisions[start:] if entry[1] == d]
            if d < (1 << 65):
                require(any(entry[4]['dense_over_128'] for entry in local), 'Missing dense >128-bit quotient')
            else:
                require(any(entry[4]['dense'] and entry[4]['quotient_bits']==128 for entry in local),
                        'Missing dense 128-bit quotient at the u128 divisor boundary')
                if d >= 1 << 128:
                    require(all(entry[4]['quotient_bits']<=128 for entry in local), 'Impossible quotient width at divisor >=2^128')
                else:
                    require(any(entry[4]['quotient_bits']==129 for entry in local), 'Missing 129-bit lower-boundary transition')
        operands = sorted({value for n,d,_,_,_ in divisions for value in (n,d)})
        require(len(operands) == 203 and len(divisions) == 226, 'Frozen Wide coverage count changed')
        for operand in operands:
            self.add('wide','w_build',split(operand),('ok',wide_text(operand)),'operand-construction',
                     dict(value=str(operand),bits=operand.bit_length()))
        for n,d,q,r,shape in divisions:
            self.add('wide','w_div',(*split(n),*split(d)),('ok',wide_text(q),wide_text(r)),
                     'dense-quotient-and-divisor-boundaries',shape)

    def payloads(self):
        result = {}
        files = []
        index = []
        coverage = {}
        for suite, keyed in self.rows.items():
            rows = list(keyed.values())
            require(len(rows) == EXPECTED_COUNTS[suite], f'Frozen {suite} row count changed')
            ops = dict(Counter(row['op'] for row in rows))
            require(ops == EXPECTED_OPERATIONS[suite], f'Frozen {suite} operation counts changed')
            for kind in ('vectors','expected'):
                content = ''.join('\t'.join([r['id'],r['op'],*r['args']] if kind=='vectors' else [r['id'],*r['expected']])+'\n' for r in rows).encode()
                name = f'{kind}-{suite}.tsv'
                result[name] = content
                files.append(dict(path=name,sha256=sha(content),bytes=len(content),cases=len(rows)))
            coverage[suite] = dict(cases=len(rows),operations=ops,outcomes=dict(Counter(r['expected'][0] for r in rows)),
                families=dict(Counter(r['family'] for r in rows)))
            index += rows
        result['case-index.jsonl'] = ''.join(json.dumps(r,sort_keys=True)+'\n' for r in index).encode()
        result['coverage.json'] = json_bytes(coverage)
        time_rows = list(self.rows['time'].values())
        div_rows = [r for r in self.rows['wide'].values() if r['op']=='w_div']
        shape_summary = dict(time_both_pre_reduction_quantities_over_128=len(time_rows),
            opposite_sign_cancellation_cases=sum(r['family']==FAMILIES[1] for r in time_rows),
            minimum_time_lcm_denominator_bits=min(r['shape']['lcm_denominator_bits'] for r in time_rows),
            minimum_time_pre_reduction_numerator_bits=min(r['shape']['pre_reduction_numerator_magnitude_bits'] for r in time_rows),
            maximum_time_lcm_denominator_bits=max(r['shape']['lcm_denominator_bits'] for r in time_rows),
            maximum_time_pre_reduction_numerator_bits=max(r['shape']['pre_reduction_numerator_magnitude_bits'] for r in time_rows),
            minimum_cancellation_lost_bits=min(r['shape']['cancellation_lost_bits'] for r in time_rows if r['family']==FAMILIES[1]),
            wide_dense_quotients_over_128=sum(r['shape']['dense_over_128'] for r in div_rows),
            wide_division_cases_by_divisor={str(d):sum(int(r['shape']['divisor'])==d for r in div_rows) for d in DIVISORS},
            wide_maximum_quotient_bits=max(r['shape']['quotient_bits'] for r in div_rows))
        result['shape-summary.json'] = json_bytes(shape_summary)
        examples = []
        for family in FAMILIES:
            for op in ('add','sub'):
                examples.append(next(row for row in time_rows if row['family']==family and row['op']==op))
        for d in DIVISORS:
            local=[row for row in div_rows if int(row['shape']['divisor'])==d]
            examples.append(next((row for row in local if row['shape']['dense_over_128']),local[0]))
        result['targeted-cases.json'] = json_bytes(dict(classification='independently authored regression candidates; not observed Rust failures',cases=examples))
        for name in ('case-index.jsonl','coverage.json','shape-summary.json','targeted-cases.json'):
            files.append(dict(path=name,sha256=sha(result[name]),bytes=len(result[name])))
        manifest = dict(schema_version=2,corpus_version=VERSION,protocol='core-time-oracle TSV v1; existing probe operations only',
            numerical_contract_version='core-time-edge-v1',case_id_namespace=CASE_ID_PREFIX,
            seed_hex=hex(SEED),row_counts=EXPECTED_COUNTS,operation_counts=EXPECTED_OPERATIONS,
            expected_status='all successful; mathematical representability/shape assertions enforced before emit',
            expected_engine='fractions.Fraction for Time; unbounded Python int/divmod for Wide; no implementation model',
            generator_path='tools/generate-time-edge-oracle.py',generator_sha256=sha(Path(__file__).read_bytes()),
            baseline_observation_policy=dict(required_public_paths=REQUIRED_BASELINE_PATHS,
                optional_prior_artifact_paths=OPTIONAL_PRIOR_ARTIFACT_PATHS,
                optional_artifacts='present/absent/unreadable status only; never required or compared as a generation gate'),
            generation=dict(families=FAMILIES,common_factor_exponents=G_BITS,accepted_per_family_exponent=ACCEPTED_PER_FAMILY_G,
                maximum_attempts_per_slot=MAX_ATTEMPTS_PER_SLOT,maximum_total_candidate_attempts=MAX_ATTEMPTS_PER_SLOT*len(G_BITS)*len(FAMILIES)*ACCEPTED_PER_FAMILY_G,
                actual_attempts=sum(r['attempts'] for r in self.attempts),maximum_observed_attempts=max(r['attempts'] for r in self.attempts),
                rejected_candidate_reasons=dict(sorted(self.rejections.items())),slots=self.attempts),
            source_shape_summary=shape_summary,files=files,no_third_party_dependencies=True,
            width_limitation='For divisor >=2^128 and numerator <=2^256-1, quotient cannot exceed 128 bits. Lower boundary 2^128-1 also tests sparse 129-bit transition.',
            scope='Versioned supplemental mathematical correctness corpus, not performance, source execution or complete arithmetic qualification')
        result['manifest.json'] = json_bytes(manifest)
        return result, manifest


def preserve_failure(out, name, record):
    folder = out / 'repros'
    folder.mkdir(parents=True,exist_ok=True)
    (folder/f'{name}.json').write_bytes(json_bytes(record))


def baseline_observation():
    def observe(path, required):
        entry = dict(path=path,requirement='required-public' if required else 'optional',sha256=None)
        target = REPO/path
        try:
            if not target.is_file():
                if required:
                    raise ValueError('Required public baseline file missing or not regular: '+path)
                return dict(entry,status='absent' if not target.exists() else 'not-regular-file')
            return dict(entry,status='present',sha256=sha(target.read_bytes()))
        except OSError as error:
            if required:
                raise ValueError('Cannot read required public baseline file: '+path) from error
            return dict(entry,status='unreadable',error_type=type(error).__name__)
    return dict(required_public_files=[observe(p,True) for p in REQUIRED_BASELINE_PATHS],
                optional_prior_artifacts=[observe(p,False) for p in OPTIONAL_PRIOR_ARTIFACT_PATHS])


def verify_payloads(payloads, out):
    for name, content in payloads.items():
        require((out/name).is_file() and (out/name).read_bytes()==content,
                f'Frozen {VERSION} payload differs: {name}; do not overwrite, use a new reviewed corpus version')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,default=LANE/'v2')
    parser.add_argument('--check',action='store_true',help='regenerate in memory and compare all frozen bytes; no files written')
    args=parser.parse_args()
    out=args.output.resolve()
    require(out.is_relative_to(LANE.resolve()), 'Output must remain within the owned core-time-edge-oracle research lane')
    if (out/'manifest.json').is_file():
        existing=json.loads((out/'manifest.json').read_text())
        require(existing.get('corpus_version')==VERSION,
                'Output belongs to another frozen corpus version; choose a new v2 output directory')
    before=baseline_observation()
    corpus=Corpus()
    corpus.generate_time(out)
    corpus.generate_wide()
    payloads,manifest=corpus.payloads()
    after=baseline_observation()
    require(before['required_public_files']==after['required_public_files'],
            'Required public baseline files moved during generation')
    if args.check or (out/'manifest.json').exists():
        verify_payloads(payloads,out)
        mode='frozen-bytes-verified'
    else:
        out.mkdir(parents=True,exist_ok=True)
        for name,content in payloads.items():
            require(not (out/name).exists(), f'Refusing to overwrite pre-existing edge artifact {name}')
        for name,content in payloads.items():
            (out/name).write_bytes(content)
        (out/'baseline-observation.json').write_bytes(json_bytes(dict(schema_version=2,before=before,after=after,
            required_public_unchanged=True,
            optional_prior_artifacts_unchanged=before['optional_prior_artifacts']==after['optional_prior_artifacts'],
            source='required public file hashes are gating; optional prior artifacts are non-gating provenance only')))
        mode='new-version-emitted'
    print(json.dumps(dict(status=mode,corpus_version=VERSION,output=str(out),row_counts=manifest['row_counts'],
        operations=manifest['operation_counts'],shapes=manifest['source_shape_summary']),indent=2))
    return 0


if __name__=='__main__':
    try:
        raise SystemExit(main())
    except (ValueError,OSError) as error:
        print(f'time edge oracle failed: {error}',file=sys.stderr)
        raise SystemExit(1)
