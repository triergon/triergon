"""Explicit owner-approved, input-bound reference choices; never universal fixes."""


def rule_for(case, identity, contract_id=None):
    registry = identity.get('comparison_rules', {})
    matches = [rule for rule in registry.get('rules', [])
               if rule['case'] == case.get('id') or rule['profile'] == case.get('profile')]
    if not matches:
        return None
    if len(matches) != 1 or registry.get('approval', {}).get('status') != 'approved':
        raise ValueError('comparison rule is missing unique owner approval')
    rule = matches[0]
    if contract_id != 'K2-bounded-zlib-v1':
        raise ValueError('comparison rule used outside its selected contract')
    if identity.get('source_revision') != registry.get('reference_source_revision'):
        raise ValueError('comparison reference source pin changed')
    if case.get('id') != rule['case'] or case.get('profile') != rule['profile']:
        raise ValueError('comparison case/profile changed')
    if case.get('input_sha256', case.get('sha256')) != rule['input_sha256']:
        raise ValueError('comparison input identity changed')
    if case.get('seek_target_ns') is not None:
        raise ValueError('comparison rule does not cover a seek operation')
    return rule


def reference_record(identity, rule=None):
    name = rule['reference'] if rule else 'ffmpeg-native'
    record = identity.get('comparison_references', {}).get(name)
    if record is None:
        if name != 'ffmpeg-native':
            raise ValueError('selected native reference was not built')
        record = {'path': 'target/harness/ffmpeg-worker',
                  'sha256': identity.get('binaries', {}).get('reference'),
                  'kind': 'pinned native FFmpeg'}
    return {'id': name, **record}


def reference_summary(record):
    return {key: record[key] for key in ('id', 'sha256', 'kind')}


def validate_case_evidence(case, identity, contract_id=None):
    """Return the exact selected worker hashes or fail closed on a disposition."""
    rule = rule_for(case, identity, contract_id)
    selected = reference_record(identity, rule)
    if rule or 'reference' in case:
        if case.get('reference') != reference_summary(selected):
            raise ValueError('case reference does not match the built selected worker')
    if rule:
        parity = case.get('parity', {})
        if parity.get('comparison_rule') != rule['id']:
            raise ValueError('approved comparison rule evidence missing')
        if parity.get('source_content_status') != 'pass':
            raise ValueError('complete source content proof missing')
        if parity.get('raw_native_disposition') != {'status': 'reviewed', 'rule': rule['id']}:
            raise ValueError('raw native disagreement was not retained and checked')
        if not rule.get('raw_native_output_sha256') or parity.get('raw_native_output_sha256') != rule['raw_native_output_sha256']:
            raise ValueError('exact raw native output proof missing')
        if case.get('operation', {}).get('view') != rule['view']:
            raise ValueError('timed comparison view differs from the approved view')
    return {'candidate': identity.get('binaries', {}).get('candidate'),
            'reference': selected['sha256']}
