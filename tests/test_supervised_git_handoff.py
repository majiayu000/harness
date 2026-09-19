"""Real Git boundary tests for the supervised revision transfer helper."""
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile

import pytest

SPEC = importlib.util.spec_from_file_location('handoff', Path(__file__).parents[1] / 'scripts/supervised_git_handoff.py')
handoff = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(handoff)


def git(path, *args):
    result = subprocess.run(['git', '-C', str(path), '-c', 'user.name=Test',
                             '-c', 'user.email=test@example.invalid', *args],
                            env=handoff.environment(), capture_output=True, text=True, check=True)
    return result.stdout.strip()


@pytest.fixture
def repository(tmp_path):
    source = tmp_path / 'source'
    source.mkdir()
    git(source, 'init', '--template=')
    (source / 'old').write_text('base\n')
    (source / '.gitignore').write_text('ignored\n')
    git(source, 'add', '.')
    git(source, 'commit', '-m', 'base')
    base = git(source, 'rev-parse', 'HEAD')
    (source / 'gold').write_text('future gold')
    git(source, 'add', '.')
    git(source, 'commit', '-m', 'future')
    future = git(source, 'rev-parse', 'HEAD')
    bundle = tmp_path / 'base.bundle'
    frozen = handoff.freeze(source, base, bundle)
    workspace = tmp_path / 'workspace'
    handoff.prepare(bundle, base, frozen['bundle_sha256'], workspace)
    return source, base, future, bundle, workspace


def unpack(workspace, base, tmp_path):
    output = io.BytesIO()
    handoff.export(workspace, base, output)
    output.seek(0)
    root = tmp_path / 'exported'
    root.mkdir()
    with tarfile.open(fileobj=output) as archive:
        archive.extractall(root, filter='data')
    return root, json.loads((root / 'revision.json').read_text())


def test_base_only_and_successful_candidate_reconstruction(repository, tmp_path):
    source, base, future, bundle, workspace = repository
    assert git(source, 'rev-parse', 'HEAD') == future
    assert not (workspace / 'gold').exists()
    with pytest.raises(RuntimeError):
        handoff.commit(workspace / '.git', future)
    (workspace / 'old').unlink()
    (workspace / 'run').write_text('#!/bin/sh\necho hello\n')
    (workspace / 'run').chmod(0o755)
    (workspace / '.gitattributes').write_text('run export-ignore\nraw export-subst\n')
    (workspace / 'raw').write_text('$Format:%H$\n')
    git(workspace, 'add', '-A')
    git(workspace, 'commit', '-m', 'candidate')
    root, revision = unpack(workspace, base, tmp_path)
    candidate = git(workspace, 'rev-parse', 'HEAD')
    assert revision == {'base_commit': base, 'candidate_commit': candidate}
    destination = tmp_path / 'verified'
    handoff.verify(root / 'candidate.bundle', base, candidate,
                   handoff.digest(root / 'candidate.bundle'), root / 'workspace', destination)
    assert (destination / 'raw').read_text() == '$Format:%H$\n'
    assert (destination / 'run').stat().st_mode & 0o111
    assert not (destination / 'old').exists()
    assert not (destination / '.git').exists()


@pytest.mark.parametrize('kind', ['dirty', 'untracked', 'ignored', 'assume', 'skip', 'mode', 'symlink'])
def test_workspace_must_exactly_match_commit(repository, tmp_path, kind):
    _, base, _, _, workspace = repository
    if kind in {'assume', 'skip'}:
        git(workspace, 'update-index', '--assume-unchanged' if kind == 'assume' else '--skip-worktree', 'old')
    if kind in {'dirty', 'assume', 'skip'}:
        (workspace / 'old').write_text('tampered')
    elif kind == 'mode':
        (workspace / 'old').chmod(0o755)
    elif kind == 'symlink':
        (workspace / 'link').symlink_to('/etc/passwd')
    else:
        (workspace / ('ignored' if kind == 'ignored' else 'extra')).write_text('extra')
    with pytest.raises(RuntimeError, match='exactly match|link or special'):
        unpack(workspace, base, tmp_path)


def test_candidate_metadata_and_inherited_configuration_cannot_execute(repository, tmp_path, monkeypatch):
    source, base, future, _, workspace = repository
    marker = tmp_path / 'executed'
    metadata = workspace / '.git'
    (metadata / 'config').write_text('[core]\n fsmonitor = touch ' + str(marker) + '\n[filter "evil"]\n smudge = touch ' + str(marker) + '\n[include]\n path = /does/not/exist\n')
    (metadata / 'hooks').mkdir(exist_ok=True)
    hook = metadata / 'hooks' / 'post-checkout'
    hook.write_text('#!/bin/sh\ntouch ' + str(marker))
    hook.chmod(0o755)
    (metadata / 'objects/info').mkdir(exist_ok=True)
    (metadata / 'objects/info/alternates').write_text(str(source / '.git/objects'))
    (metadata / 'refs/replace').mkdir(parents=True)
    (metadata / 'refs/replace' / base).write_text(future + '\n')
    monkeypatch.setenv('GIT_CONFIG_COUNT', '1')
    monkeypatch.setenv('GIT_CONFIG_KEY_0', 'core.fsmonitor')
    monkeypatch.setenv('GIT_CONFIG_VALUE_0', 'touch ' + str(marker))
    monkeypatch.setenv('GIT_OBJECT_DIRECTORY', str(source / '.git/objects'))
    root, revision = unpack(workspace, base, tmp_path)
    assert revision['candidate_commit'] == base
    assert not marker.exists()
    assert not (root / 'workspace/gold').exists()


