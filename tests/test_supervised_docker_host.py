from __future__ import annotations

import importlib.util
import io
import json
from pathlib import Path
import tarfile
import sys
import subprocess
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
    runner.cleanup = Mock()
    runner.complete = Mock()
    runner.launch = Mock(side_effect=AssertionError('must not launch'))
    (tmp_path / 'agent.jsonl').write_bytes(b'captured prefix')
    (tmp_path / 'agent.stderr').write_bytes(b'error prefix')
    runner.run()
    assert (tmp_path / 'agent.jsonl').read_bytes() == b'captured prefix'
    assert (tmp_path / 'agent.stderr').read_bytes() == b'error prefix'
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


def test_attached_streams_keep_separate_binary_bytes_and_nonzero_exit(tmp_path):
    module = load()
    renew = Mock()
    code = module.stream_agent_output([
        sys.executable, '-c',
        "import os; os.write(1, b'out\\x00\\xff'); os.write(2, b'err\\xff'); raise SystemExit(7)",
    ], tmp_path, 5, renew)
    assert code == 7
    assert (tmp_path / 'agent.jsonl').read_bytes() == b'out\x00\xff'
    assert (tmp_path / 'agent.stderr').read_bytes() == b'err\xff'
    renew.assert_called()


@pytest.mark.parametrize('stdout_size,stderr_size', [(600, 600), (1024 * 1024, 0), (0, 1024 * 1024)])
def test_shared_output_limit_bounds_retained_prefix_without_newlines(tmp_path, stdout_size, stderr_size):
    module = load()
    module.OUTPUT_LIMIT = 1024
    with pytest.raises(RuntimeError, match='output exceeded 1024 bytes'):
        module.stream_agent_output([
            sys.executable, '-c',
            f"import os; os.write(1, b'a' * {stdout_size}); os.write(2, b'b' * {stderr_size})",
        ], tmp_path, 5, Mock())
    assert sum((tmp_path / name).stat().st_size for name in ['agent.jsonl', 'agent.stderr']) == 1024


def test_exact_combined_output_limit_succeeds(tmp_path):
    module = load()
    module.OUTPUT_LIMIT = 1024
    assert module.stream_agent_output([
        sys.executable, '-c', "import os; os.write(1, b'a' * 512); os.write(2, b'b' * 512)",
    ], tmp_path, 5, Mock()) == 0
    assert (tmp_path / 'agent.jsonl').read_bytes() == b'a' * 512
    assert (tmp_path / 'agent.stderr').read_bytes() == b'b' * 512


def test_wall_timeout_keeps_output_and_reaps_attached_client(tmp_path, monkeypatch):
    module = load()
    popen = subprocess.Popen
    processes = []

    def start(*args, **kwargs):
        process = popen(*args, **kwargs)
        processes.append(process)
        return process

    monkeypatch.setattr(module.subprocess, 'Popen', start)
    renew = Mock()
    with pytest.raises(RuntimeError, match='wall deadline'):
        module.stream_agent_output([
            sys.executable, '-c', "import os,time; os.write(1,b'prefix'); time.sleep(30)",
        ], tmp_path, 1, renew)
    assert (tmp_path / 'agent.jsonl').read_bytes() == b'prefix'
    assert processes[0].poll() is not None
    assert renew.call_count > 1


def test_lease_failure_keeps_prefix_and_reaps_client(tmp_path):
    module = load()

    def renew():
        if (tmp_path / 'agent.jsonl').stat().st_size:
            raise RuntimeError('lease lost')

    with pytest.raises(RuntimeError, match='lease lost'):
        module.stream_agent_output([
            sys.executable, '-c', "import os,time; os.write(1,b'prefix'); time.sleep(30)",
        ], tmp_path, 5, renew)
    assert (tmp_path / 'agent.jsonl').read_bytes() == b'prefix'


def test_continuous_output_does_not_starve_lease_checks(tmp_path):
    module = load()
    module.OUTPUT_LIMIT = 2 * 1024 * 1024
    renew = Mock()
    assert module.stream_agent_output([
        sys.executable, '-c',
        "import os\nfor _ in range(256): os.write(1,b'a'*4096)",
    ], tmp_path, 5, renew) == 0
    assert renew.call_count > 2


def test_candidate_export_does_not_replace_logs_and_removes_agent_before_extract(tmp_path, monkeypatch):
    module = load()
    runner = module.Host.__new__(module.Host)
    runner.root, runner.name = tmp_path, 'owned'
    (tmp_path / 'agent.jsonl').write_bytes(b'original')
    (tmp_path / 'agent.stderr').write_bytes(b'errors')
    events = []

    def export(command, **kwargs):
        assert command[:6] == ['docker', 'exec', 'owned', 'python3', '-I', '-c']
        assert 'os.kill(-1, signal.SIGKILL)' in command[6]
        events.append('export')

    monkeypatch.setattr(module.subprocess, 'run', export)
    monkeypatch.setattr(module, 'docker', lambda *args: events.append(args))
    monkeypatch.setattr(module, 'extract_candidate', lambda *args: events.append('extract'))
    runner.capture()
    assert events == ['export', ('rm', '-f', 'owned'), 'extract']
    assert (tmp_path / 'agent.jsonl').read_bytes() == b'original'
    assert (tmp_path / 'agent.stderr').read_bytes() == b'errors'


def test_both_streams_larger_than_pipe_buffers_are_drained(tmp_path):
    module = load()
    assert module.stream_agent_output([
        sys.executable, '-c',
        "import os\nfor _ in range(256):\n os.write(1,b'a'*4096)\n os.write(2,b'b'*4096)",
    ], tmp_path, 5, Mock()) == 0
    assert (tmp_path / 'agent.jsonl').read_bytes() == b'a' * (1024 * 1024)
    assert (tmp_path / 'agent.stderr').read_bytes() == b'b' * (1024 * 1024)


def test_keyboard_interrupt_in_stream_keeps_prefix(tmp_path):
    module = load()

    def renew():
        if (tmp_path / 'agent.jsonl').stat().st_size:
            raise KeyboardInterrupt()

    with pytest.raises(KeyboardInterrupt):
        module.stream_agent_output([
            sys.executable, '-c', "import os,time; os.write(1,b'prefix'); time.sleep(30)",
        ], tmp_path, 5, renew)
    assert (tmp_path / 'agent.jsonl').read_bytes() == b'prefix'
