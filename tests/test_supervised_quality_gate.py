"""Real Git tests for binding retained candidate bundles to expected_head_sha."""
from __future__ import annotations

import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tarfile

import pytest

ROOT = Path(__file__).resolve().parents[1]
HANDOFF_SPEC = importlib.util.spec_from_file_location(
    'handoff', ROOT / 'scripts/supervised_git_handoff.py'
)
handoff = importlib.util.module_from_spec(HANDOFF_SPEC)
HANDOFF_SPEC.loader.exec_module(handoff)
GATE_SPEC = importlib.util.spec_from_file_location(
    'quality_gate', ROOT / 'scripts/supervised_quality_gate.py'
)
quality_gate = importlib.util.module_from_spec(GATE_SPEC)
GATE_SPEC.loader.exec_module(quality_gate)


def git(path, *args):
    result = subprocess.run(
        ['git', '-C', str(path), '-c', 'user.name=Test',
         '-c', 'user.email=test@example.invalid', *args],
        env=handoff.environment(), capture_output=True, text=True, check=True,
    )
    return result.stdout.strip()


@pytest.fixture
def retained_candidate(tmp_path):
    source = tmp_path / 'source'
    source.mkdir()
    git(source, 'init', '--template=')
    (source / 'README').write_text('base\n')
    git(source, 'add', '.')
    git(source, 'commit', '-m', 'base')
    base = git(source, 'rev-parse', 'HEAD')
    bundle = tmp_path / 'base.bundle'
    frozen = handoff.freeze(source, base, bundle)
    workspace = tmp_path / 'workspace'
    handoff.prepare(bundle, base, frozen['bundle_sha256'], workspace)
    (workspace / 'feature').write_text('candidate change\n')
    git(workspace, 'add', '.')
    git(workspace, 'commit', '-m', 'candidate')
    candidate = git(workspace, 'rev-parse', 'HEAD')
    output = io.BytesIO()
    handoff.export(workspace, base, output)
    output.seek(0)
    root = tmp_path / 'candidate'
    root.mkdir()
    with tarfile.open(fileobj=output) as archive:
        archive.extractall(root, filter='data')
    revision = json.loads((root / 'revision.json').read_text())
    digest = handoff.digest(root / 'candidate.bundle')
    return {
        'base': base,
        'candidate': candidate,
        'root': root,
        'digest': digest,
        'handoff': {
            'base_commit': revision['base_commit'],
            'candidate_commit': revision['candidate_commit'],
            'bundle_sha256': digest,
            'verified': False,
        },
    }


def test_retained_bundle_verify_binds_only_matching_expected_head(retained_candidate, tmp_path):
    data = retained_candidate
    quality_gate.bind_retained_candidate(data['handoff'], data['candidate'])
    destination = tmp_path / 'verified'
    handoff.verify(
        data['root'] / 'candidate.bundle',
        data['base'],
        data['candidate'],
        data['digest'],
        data['root'] / 'workspace',
        destination,
    )
    assert (destination / 'feature').read_text() == 'candidate change\n'
    with pytest.raises(RuntimeError, match='not an externally verified PR head'):
        quality_gate.bind_retained_candidate(data['handoff'], '0' * 40)
    wrong = tmp_path / 'wrong'
    with pytest.raises(RuntimeError):
        handoff.verify(
            data['root'] / 'candidate.bundle',
            data['base'],
            '0' * 40,
            data['digest'],
            data['root'] / 'workspace',
            wrong,
        )


def test_execution_evidence_records_expected_head_not_fabricated_usage(retained_candidate):
    data = retained_candidate
    validation = [{
        'argv': ['python3', '-I', '/verify.py', '/candidate'],
        'exit_code': 0,
        'output_sha256': 'a' * 64,
        'duration_ms': 2,
    }]
    evidence = quality_gate.execution_evidence(data['candidate'], validation)
    assert evidence['checked_out_commit'] == data['candidate']
    assert evidence['checked_out_commit'] == data['handoff']['candidate_commit']
    assert evidence['usage']['total_tokens'] == 0
    assert evidence['usage']['model'] == ''
    assert evidence['usage']['cost_usd_micros'] is None
    assert evidence['isolation_cleanup_status'] == 'cleaned'
    result = quality_gate.activity_result(
        data['candidate'], data['handoff']['candidate_commit'], validation
    )
    assert result['status'] == 'succeeded'
    assert result['signals'][0]['signal_type'] == quality_gate.QUALITY_PASSED_SIGNAL
    assert result['signals'][0]['signal']['expected_head_sha'] == data['candidate']