@pytest.mark.parametrize('kind', ['wrongsha', 'missing', 'nondescendant', 'corrupt', 'snapshot'])
def test_invalid_revision_or_bundle_fails(repository, tmp_path, kind):
    _, base, _, bundle, workspace = repository
    if kind == 'wrongsha':
        with pytest.raises(RuntimeError, match='SHA-1'):
            handoff.freeze(workspace, 'HEAD', tmp_path / 'bad.bundle')
        return
    if kind == 'missing':
        import shutil
        shutil.rmtree(workspace / '.git/objects')
        (workspace / '.git/objects').mkdir()
        with pytest.raises(RuntimeError):
            unpack(workspace, base, tmp_path)
        return
    if kind == 'nondescendant':
        git(workspace, 'checkout', '--orphan', 'unrelated')
        git(workspace, 'commit', '-m', 'unrelated')
        with pytest.raises(RuntimeError):
            unpack(workspace, base, tmp_path)
        return
    root, revision = unpack(workspace, base, tmp_path)
    candidate_bundle = root / 'candidate.bundle'
    expected = handoff.digest(candidate_bundle)
    if kind == 'corrupt':
        candidate_bundle.write_bytes(b'corrupted')
    else:
        (root / 'workspace/ignored').write_text('injected')
    with pytest.raises(RuntimeError, match='digest|exactly match'):
        handoff.verify(candidate_bundle, base, revision['candidate_commit'], expected,
                       root / 'workspace', tmp_path / 'destination')


def test_packed_branch_head_supported(repository, tmp_path):
    _, base, _, _, workspace = repository
    git(workspace, 'checkout', '-b', 'candidate')
    git(workspace, 'pack-refs', '--all', '--prune')
    _, revision = unpack(workspace, base, tmp_path)
    assert revision['candidate_commit'] == base


def test_alternates_cannot_supply_missing_candidate_objects(repository, tmp_path):
    source, base, future, _, workspace = repository
    metadata = workspace / '.git'
    (metadata / 'HEAD').write_text(future + '\n')
    (metadata / 'objects/info').mkdir(exist_ok=True)
    (metadata / 'objects/info/alternates').write_text(str(source / '.git/objects') + '\n')
    with pytest.raises(RuntimeError, match='git cat-file failed'):
        unpack(workspace, base, tmp_path)


def test_candidate_packed_objects_round_trip(repository, tmp_path):
    _, base, _, _, workspace = repository
    git(workspace, 'repack', '-ad')
    root, revision = unpack(workspace, base, tmp_path)
    handoff.verify(root / 'candidate.bundle', base, revision['candidate_commit'],
                   handoff.digest(root / 'candidate.bundle'), root / 'workspace', tmp_path / 'verified')
    assert (tmp_path / 'verified/old').read_text() == 'base\n'


def test_smudge_filter_never_runs_and_raw_bytes_survive(repository, tmp_path):
    _, base, _, _, workspace = repository
    marker = tmp_path / 'filter-executed'
    (workspace / '.gitattributes').write_text('old filter=evil\n')
    git(workspace, 'add', '.gitattributes')
    git(workspace, 'commit', '-m', 'attribute')
    (workspace / '.git/config').write_text('[filter "evil"]\n smudge = touch ' + str(marker) + '\n clean = touch ' + str(marker) + '\n required = true\n')
    root, revision = unpack(workspace, base, tmp_path)
    handoff.verify(root / 'candidate.bundle', base, revision['candidate_commit'],
                   handoff.digest(root / 'candidate.bundle'), root / 'workspace', tmp_path / 'verified')
    assert (tmp_path / 'verified/old').read_text() == 'base\n'
    assert not marker.exists()


@pytest.mark.parametrize('operation', ['prepare', 'verify'])
@pytest.mark.parametrize('destination_kind', ['empty', 'nonempty', 'symlink', 'file'])
def test_destination_mountpoint_must_be_empty_regular_directory(repository, tmp_path, operation, destination_kind):
    _, base, _, bundle, workspace = repository
    destination = tmp_path / 'mountpoint'
    if destination_kind == 'file':
        destination.write_text('preserve')
    elif destination_kind == 'symlink':
        target = tmp_path / 'target'
        target.mkdir()
        destination.symlink_to(target, target_is_directory=True)
    else:
        destination.mkdir()
        if destination_kind == 'nonempty':
            (destination / 'preserve').write_text('preserve')
    if operation == 'prepare':
        def invoke():
            handoff.prepare(bundle, base, handoff.digest(bundle), destination)
    else:
        root, revision = unpack(workspace, base, tmp_path)
        def invoke():
            handoff.verify(root / 'candidate.bundle', base, revision['candidate_commit'],
                           handoff.digest(root / 'candidate.bundle'), root / 'workspace', destination)
    if destination_kind == 'empty':
        invoke()
        assert (destination / 'old').read_text() == 'base\n'
    else:
        with pytest.raises(RuntimeError, match='empty regular directory'):
            invoke()
        if destination_kind == 'nonempty':
            assert (destination / 'preserve').read_text() == 'preserve'
        elif destination_kind == 'file':
            assert destination.read_text() == 'preserve'
        else:
            assert not list(target.iterdir())
