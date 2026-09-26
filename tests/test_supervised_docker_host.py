from __future__ import annotations

import importlib.util
import io
import json
import os
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


def prepared_prompt():
    return {"prompt": "Server-rendered task and result contract.",
            "prompt_packet_digest": "a" * 64, "activity_result_schema": {"activity": "modify"}}


def native_result(status="succeeded"):
    return {"activity": "modify", "status": status, "summary": "Observed result",
            "artifacts": [{"artifact_type": "findings", "artifact": {"items": [1]}}],
            "signals": [{"signal_type": "Modified", "signal": {"count": 1}}],
            "validation": [{"command": "check", "status": "passed"}],
            "error": None, "error_kind": None}


def write_result_log(path, result):
    message = {"type": "item.completed", "item": {"type": "agent_message",
               "text": "```harness-activity-result\n" + json.dumps(result) + "\n```"}}
    usage = {"type": "turn.completed", "usage": {
        "input_tokens": 120, "output_tokens": 30, "cached_input_tokens": 80}}
    path.write_text(json.dumps(message) + "\n" + json.dumps(usage) + "\n")


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
    claim = {'claimed': True, 'prepared_prompt': prepared_prompt(), **runner.state['lease'], 'runtime_job': {
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
    runner.state = {'input_snapshot': {'base_commit': 'a' * 40}}
    runner.collect_resources = Mock(side_effect=lambda **kw: events.append('resources'))
    (tmp_path / 'agent.jsonl').write_bytes(b'original')
    (tmp_path / 'agent.stderr').write_bytes(b'errors')
    events = []

    def export(command, **kwargs):
        assert command[:6] == ['docker', 'exec', 'owned', 'python3', '-I', '-c']
        assert 'os.kill(-1, signal.SIGKILL)' in command[6]
        assert '/git-handoff.py' in command[6]
        assert command[-1] == 'a' * 40
        events.append('export')

    def extract(*args):
        events.append('extract')
        candidate = tmp_path / 'candidate'
        candidate.mkdir()
        (candidate / 'candidate.bundle').write_bytes(b'candidate bundle')
        (candidate / 'revision.json').write_text(json.dumps({
            'base_commit': 'a' * 40, 'candidate_commit': 'b' * 40}))

    monkeypatch.setattr(module.subprocess, 'run', export)
    monkeypatch.setattr(module, 'docker', lambda *args: events.append(args))
    monkeypatch.setattr(module, 'extract_candidate', extract)
    runner.capture()
    assert events == ['export', 'resources', ('rm', '-f', 'owned'), 'extract']
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


@pytest.mark.parametrize('failure', ['deadline', 'output'])
def test_stuck_client_cleanup_preserves_primary_failure_and_closes_streams(tmp_path, monkeypatch, failure):
    module = load()
    module.OUTPUT_LIMIT = 1
    streams = []
    for _ in range(2):
        reader, writer = os.pipe()
        os.write(writer, b'xx')
        os.close(writer)
        streams.append(os.fdopen(reader, 'rb', buffering=0))
    process = Mock(stdout=streams[0], stderr=streams[1])
    process.poll.return_value = None
    process.wait.side_effect = subprocess.TimeoutExpired(['docker', 'exec'], 5)
    monkeypatch.setattr(module.quality_gate.subprocess, 'Popen', Mock(return_value=process))
    try:
        with pytest.raises(RuntimeError) as raised:
            module.stream_agent_output(['docker', 'exec'], tmp_path, 0 if failure == 'deadline' else 5, Mock())
        message = str(raised.value)
        assert ('wall deadline' if failure == 'deadline' else 'output exceeded') in message
        assert 'agent output cleanup failed: wait:' in message
        assert 'timed out after 5 seconds' in message
        assert raised.value.__cause__ is not None
        assert all(stream.closed for stream in streams)
        process.kill.assert_called_once()
        process.wait.assert_called_once_with(timeout=5)
    finally:
        for stream in streams:
            stream.close()


def resource_runner(tmp_path, monkeypatch, *, running=True, oom=0, pids_max=0, stop_error=False,
                    disk=None, disk_error=None):
    runner = host(tmp_path, 'executing')
    runner.name = 'owned'
    runner.state['container_started'] = True
    runner.state['candidate_id'] = 'a' * 64
    metrics = {'cpu_time_micros': 2300123, 'current_pids_before': 1, 'peak_memory_bytes': 42000000,
               'peak_pids': 7, 'current_pids': 1, 'memory_events': {'oom': oom, 'oom_kill': oom},
               'pids_events': {'max': pids_max}}
    if disk is None:
        disk = {
            'sample_kind': 'terminal', 'observed_at': '2026-09-20T12:00:00Z',
            'scope': 'candidate agent-writable tmpfs roots under trial policy',
            'mounts': [
                {'root': '/workspace', 'device': '0:1', 'fstype': 'tmpfs',
                 'used_bytes': 4096, 'capacity_bytes': 512 * 1024 * 1024, 'frsize': 4096},
                {'root': '/home/harness', 'device': '0:2', 'fstype': 'tmpfs',
                 'used_bytes': 0, 'capacity_bytes': 128 * 1024 * 1024, 'frsize': 4096},
                {'root': '/tmp', 'device': '0:3', 'fstype': 'tmpfs',
                 'used_bytes': 8192, 'capacity_bytes': 64 * 1024 * 1024, 'frsize': 4096},
                {'root': '/dev/shm', 'device': '0:4', 'fstype': 'tmpfs',
                 'used_bytes': 0, 'capacity_bytes': 64 * 1024 * 1024, 'frsize': 4096},
            ],
            'aggregate_used_bytes': 12288, 'aggregate_capacity_bytes': 768 * 1024 * 1024,
            'distinct_filesystems': 4,
        }
    calls = []
    def invoke(*args, **kwargs):
        calls.append(args)
        joined = '\n'.join(str(arg) for arg in args)
        if args[0] == 'inspect':
            return json.dumps({'Running': running, 'OOMKilled': bool(oom)})
        if args[:2] == ('exec', 'owned') and 'signal.SIGKILL' in joined:
            if stop_error:
                raise RuntimeError('PID exhaustion prevented quiescing')
            return ''
        if args[:2] == ('exec', 'owned-observer'):
            return json.dumps(metrics)
        if args[:2] == ('exec', 'owned') and 'statvfs' in joined:
            if disk_error is not None:
                raise RuntimeError(disk_error)
            return json.dumps(disk)
        return ''
    # host() loads its own module; patch the method globals, not another import.
    monkeypatch.setitem(runner.collect_resources.__globals__, 'docker', invoke)
    return runner, calls, metrics, disk


def test_resource_peaks_are_saved_outside_candidate_after_quiescing(tmp_path, monkeypatch):
    runner, calls, metrics, disk = resource_runner(tmp_path, monkeypatch)
    runner.collect_resources(stop_agents=True)
    evidence = json.loads((tmp_path / 'candidate-resources.json').read_text())
    assert evidence['status'] == 'complete'
    assert evidence['metrics'] == metrics
    assert evidence['disk'] == disk
    assert evidence['disk']['sample_kind'] == 'terminal'
    stop = next(i for i, c in enumerate(calls)
                if c[:2] == ('exec', 'owned') and 'signal.SIGKILL' in '\n'.join(map(str, c)))
    sample = next(i for i, c in enumerate(calls) if c[:2] == ('exec', 'owned-observer'))
    disk_i = next(i for i, c in enumerate(calls)
                  if c[:2] == ('exec', 'owned') and 'statvfs' in '\n'.join(map(str, c)))
    assert stop < sample < disk_i
    assert runner.result('succeeded', 'verified')['artifacts'][0]['artifact'] == evidence


@pytest.mark.parametrize('failure', ['oom', 'pids', 'dead', 'pid-exec'])
def test_resource_failures_cannot_be_success_or_fake_zero(tmp_path, monkeypatch, failure):
    runner, calls, metrics, disk = resource_runner(
        tmp_path, monkeypatch, running=failure != 'dead', oom=int(failure == 'oom'),
        pids_max=int(failure in {'pids', 'pid-exec'}), stop_error=failure == 'pid-exec')
    with pytest.raises(RuntimeError, match='OOM or PID limit' if failure in {'oom', 'pids'}
                       else 'resource evidence incomplete'):
        runner.collect_resources(stop_agents=True)
    evidence = json.loads((tmp_path / 'candidate-resources.json').read_text())
    assert evidence['status'] == ('limit_exceeded' if failure in {'oom', 'pids'} else 'incomplete')
    assert evidence['metrics'] == metrics
    assert evidence['error']
    assert runner.result('succeeded', 'verifier accepted')['status'] == 'failed'
    assert runner.result('failed', 'original timeout')['error'] == 'original timeout'


@pytest.mark.parametrize('resource', ['memory', 'pids'])
@pytest.mark.parametrize('measurement_failure', [None, 'disk', 'quiescence', 'dead', 'pid-exec'])
def test_eval_quota_failures_report_only_complete_measurements(
        tmp_path, monkeypatch, resource, measurement_failure):
    runner, _, metrics, _ = resource_runner(
        tmp_path, monkeypatch, oom=int(resource == 'memory'), pids_max=int(resource == 'pids'),
        running=measurement_failure != 'dead', stop_error=measurement_failure == 'pid-exec',
        disk_error='disk measurement unavailable' if measurement_failure == 'disk' else None)
    if measurement_failure == 'quiescence':
        metrics['current_pids'] = 2
    runner.state.update(
        enforced_limits={'requested': {}, 'effective': {
            'cpu_time_secs': 60, 'memory_bytes': 512 * 1024 * 1024, 'pids': 32,
            'disk_bytes': 1024 * 1024 * 1024, 'output_bytes': 1024, 'wall_time_secs': 30,
        }, 'caps': []},
        output_bytes=100, wall_time_millis=1000,
    )
    with pytest.raises(RuntimeError):
        runner.collect_resources(stop_agents=True)
    for native_status in ('succeeded', 'succeeded_with_blockers', 'failed'):
        result = runner.result(native_status, 'original result', native=native_result(native_status))
        assert result['status'] == 'failed'
        reports = [item['artifact'] for item in result['artifacts']
                   if item['artifact_type'] == 'resource_limit_report']
        if measurement_failure:
            assert runner.state['resource_evidence']['status'] == 'incomplete'
            assert reports == []
        else:
            assert runner.state['resource_evidence']['status'] == 'limit_exceeded'
            assert reports[0]['termination']['resource'] == resource
            assert reports[0]['usage']['peak_memory_bytes'] == metrics['peak_memory_bytes']
            assert reports[0]['usage']['peak_pids'] == metrics['peak_pids']
            if native_status != 'failed':
                assert result['signals'] == []


def test_missing_observer_keeps_missing_evidence_instead_of_zero(tmp_path, monkeypatch):
    runner, _, _, _ = resource_runner(tmp_path, monkeypatch)
    def unavailable(*args):
        if args[0] == 'inspect':
            return json.dumps({'Running': False, 'OOMKilled': False})
        raise RuntimeError('observer cgroup files missing')
    monkeypatch.setitem(runner.collect_resources.__globals__, 'docker', unavailable)
    with pytest.raises(RuntimeError, match='files missing'):
        runner.collect_resources(stop_agents=True)
    evidence = runner.state['resource_evidence']
    assert 'metrics' not in evidence
    assert 'disk' not in evidence
    assert evidence['status'] == 'incomplete'
    assert runner.result('succeeded', 'accepted')['status'] == 'failed'


def test_observer_mount_is_candidate_only_and_has_no_extra_privileges(tmp_path, monkeypatch):
    module = load()
    runner = module.Host.__new__(module.Host)
    runner.root, runner.name, runner.state = tmp_path, 'owned', {}
    from types import SimpleNamespace
    runner.args = SimpleNamespace(image='sha256:' + 'b' * 64)
    calls = []
    def invoke(*args):
        calls.append(args)
        if args[0] == 'info':
            return json.dumps({'CgroupVersion': '2', 'CgroupDriver': 'cgroupfs'})
        if args[0] == 'inspect':
            return 'a' * 64
        return '{}'
    monkeypatch.setattr(module, 'docker', invoke)
    monkeypatch.setattr(module.os, 'getuid', lambda: 1001)
    runner.start_observer()
    command = next(c for c in calls if c[0] == 'run')
    mount = command[command.index('--mount') + 1]
    assert mount == 'type=bind,src=/sys/fs/cgroup/docker/' + 'a' * 64 + ',dst=/sys/fs/cgroup,readonly'
    assert command[command.index('--network') + 1] == 'none'
    assert command[command.index('--user') + 1].startswith('1001:')
    assert command[command.index('--shm-size') + 1] == '64m'
    assert '--privileged' not in command and '--pid' not in command
    assert not any('docker.sock' in argument for argument in command)


def test_nonquiescent_candidate_retains_metrics_but_cannot_pass(tmp_path, monkeypatch):
    runner, _, metrics, disk = resource_runner(tmp_path, monkeypatch)
    metrics['current_pids'] = 2
    with pytest.raises(RuntimeError, match='not quiescent'):
        runner.collect_resources(stop_agents=True)
    assert runner.state['resource_evidence']['metrics']['current_pids'] == 2
    assert runner.state['resource_evidence']['disk'] == disk
    assert runner.result('succeeded', 'accepted')['status'] == 'failed'


def test_missing_disk_measurement_cannot_succeed(tmp_path, monkeypatch):
    runner, _, _, _ = resource_runner(tmp_path, monkeypatch, disk_error='statvfs unavailable')
    with pytest.raises(RuntimeError, match='statvfs unavailable'):
        runner.collect_resources(stop_agents=True)
    evidence = runner.state['resource_evidence']
    assert evidence['status'] == 'incomplete'
    assert 'disk' not in evidence
    assert 'metrics' in evidence
    assert runner.result('succeeded', 'accepted')['status'] == 'failed'


def test_malformed_disk_measurement_cannot_succeed_or_zero_fill(tmp_path, monkeypatch):
    bad = {
        'sample_kind': 'peak', 'observed_at': '2026-09-20T12:00:00Z',
        'scope': 'candidate agent-writable tmpfs roots under trial policy',
        'mounts': [], 'aggregate_used_bytes': 0, 'aggregate_capacity_bytes': 0,
        'distinct_filesystems': 0,
    }
    runner, _, _, _ = resource_runner(tmp_path, monkeypatch, disk=bad)
    with pytest.raises(RuntimeError, match='resource evidence incomplete'):
        runner.collect_resources(stop_agents=True)
    evidence = runner.state['resource_evidence']
    assert evidence['status'] == 'incomplete'
    assert evidence['disk'] == bad
    assert 'terminal' in evidence['error'] or 'mounts' in evidence['error']
    assert runner.result('succeeded', 'accepted')['status'] == 'failed'


def test_disk_aggregate_counts_distinct_filesystems_only():
    module = load()
    shared = {
        'sample_kind': 'terminal', 'observed_at': '2026-09-20T12:00:00Z',
        'scope': 'candidate agent-writable tmpfs roots under trial policy',
        'mounts': [
            {'root': '/workspace', 'device': '0:9', 'fstype': 'tmpfs',
             'used_bytes': 100, 'capacity_bytes': 1000, 'frsize': 4096},
            {'root': '/home/harness', 'device': '0:9', 'fstype': 'tmpfs',
             'used_bytes': 100, 'capacity_bytes': 1000, 'frsize': 4096},
            {'root': '/tmp', 'device': '0:10', 'fstype': 'tmpfs',
             'used_bytes': 50, 'capacity_bytes': 500, 'frsize': 4096},
            {'root': '/dev/shm', 'device': '0:11', 'fstype': 'tmpfs',
             'used_bytes': 25, 'capacity_bytes': 250, 'frsize': 4096},
        ],
        'aggregate_used_bytes': 175, 'aggregate_capacity_bytes': 1750,
        'distinct_filesystems': 3,
    }
    assert module.disk_evidence_errors(shared) == []
    shared['aggregate_used_bytes'] = 275
    assert 'aggregate_used_bytes' in ';'.join(module.disk_evidence_errors(shared))


def test_disk_metrics_reader_rejects_missing_root(tmp_path):
    module = load()
    mountinfo = (
        f'1 0 0:1 / {tmp_path}/workspace rw - tmpfs tmpfs rw\n'
        f'2 0 0:2 / {tmp_path}/home rw - tmpfs tmpfs rw\n'
        f'3 0 0:3 / {tmp_path}/tmp rw - tmpfs tmpfs rw\n'
    )
    (tmp_path / 'workspace').mkdir()
    (tmp_path / 'home').mkdir()
    (tmp_path / 'tmp').mkdir()
    # Intentionally omit /dev/shm directory and mountinfo entry.
    script = (module.DISK_METRICS_SCRIPT
              .replace("ROOTS = ('/workspace', '/home/harness', '/tmp', '/dev/shm')",
                       f"ROOTS = ({str(tmp_path / 'workspace')!r}, {str(tmp_path / 'home')!r}, "
                       f"{str(tmp_path / 'tmp')!r}, {str(tmp_path / 'shm')!r})")
              .replace("Path('/proc/self/mountinfo')", f"Path({str(tmp_path / 'mountinfo')!r})"))
    (tmp_path / 'mountinfo').write_text(mountinfo)
    result = subprocess.run([sys.executable, '-I', '-c', script], capture_output=True, text=True)
    assert result.returncode != 0
    assert 'missing mount root' in result.stderr
    assert result.stdout == ''


def test_native_metrics_reader_requires_cpu_total(tmp_path):
    module = load()
    for name, text in {'pids.current': '1', 'cpu.stat': 'user_usec 12\n'}.items():
        (tmp_path / name).write_text(text)
    script = module.CGROUP_METRICS_SCRIPT.replace("Path('/sys/fs/cgroup')", f'Path({str(tmp_path)!r})')
    result = subprocess.run([sys.executable, '-I', '-c', script], capture_output=True, text=True)
    assert result.returncode != 0
    assert 'usage_usec' in result.stderr
    assert result.stdout == ''


def test_candidate_reaper_collects_exited_children_with_fixed_deadline():
    module = load()
    setup = "import os\npid=os.fork()\nif pid==0: os._exit(0)\n"
    check = "\ntry: os.waitpid(pid,os.WNOHANG)\nexcept ChildProcessError: pass\nelse: raise AssertionError('child was not reaped')\n"
    script = setup + module.CANDIDATE_REAPER_SCRIPT.replace('+ 900', '+ 0.2') + check
    subprocess.run([sys.executable, '-I', '-c', script], check=True, timeout=3)


def test_capture_failure_retains_completed_model_usage(tmp_path):
    import hashlib
    runner = host(tmp_path, 'claiming')
    runner.name = 'test'
    from types import SimpleNamespace
    runner.args = SimpleNamespace(model='scripted-model')
    runner.state.update(request={'project': str(tmp_path), 'prompt': 'task'},
                        submission={'task_id': 'task-id', 'workflow_id': 'workflow-id'})
    digest = hashlib.sha256(b'\0'.join(x.encode() for x in [str(tmp_path), '', 'task-id', 'task'])).hexdigest()
    claim = {'claimed': True, 'prepared_prompt': prepared_prompt(), **runner.state['lease'], 'runtime_job': {
        'id': 'job', 'input': {'activity': 'modify', 'workflow_id': 'workflow-id',
                             'command': {'prompt_ref': 'prompt-memory:' + digest}}}}
    runner.api = Mock(side_effect=[{}, claim])
    runner.launch = Mock()
    runner.wait = Mock(return_value=0)
    runner.capture = Mock(side_effect=RuntimeError('candidate did not quiesce'))
    runner.cleanup = Mock()
    runner.complete = Mock()
    write_result_log(tmp_path / 'agent.jsonl', native_result())
    runner.run()
    result = runner.state['result']
    assert result['status'] == 'failed'
    assert 'did not quiesce' in result['error']
    usage = next(a['artifact'] for a in result['artifacts'] if a['artifact_type'] == 'runtime_host_usage')
    assert usage == {'model': 'scripted-model', 'input_tokens': 120, 'output_tokens': 30,
                     'cached_input_tokens': 80, 'total_tokens': 150}


def git(source, *args):
    return subprocess.run(['git', '-C', str(source), *args], check=True,
                          capture_output=True, text=True).stdout.strip()


def commit_source(source):
    git(source, 'add', '--all')
    git(source, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.test',
        '-c', 'core.hooksPath=/dev/null', 'commit', '--allow-empty', '-m', 'Fixture')
    return git(source, 'rev-parse', 'HEAD')


def snapshot_runner(tmp_path):
    from types import SimpleNamespace
    runner = host(tmp_path / 'state', 'new')
    runner.root.mkdir(mode=0o700)
    source = tmp_path / 'source'
    source.mkdir()
    git(source, 'init', '--template=')
    (source / 'original').write_text('original')
    runner.args = SimpleNamespace(workspace=source, base_commit=commit_source(source))
    runner.workspace = source.resolve()
    return runner, source


def prepare_snapshot(runner, destination):
    snapshot = runner.state['input_snapshot']
    subprocess.run([sys.executable, '-I', str(load().GIT_HANDOFF_SCRIPT), 'prepare',
                    str(runner.root / 'input.bundle'), snapshot['base_commit'],
                    snapshot['bundle_sha256'], str(destination)], check=True,
                   capture_output=True, text=True)


def test_frozen_source_preserves_bytes_and_executable_mode_after_original_changes(tmp_path):
    runner, source = snapshot_runner(tmp_path)
    executable = source / 'run.sh'
    executable.write_text('#!/bin/sh\necho original\n')
    executable.chmod(0o755)
    runner.args.base_commit = commit_source(source)
    runner.freeze_input()
    snapshot = runner.state['input_snapshot']
    executable.write_text('changed')
    (source / 'later.txt').write_text('not part of input')
    frozen = tmp_path / 'extracted-test'
    prepare_snapshot(runner, frozen)
    assert (frozen / 'run.sh').read_text() == '#!/bin/sh\necho original\n'
    assert (frozen / 'run.sh').stat().st_mode & 0o111 == 0o111
    assert not (frozen / 'later.txt').exists()
    assert git(frozen, 'rev-parse', 'HEAD') == runner.args.base_commit
    import hashlib
    assert hashlib.sha256((runner.root / 'input.bundle').read_bytes()).hexdigest() == snapshot['bundle_sha256']
    artifact = runner.result('failed', 'test')['artifacts'][0]
    assert artifact == {'artifact_type': 'supervised_input_snapshot', 'artifact': snapshot}


@pytest.mark.parametrize('kind', ['file_link', 'directory_link', 'fifo'])
def test_untracked_source_files_are_excluded_from_pinned_commit(tmp_path, kind):
    runner, source = snapshot_runner(tmp_path)
    if kind == 'fifo':
        os.mkfifo(source / 'untracked')
    else:
        (source / 'untracked').symlink_to(tmp_path if kind == 'directory_link' else __file__)
    runner.freeze_input()
    frozen = tmp_path / 'frozen'
    prepare_snapshot(runner, frozen)
    assert not (frozen / 'untracked').exists()
    assert (frozen / 'original').read_text() == 'original'


def test_tracked_source_symlink_is_rejected_before_agent_execution(tmp_path):
    runner, source = snapshot_runner(tmp_path)
    (source / 'link').symlink_to('original')
    runner.args.base_commit = commit_source(source)
    runner.freeze_input()
    with pytest.raises(subprocess.CalledProcessError) as failure:
        prepare_snapshot(runner, tmp_path / 'frozen')
    assert 'link' in failure.value.stderr


@pytest.mark.parametrize('kind', ['same', 'state_inside_source', 'source_inside_state'])
def test_source_and_state_must_not_overlap(tmp_path, kind):
    from types import SimpleNamespace
    module = load()
    source, state = tmp_path / 'source', tmp_path / 'state'
    if kind == 'same':
        state = source
    elif kind == 'state_inside_source':
        state = source / 'state'
    else:
        source = state / 'source'
    with pytest.raises(RuntimeError, match='must not overlap'):
        module.Host(SimpleNamespace(workspace=source, state_dir=state))
    assert not state.exists()


@pytest.mark.parametrize('phase', ['preparing', 'preparation_failed'])
def test_interrupted_snapshot_is_not_recreated_or_claimed(tmp_path, phase):
    runner = host(tmp_path, phase)
    runner.freeze_input = Mock()
    runner.api = Mock()
    with pytest.raises(RuntimeError, match='will not recopy or rerun'):
        runner.run()
    runner.freeze_input.assert_not_called()
    runner.api.assert_not_called()


def test_claiming_restart_reuses_snapshot_without_reading_source(tmp_path):
    runner = host(tmp_path, 'claiming')
    runner.name = 'test'
    runner.freeze_input = Mock(side_effect=AssertionError('must not recopy source'))
    runner.api = Mock(side_effect=RuntimeError('stop before claim'))
    with pytest.raises(RuntimeError, match='stop before claim'):
        runner.run()
    runner.freeze_input.assert_not_called()


def test_launch_mounts_only_frozen_input(tmp_path, monkeypatch):
    from types import SimpleNamespace
    module = load()
    runner = module.Host.__new__(module.Host)
    runner.root, runner.name, runner.state = tmp_path, 'owned', {}
    import hashlib
    (tmp_path / 'input.bundle').write_bytes(b'prepared bundle')
    runner.state['input_snapshot'] = {'base_commit': 'a' * 40, 'bundle_sha256': hashlib.sha256(b'prepared bundle').hexdigest()}
    runner.args = SimpleNamespace(workspace=tmp_path / 'mutable', auth_file=tmp_path / 'auth',
                                  synthetic_dns=False, proxy_image='proxy', image='agent')
    runner.start_observer = Mock()
    calls = []
    monkeypatch.setattr(module, 'docker', lambda *args: calls.append(args))
    runner.launch()
    candidate = next(c for c in calls if c[:5] == ('run', '-d', '--name', 'owned', '--network'))
    assert f'type=bind,src={tmp_path}/input.bundle,dst=/input.bundle,readonly' in candidate
    assert f'type=bind,src={module.GIT_HANDOFF_SCRIPT},dst=/git-handoff.py,readonly' in candidate
    assert not any(str(tmp_path / 'mutable') in arg for arg in candidate)


def test_failed_preparation_is_persisted_before_any_claim(tmp_path):
    from types import SimpleNamespace
    runner = host(tmp_path, 'new')
    request, submission, verifier = [tmp_path / name for name in ['request.json', 'submission.json', 'verify.py']]
    request.write_text('{}')
    submission.write_text('{}')
    verifier.write_text('pass\n')
    runner.args = SimpleNamespace(request=request, submission=submission, verifier=verifier)
    runner.freeze_input = Mock(side_effect=RuntimeError('input contains a FIFO'))
    runner.api = Mock()
    with pytest.raises(RuntimeError, match='FIFO'):
        runner.run()
    runner.api.assert_not_called()
    saved = json.loads((tmp_path / 'state.json').read_text())
    assert saved['phase'] == 'preparation_failed'
    assert saved['preparation_error'] == 'input contains a FIFO'


def test_workspace_alias_retargeting_does_not_change_canonical_source(tmp_path, monkeypatch):
    from types import SimpleNamespace
    module = load()
    source = tmp_path / 'source'
    source.mkdir()
    (source / 'original').write_text('original')
    git(source, 'init', '--template=')
    base = commit_source(source)
    alias = tmp_path / 'alias'
    alias.symlink_to(source, target_is_directory=True)
    state = tmp_path / 'private'
    args = SimpleNamespace(workspace=alias, state_dir=state, server_url='http://localhost',
                           image='image', verifier_image='verifier', proxy_image='proxy',
                           model='model', timeout=30, synthetic_dns=False, base_commit=base)
    monkeypatch.setenv('HARNESS_API_TOKEN', 'test-only')
    runner = module.Host(args)
    try:
        alias.unlink()
        alias.symlink_to(state, target_is_directory=True)
        runner.freeze_input()
        assert runner.state['identity']['workspace'] == str(source.resolve())
        frozen = tmp_path / 'frozen'
        prepare_snapshot(runner, frozen)
        assert (frozen / 'original').read_text() == 'original'
        assert not (frozen / 'input.bundle').exists()
        assert runner.state['identity']['base_commit'] == base
    finally:
        runner.lock.close()


def test_launch_passes_prepared_digest_to_consumer_before_auth_copy(tmp_path, monkeypatch):
    from types import SimpleNamespace
    runner, _ = snapshot_runner(tmp_path)
    runner.freeze_input()
    expected = runner.state['input_snapshot']['bundle_sha256']
    (runner.root / 'input.bundle').write_bytes(b'changed bytes')
    runner.name = 'owned'
    runner.args = SimpleNamespace(auth_file=tmp_path / 'auth', image='image',
                                  proxy_image='proxy', synthetic_dns=False)
    runner.start_observer = Mock()
    calls = []
    def docker(*args):
        calls.append(args)
        if args[0] == 'exec':
            assert args[2:6] == ('python3', '-I', '/git-handoff.py', 'prepare')
            assert args[-2:] == (expected, '/workspace')
            assert args[-3] == runner.state['input_snapshot']['base_commit']
            raise RuntimeError('consumer rejected input digest')
        return ''
    monkeypatch.setitem(runner.launch.__globals__, 'docker', docker)
    with pytest.raises(RuntimeError, match='consumer rejected'):
        runner.launch()
    assert not any('cp /run/codex-auth' in argument for call in calls for argument in call)


def test_prepare_checks_mounted_bytes_before_creating_candidate_files(tmp_path):
    runner, _ = snapshot_runner(tmp_path)
    runner.freeze_input()
    (runner.root / 'input.bundle').write_bytes(b'changed after host check')
    workspace = tmp_path / 'workspace'
    with pytest.raises(subprocess.CalledProcessError) as failure:
        prepare_snapshot(runner, workspace)
    assert 'digest' in failure.value.stderr
    assert not workspace.exists()


def test_wait_uses_only_persisted_server_prompt(tmp_path, monkeypatch):
    from types import SimpleNamespace
    runner = host(tmp_path, 'executing')
    runner.name = 'test'
    runner.args = SimpleNamespace(model='model', timeout=20)
    runner.state.update(request={'prompt': 'raw task'}, prepared_prompt=prepared_prompt())
    stream = Mock(return_value=0)
    monkeypatch.setitem(runner.wait.__globals__, 'stream_agent_output', stream)
    assert runner.wait() == 0
    command = stream.call_args.args[0]
    assert command[-1] == prepared_prompt()['prompt']
    assert 'raw task' not in command


def test_native_payloads_are_preserved_without_encoding(tmp_path):
    runner = host(tmp_path, 'executing')
    native = native_result()
    write_result_log(tmp_path / 'agent.jsonl', native)
    parsed = runner.wait.__globals__['read_activity_result'](tmp_path / 'agent.jsonl', 'modify')
    result = runner.result(parsed['status'], parsed['summary'], [{'artifact_type': 'host', 'artifact': {}}], parsed)
    assert result['signals'] == native['signals']
    assert result['validation'] == native['validation']
    assert result['artifacts'][0] == native['artifacts'][0]
    assert result['summary'] == native['summary']


@pytest.mark.parametrize('kind', ['missing', 'duplicate', 'tool_output', 'wrong_activity', 'malformed', 'wrong_arrays'])
def test_invalid_native_results_fail_clearly(tmp_path, kind):
    module = load()
    log = tmp_path / 'agent.jsonl'
    result = native_result()
    if kind == 'wrong_activity':
        result['activity'] = 'different'
    if kind == 'wrong_arrays':
        result['signals'] = 'encoded'
    write_result_log(log, result)
    text = log.read_text()
    if kind == 'missing':
        text = '{"type":"turn.completed","usage":{}}'
    elif kind == 'duplicate':
        text *= 2
    elif kind == 'tool_output':
        text = text.replace('agent_message', 'command_execution')
    elif kind == 'malformed':
        text = text.replace('```harness-activity-result\\n{', '```harness-activity-result\\ninvalid{')
    log.write_text(text)
    with pytest.raises((RuntimeError, ValueError)):
        module.read_activity_result(log, 'modify')


def claimed_runner(tmp_path):
    import hashlib
    from types import SimpleNamespace
    runner = host(tmp_path, 'claiming')
    runner.name = 'test'
    runner.args = SimpleNamespace(model='model', image='image', verifier_image='verifier')
    runner.state.update(request={'project': str(tmp_path), 'prompt': 'task'},
                        submission={'task_id': 'task-id', 'workflow_id': 'workflow-id'})
    digest = hashlib.sha256(b'\0'.join(x.encode() for x in [str(tmp_path), '', 'task-id', 'task'])).hexdigest()
    claim = {'claimed': True, 'prepared_prompt': prepared_prompt(), **runner.state['lease'],
             'runtime_job': {'id': 'job', 'input': {'activity': 'modify', 'workflow_id': 'workflow-id',
                             'command': {'prompt_ref': 'prompt-memory:' + digest}}}}
    runner.api = Mock(side_effect=[{}, claim])
    runner.launch = Mock()
    runner.wait = Mock(return_value=0)
    runner.capture = Mock(side_effect=lambda: runner.state.update(git_handoff={
        'base_commit': 'a' * 40, 'candidate_commit': 'b' * 40,
        'bundle_sha256': 'c' * 64, 'verified': False}))
    runner.verify = Mock(return_value=0)
    runner.collect_resources = Mock()
    runner.cleanup = Mock()
    runner.complete = Mock()
    (tmp_path / 'candidate.tar').write_bytes(b'candidate')
    (tmp_path / 'verifier.py').write_text('pass')
    return runner, claim


def test_cumulative_cpu_budget_is_cgroup_usage_not_a_rate_or_wall_timeout():
    module = load()
    limits = module.quality_gate.trusted_resource_limits({
        "resource_limits": {
            "requested": {"cpu_time_secs": 2},
            "effective": {
                "cpu_time_secs": 1, "memory_bytes": 8, "pids": 16,
                "disk_bytes": 512 * 1024 * 1024, "output_bytes": 1024, "wall_time_secs": 30,
            },
        }
    })
    assert module.quality_gate.cpu_time_exceeded(1_000_000, 1) is False
    assert module.quality_gate.cpu_time_exceeded(1_000_001, 1) is True
    report = module.quality_gate.build_resource_report(
        limits, cpu_usec=1_000_001, peak_memory_bytes=1, peak_pids=1,
        disk_bytes=1, output_bytes=1, wall_time_millis=1,
    )
    assert report["termination"]["resource"] == "cpu_time"
    assert report["usage"]["cpu_time_millis"] == 1000
    assert report["usage"]["wall_time_millis"] == 1
    variables = module.quality_gate.bind_eval_contract(
        {
            "resource_limits": limits,
            "network_policy": {"inbound": "deny", "outbound": "allowlist", "network_allowlist": ["chatgpt.com"]},
            "credential_environment_variables": {"MODEL_TOKEN": "secret"},
        },
        {"command": {"eval": {}}},
    )
    assert variables == {"MODEL_TOKEN": "secret"}
    assert "secret" not in json.dumps({key: value for key, value in limits.items()})


def test_root_eval_contract_is_enforced():
    module = load()
    claim = {
        "resource_limits": {"requested": {}, "effective": {
            "cpu_time_secs": 1, "memory_bytes": 8, "pids": 16,
            "disk_bytes": 512 * 1024 * 1024, "output_bytes": 1024,
            "wall_time_secs": 30,
        }},
        "network_policy": {"inbound": "deny", "outbound": "deny", "network_allowlist": []},
        "credential_environment_variables": {"MODEL_TOKEN": "private-value"},
    }
    assert module.quality_gate.bind_eval_contract(claim, {"eval": {}, "command": {}}) == {
        "MODEL_TOKEN": "private-value"
    }
    with pytest.raises(RuntimeError, match="trusted_eval_verifier_v1"):
        module.quality_gate.bind_eval_contract(
            claim, {"eval": {"required_runtime_host_capabilities": ["trusted_eval_verifier_v1"]}}
        )


def test_eval_credentials_enter_container_over_stdin_only(tmp_path, monkeypatch):
    from types import SimpleNamespace
    module = load()
    runner = host(tmp_path, 'executing')
    runner.args = SimpleNamespace(timeout=30, model='model')
    runner.name = 'candidate'
    runner.credential_variables = {"MODEL_TOKEN": "private-value"}
    runner.state.update(
        enforced_limits={"effective": {
            "wall_time_secs": 30, "cpu_time_secs": 10, "output_bytes": 1024,
        }},
        prepared_prompt={"prompt": "task"},
    )
    runner.persist = Mock()
    runner.renew = Mock()
    observed = {}

    def stream(command, root, timeout, renew, **kwargs):
        observed.update(command=command, kwargs=kwargs)
        return 0

    monkeypatch.setitem(runner.wait.__globals__, 'stream_agent_output', stream)
    assert runner.wait() == 0
    assert 'private-value' not in ' '.join(observed['command'])
    assert observed['command'][:3] == ['docker', 'exec', '-i']
    assert json.loads(observed['kwargs']['stdin_data']) == runner.credential_variables
    assert runner.state['wall_time_millis'] >= 0


def test_failed_eval_without_measurements_does_not_invent_evidence(tmp_path):
    runner = host(tmp_path, 'executing')
    runner.state.update(
        enforced_limits={"effective": {}},
        enforced_network_policy={"inbound": "deny", "outbound": "deny", "network_allowlist": []},
        input_snapshot={"base_commit": "a" * 40},
    )
    failed = runner.result('failed', 'container launch failed')
    assert failed['error_kind'] == 'unknown'
    assert 'execution_evidence' not in runner.state
    assert not any(item['artifact_type'] in {'resource_limit_report', 'network_policy_report'}
                   for item in failed['artifacts'])


def test_rendered_claim_and_native_result_survive_completion_and_restart(tmp_path):
    runner, claim = claimed_runner(tmp_path)
    native = native_result()
    write_result_log(tmp_path / 'agent.jsonl', native)
    runner.run()
    assert runner.api.call_args_list[1].args[1]['execution_workspace'] == '/workspace'
    assert runner.api.call_args_list[0].args[1]['capabilities'] == [
        'runtime_job_lease_proof_v1', 'eval_resource_limits', 'eval_network_policy',
    ]
    saved = json.loads((tmp_path / 'state.json').read_text())
    assert saved['prepared_prompt'] == claim['prepared_prompt']
    assert saved['agent_result'] == native
    assert saved['result']['signals'] == native['signals']
    assert saved['git_handoff']['verified'] is True
    evidence = next(a['artifact'] for a in saved['result']['artifacts']
                    if a['artifact_type'] == 'supervised_git_handoff')
    assert evidence == saved['git_handoff']
    runner.verify.assert_called_once()
    runner.launch.reset_mock()
    runner.run()
    runner.launch.assert_not_called()


@pytest.mark.parametrize('defect', ['missing', 'schema', 'digest'])
def test_invalid_prepared_prompt_fails_before_launch(tmp_path, defect):
    runner, claim = claimed_runner(tmp_path)
    if defect == 'missing':
        del claim['prepared_prompt']
    elif defect == 'schema':
        claim['prepared_prompt']['activity_result_schema']['activity'] = 'other'
    else:
        claim['prepared_prompt']['prompt_packet_digest'] = ''
    runner.run()
    runner.launch.assert_not_called()
    assert runner.state['result']['status'] == 'failed'
    runner.complete.assert_called_once()


@pytest.mark.parametrize('status', ['failed', 'blocked', 'cancelled'])
def test_non_success_result_is_not_promoted_by_zero_exit(tmp_path, status):
    runner, _ = claimed_runner(tmp_path)
    native = native_result(status)
    native.update(error='cannot proceed', error_kind='external_dependency')
    write_result_log(tmp_path / 'agent.jsonl', native)
    runner.run()
    assert runner.state['result']['status'] == status
    assert runner.state['result']['error'] == 'cannot proceed'
    assert runner.state['result']['error_kind'] == 'external_dependency'
    runner.verify.assert_not_called()
    runner.collect_resources.assert_called_once_with(stop_agents=True)


def test_verifier_failure_does_not_forward_success_signal(tmp_path):
    runner, _ = claimed_runner(tmp_path)
    runner.verify.return_value = 1
    write_result_log(tmp_path / 'agent.jsonl', native_result())
    runner.run()
    assert runner.state['result']['status'] == 'failed'
    assert runner.state['result']['signals'] == []
    assert 'verifier rejected' in runner.state['result']['error']
    assert runner.state['git_handoff']['verified'] is False
    assert runner.state['agent_result']['signals']


@pytest.mark.parametrize('artifact_type', [
    'runtime_host_usage', 'supervised_input_snapshot',
    'supervised_candidate_resources', 'supervised_docker_verification', 'supervised_git_handoff',
])
def test_native_artifacts_cannot_impersonate_host_evidence(tmp_path, artifact_type):
    runner, _ = claimed_runner(tmp_path)
    native = native_result()
    forged = {'artifact_type': artifact_type, 'artifact': {'forged': True}}
    native['artifacts'].insert(0, forged)
    write_result_log(tmp_path / 'agent.jsonl', native)
    runner.run()
    result = runner.state['result']
    assert result['status'] == 'failed'
    assert 'host-owned artifact: ' + artifact_type in result['error']
    assert result['signals'] == []
    assert forged not in result['artifacts']
    usage = [a['artifact'] for a in result['artifacts'] if a['artifact_type'] == 'runtime_host_usage']
    assert len(usage) == 1
    assert usage[0]['input_tokens'] == 120
    saved = json.loads((tmp_path / 'state.json').read_text())
    assert saved['agent_result'] == native
    runner.capture.assert_not_called()
    runner.verify.assert_not_called()
    runner.complete.assert_called_once()


def test_verifier_reconstructs_pinned_bundle_offline_before_running_verifier(tmp_path, monkeypatch):
    from types import SimpleNamespace
    module = load()
    runner = host(tmp_path, 'executing')
    runner.name = 'owned'
    runner.args = SimpleNamespace(image='pinned-image', verifier_image='pinned-verifier')
    runner.renew = Mock()
    handoff = {'base_commit': 'a' * 40, 'candidate_commit': 'b' * 40,
               'bundle_sha256': 'c' * 64, 'verified': False}
    runner.state['git_handoff'] = handoff
    calls = []

    def docker(*args):
        calls.append(args)
        if args[0] == 'inspect':
            return json.dumps({'Running': False, 'ExitCode': 0})
        return 'verified output'

    monkeypatch.setitem(runner.verify.__globals__, 'docker', docker)
    assert runner.verify() == 0
    command = calls[0]
    assert command[:7] == ('run', '-d', '--name', 'owned-verify', '--network', 'none', '--read-only')
    assert 'pinned-verifier' in command
    assert 'pinned-image' not in command
    assert '--entrypoint' in command and command[command.index('--entrypoint') + 1] == 'python3'
    assert any('dst=/config.toml,readonly' in arg for arg in command)
    assert f'type=bind,src={tmp_path}/candidate,dst=/handoff,readonly' in command
    assert f'type=bind,src={module.GIT_HANDOFF_SCRIPT},dst=/git-handoff.py,readonly' in command
    assert any(arg.startswith('/candidate:rw,') for arg in command)
    assert not any('auth' in arg for arg in command)
    assert command[-3:] == (handoff['base_commit'], handoff['candidate_commit'], handoff['bundle_sha256'])
    script = command[command.index('-c') + 1]
    events = []
    monkeypatch.setattr(sys, 'argv', ['-c', *command[-3:]])
    monkeypatch.setattr(subprocess, 'run', lambda args, **kw: events.append(('reconstruct', args, kw)))
    monkeypatch.setattr(subprocess, 'call', lambda args: events.append(('verify', args)) or 0)
    with pytest.raises(SystemExit) as outcome:
        exec(script, {})
    assert outcome.value.code == 0
    assert events == [
        ('reconstruct', ['python3', '-I', '/git-handoff.py', 'verify', '/handoff/candidate.bundle',
                         *command[-3:], '/handoff/workspace', '/candidate'], {'check': True}),
        ('verify', ['python3', '-I', '/verify.py', '/candidate']),
    ]
    events.clear()
    monkeypatch.setattr(subprocess, 'run', Mock(side_effect=subprocess.CalledProcessError(1, 'reconstruct')))
    with pytest.raises(subprocess.CalledProcessError):
        exec(script, {})
    assert not events
    assert (tmp_path / 'verifier.stdout').read_text() == 'verified output'
    assert handoff['verified'] is False


def quality_gate_module():
    path = Path(__file__).resolve().parents[1] / 'scripts/supervised_quality_gate.py'
    spec = importlib.util.spec_from_file_location('supervised_quality_gate', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def seed_retained_candidate(tmp_path, candidate='b' * 40):
    candidate_dir = tmp_path / 'candidate'
    workspace = candidate_dir / 'workspace'
    workspace.mkdir(parents=True)
    (workspace / 'ok').write_text('ok\n')
    bundle = candidate_dir / 'candidate.bundle'
    bundle.write_bytes(b'candidate-bundle-bytes')
    digest = __import__('hashlib').sha256(bundle.read_bytes()).hexdigest()
    handoff = {
        'base_commit': 'a' * 40,
        'candidate_commit': candidate,
        'bundle_sha256': digest,
        'verified': True,
    }
    return handoff, bundle


def test_prepare_follow_on_claim_clears_execution_evidence_and_refuses_completion(tmp_path):
    runner = host(tmp_path, 'new')
    runner.persist = Mock()
    runner.state['execution_evidence'] = {
        'checked_out_commit': 'a' * 40,
        'usage': {'total_tokens': 9},
    }
    runner.state['job'] = {'id': 'old-job'}
    runner.prepare_follow_on_claim()
    assert 'execution_evidence' not in runner.state
    assert 'job' not in runner.state
    assert runner.state['phase'] == 'claiming'
    (tmp_path / 'completion.json').write_text('{"completed":true}\n')
    with pytest.raises(RuntimeError, match='refusing to mutate a completed run'):
        runner.prepare_follow_on_claim()
    assert (tmp_path / 'completion.json').read_text() == '{"completed":true}\n'


def test_fresh_state_consumes_copied_candidate_without_mutating_prior_completed_run(tmp_path):
    """Supported handoff: copy candidate/ into a fresh state dir; leave prior run intact."""
    import hashlib
    import shutil
    from types import SimpleNamespace

    prior = tmp_path / 'prior'
    fresh = tmp_path / 'fresh'
    prior.mkdir()
    fresh.mkdir()
    handoff, bundle = seed_retained_candidate(prior)
    (prior / 'candidate' / 'revision.json').write_text(json.dumps({
        'base_commit': handoff['base_commit'],
        'candidate_commit': handoff['candidate_commit'],
    }))
    prior_state = {
        'host_id': 'prior-host',
        'phase': 'completed',
        'git_handoff': handoff,
        'job': {'id': 'impl-job', 'input': {'activity': 'modify'}},
        'lease': {'lease_generation': 1, 'lease_expires_at': 'old', 'lease_proof': 'old'},
        'result': {'status': 'succeeded'},
        'execution_evidence': {
            'checked_out_commit': 'f' * 40,
            'usage': {'total_tokens': 42},
        },
    }
    (prior / 'state.json').write_text(json.dumps(prior_state, indent=2))
    (prior / 'completion.json').write_text(json.dumps({'completed': True}))
    (prior / 'completion-response.json').write_text(json.dumps({'completed': True}))
    prior_snapshot = {
        path.relative_to(prior).as_posix(): path.read_bytes()
        for path in prior.rglob('*') if path.is_file()
    }

    shutil.copytree(prior / 'candidate', fresh / 'candidate')
    request_path = fresh / 'request.json'
    submission_path = fresh / 'submission.json'
    verifier_path = fresh / 'operator-verifier.py'
    request_path.write_text(json.dumps({'project': str(fresh), 'prompt': 'qg'}))
    submission_path.write_text(json.dumps({'task_id': 'qg-task', 'workflow_id': 'workflow-id'}))
    verifier_path.write_text('pass\n')

    runner = host(fresh, 'new')
    runner.name = 'fresh-host'
    runner.args = SimpleNamespace(
        base_commit=handoff['base_commit'],
        model='model',
        image='image',
        verifier_image='verifier',
        request=request_path,
        submission=submission_path,
        verifier=verifier_path,
    )
    runner.persist = lambda: (fresh / 'state.json').write_text(json.dumps(runner.state, indent=2))
    expected = handoff['candidate_commit']
    command = {
        'activity': 'run_quality_gate',
        'expected_head_sha': expected,
        'validation_commands_argv': [['python3', '-I', '/trusted/verify.py', '/candidate']],
    }
    claim = {
        'claimed': True,
        'lease_generation': 2,
        'lease_expires_at': 'new-expiry',
        'lease_proof': 'new-proof',
        'runtime_job': {
            'id': 'quality-job',
            'input': {
                'activity': 'run_quality_gate',
                'workflow_id': 'workflow-id',
                'command': command,
            },
        },
    }
    runner.api = Mock(side_effect=[{}, claim])
    runner.launch = Mock(side_effect=AssertionError('must not launch agent'))
    runner.wait = Mock(side_effect=AssertionError('must not wait on agent'))
    validation = [{
        'argv': command['validation_commands_argv'][0],
        'exit_code': 0,
        'output_sha256': 'd' * 64,
        'duration_ms': 3,
    }]
    runner.verify_expected_head = Mock(return_value=validation)
    runner.cleanup = Mock()
    runner.complete = Mock(side_effect=lambda: runner.state.update(phase='completed') or
                           setattr(runner, '_completion', {
                               **{k: runner.state['lease'][k] for k in
                                  ('lease_generation', 'lease_expires_at', 'lease_proof')},
                               'result': runner.state['result'],
                               'execution_evidence': runner.state['execution_evidence'],
                           }))
    # Stale evidence must not survive into the fresh claim even if present briefly.
    runner.state['execution_evidence'] = {'checked_out_commit': '0' * 40, 'usage': {'total_tokens': 99}}

    runner.run()

    for rel, content in prior_snapshot.items():
        assert (prior / rel).read_bytes() == content
    assert json.loads((prior / 'state.json').read_text())['phase'] == 'completed'
    assert json.loads((prior / 'state.json').read_text())['execution_evidence']['usage']['total_tokens'] == 42
    runner.launch.assert_not_called()
    assert runner.state['result']['status'] == 'succeeded'
    assert runner.state['execution_evidence']['checked_out_commit'] == expected
    assert runner.state['execution_evidence']['usage']['total_tokens'] == 0
    assert runner._completion['execution_evidence']['checked_out_commit'] == expected
    assert runner.state['git_handoff']['bundle_sha256'] == hashlib.sha256(bundle.read_bytes()).hexdigest()


def test_bind_rejects_local_candidate_that_is_not_expected_head():
    gate = quality_gate_module()
    with pytest.raises(RuntimeError, match='not an externally verified PR head'):
        gate.bind_retained_candidate({'candidate_commit': 'a' * 40}, 'b' * 40)


def test_native_quality_gate_claim_binds_expected_head_and_submits_evidence(tmp_path):
    runner, _ = claimed_runner(tmp_path)
    handoff, bundle = seed_retained_candidate(tmp_path)
    runner.state['git_handoff'] = handoff
    (tmp_path / 'verifier.py').write_text('pass\n')
    expected = handoff['candidate_commit']
    command = {
        'activity': 'run_quality_gate',
        'expected_head_sha': expected,
        'validation_commands_argv': [['python3', '-I', '/trusted/verify.py', '/candidate']],
    }
    claim = {
        'claimed': True,
        **runner.state['lease'],
        'runtime_job': {
            'id': 'quality-job',
            'input': {
                'activity': 'run_quality_gate',
                'workflow_id': 'workflow-id',
                'command': command,
            },
        },
    }
    runner.api = Mock(side_effect=[{}, claim])
    runner.launch = Mock(side_effect=AssertionError('must not launch agent'))
    runner.wait = Mock(side_effect=AssertionError('must not wait on agent'))
    validation = [{
        'argv': command['validation_commands_argv'][0],
        'exit_code': 0,
        'output_sha256': 'd' * 64,
        'duration_ms': 3,
    }]
    runner.verify_expected_head = Mock(return_value=validation)
    runner.cleanup = Mock()
    runner.complete = Mock(side_effect=lambda: runner.state.update(phase='completed') or
                           setattr(runner, '_completion', {
                               **runner.state['lease'],
                               'result': runner.state['result'],
                               'execution_evidence': runner.state['execution_evidence'],
                           }))
    # claimed_runner leaves phase claiming; keep retained candidate and skip freeze.
    runner.state['phase'] = 'claiming'
    runner.state['input_snapshot'] = {'base_commit': 'a' * 40, 'bundle_sha256': 'e' * 64}
    runner.run()
    runner.launch.assert_not_called()
    runner.verify_expected_head.assert_called_once_with(
        expected, [command['validation_commands_argv'][0]]
    )
    assert runner.state['result']['status'] == 'succeeded'
    assert runner.state['result']['signals'][0]['signal_type'] == 'QualityPassed'
    evidence = runner.state['execution_evidence']
    assert evidence['checked_out_commit'] == expected
    assert evidence['validation'] == validation
    assert evidence['usage']['total_tokens'] == 0
    assert evidence['isolation_cleanup_status'] == 'cleaned'
    assert runner._completion['execution_evidence']['checked_out_commit'] == expected
    handoff_artifact = next(
        a['artifact'] for a in runner.state['result']['artifacts']
        if a['artifact_type'] == 'supervised_git_handoff'
    )
    assert handoff_artifact['candidate_commit'] == expected
    assert handoff_artifact['verified'] is True
    assert not any(
        a['artifact_type'] == 'supervised_candidate_resources'
        for a in runner.state['result']['artifacts']
    )
    assert evidence['resource_limit_report']['usage'] == {}
    assert 'eval_resource_limits not advertised' in evidence['resource_limit_report']['reason']


def test_native_quality_gate_rejects_mismatched_expected_head(tmp_path):
    runner, _ = claimed_runner(tmp_path)
    handoff, _ = seed_retained_candidate(tmp_path, candidate='b' * 40)
    runner.state.update(phase='claiming', git_handoff=handoff,
                        input_snapshot={'base_commit': 'a' * 40, 'bundle_sha256': 'e' * 64})
    (tmp_path / 'verifier.py').write_text('pass\n')
    claim = {
        'claimed': True,
        **runner.state['lease'],
        'runtime_job': {
            'id': 'quality-job',
            'input': {
                'activity': 'run_quality_gate',
                'workflow_id': 'workflow-id',
                'command': {
                    'expected_head_sha': 'c' * 40,
                    'validation_commands_argv': [['python3', '-I', '/verify.py', '/candidate']],
                },
            },
        },
    }
    runner.api = Mock(side_effect=[{}, claim])
    runner.launch = Mock(side_effect=AssertionError('must not launch'))
    runner.cleanup = Mock()
    runner.complete = Mock()
    runner.run()
    assert runner.state['result']['status'] == 'failed'
    assert 'not an externally verified PR head' in runner.state['result']['error']
    runner.launch.assert_not_called()
    runner.complete.assert_called_once()


def test_verify_expected_head_uses_helper_verify_and_server_argv(tmp_path, monkeypatch):
    from types import SimpleNamespace
    module = load()
    runner = host(tmp_path, 'executing')
    runner.name = 'owned'
    runner.args = SimpleNamespace(image='pinned-image', verifier_image='pinned-verifier')
    runner.renew = Mock()
    handoff, _ = seed_retained_candidate(tmp_path)
    runner.state['git_handoff'] = handoff
    (tmp_path / 'verifier.py').write_text('pass\n')
    expected = handoff['candidate_commit']
    commands = [['python3', '-I', '/trusted/verify.py', '/candidate']]
    out = tmp_path / 'quality-gate-out'
    calls = []

    def docker(*args):
        calls.append(args)
        if args[0] == 'run':
            out.mkdir(exist_ok=True)
            (out / 'validation.json').write_text(json.dumps([{
                'argv': commands[0], 'exit_code': 0,
                'output_sha256': 'f' * 64, 'duration_ms': 1,
            }]))
            return 'cid'
        if args[0] == 'inspect':
            return json.dumps({'Running': False, 'ExitCode': 0})
        return 'ok'

    monkeypatch.setitem(runner.verify_expected_head.__globals__, 'docker', docker)
    assert runner.verify_expected_head(expected, commands)[0]['exit_code'] == 0
    command = calls[0]
    assert command[:7] == ('run', '-d', '--name', 'owned-verify', '--network', 'none', '--read-only')
    assert 'pinned-verifier' in command
    assert 'pinned-image' not in command
    assert '--entrypoint' in command and command[command.index('--entrypoint') + 1] == 'python3'
    assert any('dst=/config.toml,readonly' in arg for arg in command)
    assert f'type=bind,src={tmp_path}/candidate,dst=/handoff,readonly' in command
    assert any('dst=/trusted/verify.py,readonly' in arg for arg in command)
    script = command[command.index('-c') + 1]
    assert '/git-handoff.py' in script and 'verify' in script
    payload = json.loads(command[-1])
    assert payload['candidate'] == expected
    assert payload['commands'] == commands


def test_cli_requires_distinct_pinned_verifier_image(monkeypatch, capsys):
    module = load()
    image = 'sha256:' + 'a' * 64
    argv = [
        'run-supervised-docker-host.py',
        '--server-url', 'http://127.0.0.1:1',
        '--request', '/tmp/r.json',
        '--submission', '/tmp/s.json',
        '--workspace', '/tmp/ws',
        '--verifier', '/tmp/v.py',
        '--auth-file', '/tmp/a.json',
        '--state-dir', '/tmp/st',
        '--base-commit', 'a' * 40,
        '--image', image,
        '--verifier-image', image,
        '--proxy-image', 'sha256:' + 'b' * 64,
    ]
    monkeypatch.setattr(sys, 'argv', argv)
    with pytest.raises(SystemExit) as outcome:
        module.main()
    assert outcome.value.code == 2
    err = capsys.readouterr().err.lower()
    assert 'differ' in err


def _docker_image_id(reference: str) -> str | None:
    try:
        result = subprocess.run(
            ['docker', 'images', '--no-trunc', '--format', '{{.ID}}', reference],
            capture_output=True, text=True, timeout=30, check=False,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if result.returncode or not result.stdout.strip():
        return None
    image_id = result.stdout.strip().splitlines()[0]
    return image_id if image_id.startswith('sha256:') else None


def test_docker_tmpfs_statvfs_enospc_and_cleanup_with_fixture_caps():
    """Credential-free probe: measure allocation, hit ENOSPC, clean owned container."""
    module = load()
    image = _docker_image_id('harness-agent:fixture')
    if image is None:
        pytest.skip('pinned local harness-agent:fixture image unavailable')
    uid, gid = os.getuid(), os.getgid()
    name = f'tmpfs-disk-test-{os.getpid()}-{os.getuid()}'
    argv = [
        'run', '-d', '--name', name, '--network', 'none',
        '--read-only', '--user', f'{uid}:{gid}', '--cap-drop', 'ALL',
        '--security-opt', 'no-new-privileges', '--pids-limit', '32',
        '--memory', '256m', '--memory-swap', '256m', '--cpus', '0.5',
        '--shm-size', '4m',
        '--tmpfs', f'/home/harness:rw,nosuid,nodev,size=2m,uid={uid},gid={gid},mode=700',
        '--tmpfs', '/tmp:rw,nosuid,nodev,size=2m',
        '--tmpfs', f'/workspace:rw,nosuid,nodev,size=2m,uid={uid},gid={gid},mode=700',
        image, 'sleep', '90',
    ]
    try:
        module.docker(*argv)
        before = json.loads(module.docker('exec', name, 'python3', '-I', '-c', module.DISK_METRICS_SCRIPT))
        assert before['sample_kind'] == 'terminal'
        assert before['distinct_filesystems'] == 4
        alloc = json.loads(module.docker('exec', name, 'python3', '-I', '-c', """
import json, os
from pathlib import Path
before = os.statvfs('/tmp')
Path('/tmp/known.bin').write_bytes(b'a' * (256 * 1024))
os.sync()
after = os.statvfs('/tmp')
print(json.dumps({
  'payload_bytes': 256 * 1024,
  'delta_used': ((after.f_blocks - after.f_bfree) - (before.f_blocks - before.f_bfree)) * after.f_frsize,
}))
"""))
        assert alloc['delta_used'] >= alloc['payload_bytes']
        enospc = json.loads(module.docker('exec', name, 'python3', '-I', '-c', """
import errno, json, os
from pathlib import Path
def fill(root):
    path = Path(root) / 'fill.bin'
    wrote = 0
    err = None
    try:
        with path.open('wb') as fh:
            while True:
                fh.write(b'x' * 65536); fh.flush(); os.fsync(fh.fileno()); wrote += 65536
    except OSError as exc:
        err = {'enospc': exc.errno == errno.ENOSPC, 'errno': exc.errno}
    st = os.statvfs(root)
    return {'wrote': wrote, 'error': err,
            'used': (st.f_blocks - st.f_bfree) * st.f_frsize,
            'capacity': st.f_blocks * st.f_frsize}
print(json.dumps({'tmp': fill('/tmp'), 'shm': fill('/dev/shm')}))
"""))
        for key in ('tmp', 'shm'):
            assert enospc[key]['error']['enospc']
            assert enospc[key]['used'] <= enospc[key]['capacity']
            assert enospc[key]['used'] >= int(enospc[key]['capacity'] * 0.9)
        after = json.loads(module.docker('exec', name, 'python3', '-I', '-c', module.DISK_METRICS_SCRIPT))
        assert module.disk_evidence_errors(after) == []
    finally:
        subprocess.run(['docker', 'rm', '-f', name], capture_output=True, text=True, timeout=30)
        left = subprocess.run(
            ['docker', 'ps', '-aq', '--filter', f'name=^/{name}$'],
            capture_output=True, text=True, timeout=30,
        )
        assert left.stdout.strip() == ''
