from __future__ import annotations

import importlib.util
import io
import json
from pathlib import Path
import tarfile
from unittest.mock import Mock

import pytest


def load():
    path = Path(__file__).resolve().parents[1] / 'scripts/run-supervised-docker-host.py'
    spec = importlib.util.spec_from_file_location('supervised_docker', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def host(tmp_path, phase):
    module = load()
    result = module.Host.__new__(module.Host)
    result.root = tmp_path
    result.endpoint = '/api/runtime-hosts/test'
    result.state = {'phase': phase, 'job': {'id': 'job', 'input': {'activity': 'modify'}},
                    'lease': {'lease_generation': 1, 'lease_expires_at': 'expiry', 'lease_proof': 'proof'}}
    return result


def test_completed_restart_never_invokes_agent(tmp_path):
    runner = host(tmp_path, 'completed')
    runner.launch = Mock(side_effect=AssertionError('must not launch'))
    runner.api = Mock(side_effect=AssertionError('must not claim'))
    runner.run()
    runner.launch.assert_not_called()


@pytest.mark.parametrize('phase', ['claimed', 'executing'])
def test_interrupted_run_cleans_up_and_reports_failure_without_reexecution(tmp_path, phase):
    runner = host(tmp_path, phase)
    runner.capture_logs = Mock()
    runner.cleanup = Mock()
    runner.complete = Mock()
    runner.launch = Mock(side_effect=AssertionError('must not launch'))
    runner.run()
    runner.cleanup.assert_called_once()
    runner.complete.assert_called_once()
    assert runner.state['result']['status'] == 'failed'
    assert runner.state['phase'] == 'completing'
    assert json.loads((tmp_path / 'state.json').read_text())['result']['status'] == 'failed'


def test_completion_retry_preserves_original_payload(tmp_path):
    runner = host(tmp_path, 'completing')
    original = {'lease_generation': 1, 'result': {'status': 'succeeded'}}
    (tmp_path / 'completion.json').write_text(json.dumps(original))
    runner.api = Mock(return_value={'completed': True})
    runner.run()
    assert runner.api.call_args.args[1] == original
    assert runner.state['phase'] == 'completed'


def test_unaccepted_completion_remains_pending(tmp_path):
    runner = host(tmp_path, 'completing')
    runner.state['result'] = {'status': 'failed'}
    runner.api = Mock(return_value={'completed': False})
    with pytest.raises(RuntimeError, match='not accepted'):
        runner.complete()
    assert runner.state['phase'] == 'completing'
    assert (tmp_path / 'completion.json').is_file()


def test_usage_requires_completed_turn(tmp_path):
    module = load()
    log = tmp_path / 'agent.jsonl'
    log.write_text('{"type":"turn.started"}\n')
    with pytest.raises(RuntimeError, match='no completed-turn'):
        module.read_usage(log)
    log.write_text(json.dumps({'type': 'turn.completed', 'usage': {
        'input_tokens': 20, 'cached_input_tokens': 15, 'output_tokens': 3}}))
    assert module.read_usage(log) == {
        'input_tokens': 20, 'cached_input_tokens': 15, 'output_tokens': 3, 'total_tokens': 23}


@pytest.mark.parametrize('kind', ['symlink', 'traversal'])
def test_candidate_archive_cannot_escape_destination(tmp_path, kind):
    module = load()
    archive = tmp_path / 'candidate.tar'
    with tarfile.open(archive, 'w') as stream:
        item = tarfile.TarInfo('link' if kind == 'symlink' else '../escaped')
        if kind == 'symlink':
            item.type = tarfile.SYMTYPE
            item.linkname = '/etc/passwd'
            stream.addfile(item)
        else:
            item.size = 1
            stream.addfile(item, io.BytesIO(b'x'))
    with pytest.raises((RuntimeError, tarfile.FilterError)):
        module.extract_candidate(archive, tmp_path / 'candidate')
    assert not (tmp_path / 'escaped').exists()


def test_cleanup_attempts_remaining_resources_and_preserves_errors(tmp_path, monkeypatch):
    module = load()
    runner = module.Host.__new__(module.Host)
    runner.name = 'owned'
    runner.root = tmp_path
    runner.state = {'phase': 'cleaning', 'result': {'status': 'failed'}}
    calls = []

    def command(*args):
        calls.append(args)
        if args == ('rm', '-f', 'owned'):
            raise RuntimeError('daemon refused removal')
        if args[:2] == ('network', 'ls'):
            return 'owned'
        return 'container-id'

    monkeypatch.setattr(module, 'docker', command)
    with pytest.raises(RuntimeError, match='cleanup incomplete'):
        runner.cleanup()
    assert ('rm', '-f', 'owned-verify') in calls
    assert ('rm', '-f', 'owned-proxy') in calls
    assert ('network', 'rm', 'owned') in calls
    persisted = json.loads((tmp_path / 'state.json').read_text())
    assert persisted['phase'] == 'cleaning'
    assert persisted['result']['status'] == 'failed'
    assert 'daemon refused removal' in persisted['cleanup_errors'][0]


def test_cleanup_resume_preserves_verified_result(tmp_path):
    runner = host(tmp_path, 'cleaning')
    runner.state['result'] = {'status': 'succeeded'}
    runner.cleanup = Mock()
    runner.complete = Mock()
    runner.launch = Mock(side_effect=AssertionError('must not launch'))
    runner.run()
    assert runner.state['result']['status'] == 'succeeded'
    runner.cleanup.assert_called_once()
    runner.complete.assert_called_once()


def test_keyboard_interrupt_persists_failure_before_cleanup(tmp_path):
    import hashlib

    runner = host(tmp_path, 'claiming')
    request = {'project': str(tmp_path), 'prompt': 'task'}
    submission = {'task_id': 'task-id', 'workflow_id': 'workflow-id'}
    runner.state.update(request=request, submission=submission)
    runner.name = 'test'
    digest = hashlib.sha256(b'\0'.join(x.encode() for x in [str(tmp_path), '', 'task-id', 'task'])).hexdigest()
    claim = {'claimed': True, **runner.state['lease'], 'runtime_job': {
        'id': 'job', 'input': {'activity': 'modify', 'workflow_id': 'workflow-id',
                             'command': {'prompt_ref': 'prompt-memory:' + digest}}}}
    runner.api = Mock(side_effect=[{}, claim])
    runner.launch = Mock(side_effect=KeyboardInterrupt)
    runner.capture_logs = Mock()

    def check_cleanup_state():
        saved = json.loads((tmp_path / 'state.json').read_text())
        assert saved['phase'] == 'cleaning'
        assert saved['result']['status'] == 'failed'
        assert 'keyboard' in saved['result']['summary']

    runner.cleanup = Mock(side_effect=check_cleanup_state)
    runner.complete = Mock()
    runner.run()
    runner.complete.assert_called_once()
    assert runner.state['phase'] == 'completing'
