#!/usr/bin/env python3
"""Transfer one exact Git revision without trusting candidate repository settings."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile

BASE_REF = 'refs/heads/harness-base'
CANDIDATE_REF = 'refs/heads/harness-candidate'


def sha(value: str) -> str:
    if not re.fullmatch('[0-9a-f]{40}', value):
        raise RuntimeError('revision must be a full lowercase SHA-1 commit ID')
    return value


def environment() -> dict:
    result = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
    result.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_SYSTEM='/dev/null',
                  GIT_CONFIG_GLOBAL='/dev/null', GIT_NO_REPLACE_OBJECTS='1',
                  GIT_TERMINAL_PROMPT='0')
    return result


def git(repo: Path, *args: str, objects: Path | None = None) -> bytes:
    env = environment()
    if objects is not None:
        env['GIT_OBJECT_DIRECTORY'] = str(objects)
    result = subprocess.run(['git', '--git-dir=' + str(repo), '-c', 'core.hooksPath=/dev/null',
                             '-c', 'core.attributesFile=/dev/null', *args], env=env,
                            capture_output=True, timeout=60)
    if result.returncode:
        raise RuntimeError('git ' + args[0] + ' failed: ' + result.stderr.decode(errors='replace').strip())
    return result.stdout


def initialize(path: Path) -> None:
    git(path, 'init', '--bare', '--template=', str(path))


def digest(path: Path) -> str:
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def commit(repo: Path, revision: str, objects: Path | None = None) -> None:
    if git(repo, 'cat-file', '-t', sha(revision), objects=objects) != b'commit\n':
        raise RuntimeError('revision is not a commit')


def freeze(source: Path, base: str, bundle: Path) -> dict:
    sha(base)
    # The operator supplies this source repository; never use this discovery on
    # the agent-owned candidate repository.
    result = subprocess.run(['git', '-C', str(source), 'rev-parse', '--path-format=absolute',
                             '--git-path', 'objects'], env=environment(), capture_output=True,
                            text=True, timeout=30)
    if result.returncode:
        raise RuntimeError('cannot locate source Git objects: ' + result.stderr.strip())
    objects = Path(result.stdout.strip())
    with tempfile.TemporaryDirectory() as directory:
        repo = Path(directory) / 'repo'
        initialize(repo)
        commit(repo, base, objects)
        git(repo, 'update-ref', BASE_REF, base, objects=objects)
        git(repo, 'bundle', 'create', str(bundle.resolve()), BASE_REF, objects=objects)
    return {'base_commit': base, 'bundle_sha256': digest(bundle)}


def import_bundle(repo: Path, bundle: Path, revision: str, ref: str, expected: str) -> None:
    sha(revision)
    if digest(bundle) != expected:
        raise RuntimeError('bundle digest mismatch')
    initialize(repo)
    heads = git(repo, 'bundle', 'list-heads', str(bundle)).decode().splitlines()
    if heads != [revision + ' ' + ref]:
        raise RuntimeError('bundle must contain exactly the pinned revision ref')
    git(repo, 'bundle', 'verify', str(bundle))
    git(repo, 'bundle', 'unbundle', str(bundle))
    git(repo, 'update-ref', ref, revision)
    commit(repo, revision)
    git(repo, 'fsck', '--strict', '--no-reflogs', revision)


def tree(repo: Path, revision: str) -> dict:
    files = {}
    for entry in git(repo, 'ls-tree', '-rz', '--full-tree', revision).split(b'\0'):
        if not entry:
            continue
        metadata, rawpath = entry.split(b'\t', 1)
        mode, kind, oid = metadata.split()
        path = os.fsdecode(rawpath)
        parts = path.split('/')
        if any(p in ('', '.', '..') or p.lower() == '.git' for p in parts):
            raise RuntimeError('unsafe Git tree path')
        if kind != b'blob' or mode not in (b'100644', b'100755'):
            raise RuntimeError('Git tree contains a link, gitlink, or unsupported mode')
        files[path] = (git(repo, 'cat-file', 'blob', oid.decode()), mode == b'100755')
    return files


def materialize(files: dict, destination: Path) -> None:
    if destination.is_symlink():
        raise RuntimeError('destination must be an empty regular directory')
    if destination.exists():
        if not destination.is_dir() or any(destination.iterdir()):
            raise RuntimeError('destination must be an empty regular directory')
    else:
        destination.mkdir(parents=True)
    for name, (content, executable) in files.items():
        path = destination / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        path.chmod(0o755 if executable else 0o644)


def source_files(source: Path, exclude_git: bool = False) -> dict:
    if source.is_symlink() or not source.is_dir():
        raise RuntimeError('source must be a regular directory')
    files = {}
    for directory, directories, names in os.walk(source, followlinks=False):
        if Path(directory) == source and exclude_git and '.git' in directories:
            directories.remove('.git')
        for name in directories + names:
            path = Path(directory) / name
            mode = path.lstat().st_mode
            if stat.S_ISDIR(mode):
                continue
            if not stat.S_ISREG(mode):
                raise RuntimeError('source contains a link or special file')
            files[path.relative_to(source).as_posix()] = (path.read_bytes(), bool(mode & 0o111))
    return files


def compare(files: dict, source: Path, exclude_git: bool = False) -> None:
    if files != source_files(source, exclude_git):
        raise RuntimeError('workspace files do not exactly match candidate commit (dirty or extra files)')


def prepare(bundle: Path, base: str, expected: str, workspace: Path) -> None:
    with tempfile.TemporaryDirectory() as directory:
        repo = Path(directory) / 'repo'
        import_bundle(repo, bundle.resolve(), base, BASE_REF, expected)
        materialize(tree(repo, base), workspace)
        shutil.copytree(repo, workspace / '.git')
    repo = workspace / '.git'
    git(repo, 'config', 'core.bare', 'false')
    git(repo, 'update-ref', '--no-deref', 'HEAD', base)
    git(repo, 'read-tree', base)


def regular(path: Path) -> bytes:
    if not stat.S_ISREG(path.lstat().st_mode):
        raise RuntimeError('Git metadata must be a regular file')
    return path.read_bytes()


def candidate_head(metadata: Path) -> str:
    head = regular(metadata / 'HEAD').decode().strip()
    if head.startswith('ref: '):
        ref = head[5:]
        if not ref.startswith('refs/heads/') or any(p in ('', '.', '..') for p in ref.split('/')):
            raise RuntimeError('candidate HEAD must reference a local branch')
        path = metadata / ref
        # Never follow links in intermediate reference directories.
        for parent in path.parents:
            if parent == metadata:
                break
            if parent.is_symlink():
                raise RuntimeError('candidate reference contains a symlink')
        if path.exists() or path.is_symlink():
            head = regular(path).decode().strip()
        else:
            matches = [line.split(' ', 1)[0] for line in regular(metadata / 'packed-refs').decode().splitlines()
                       if line.endswith(' ' + ref)]
            if len(matches) != 1:
                raise RuntimeError('candidate branch ref is missing or ambiguous')
            head = matches[0]
    return sha(head)


def copy_objects(metadata: Path, repo: Path) -> None:
    objects = metadata / 'objects'
    if objects.is_symlink() or not objects.is_dir():
        raise RuntimeError('candidate objects must be a directory')
    for directory in objects.iterdir():
        if not (re.fullmatch('[0-9a-f]{2}', directory.name) or directory.name == 'pack'):
            continue
        if directory.is_symlink() or not directory.is_dir():
            raise RuntimeError('candidate object directory is invalid')
        for path in directory.iterdir():
            valid = (re.fullmatch('[0-9a-f]{38}', path.name) if directory.name != 'pack'
                     else re.fullmatch(r'pack-[0-9a-f]{40}\.(pack|idx)', path.name))
            if valid:
                content = regular(path)
                target = repo / 'objects' / directory.name / path.name
                target.parent.mkdir(exist_ok=True)
                target.write_bytes(content)


def ancestry(repo: Path, base: str, candidate: str) -> None:
    commit(repo, base)
    commit(repo, candidate)
    git(repo, 'merge-base', '--is-ancestor', base, candidate)


def export(workspace: Path, base: str, output) -> None:
    sha(base)
    metadata = workspace / '.git'
    if metadata.is_symlink() or not metadata.is_dir():
        raise RuntimeError('candidate requires a regular .git directory')
    candidate = candidate_head(metadata)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        repo = root / 'repo'
        initialize(repo)
        copy_objects(metadata, repo)
        ancestry(repo, base, candidate)
        git(repo, 'fsck', '--strict', '--no-reflogs', candidate)
        files = tree(repo, candidate)
        compare(files, workspace, exclude_git=True)
        git(repo, 'update-ref', CANDIDATE_REF, candidate)
        bundle = root / 'candidate.bundle'
        git(repo, 'bundle', 'create', str(bundle), CANDIDATE_REF)
        revision = json.dumps({'base_commit': base, 'candidate_commit': candidate}).encode()
        with tarfile.open(fileobj=output, mode='w|') as archive:
            archive.add(bundle, arcname='candidate.bundle')
            info = tarfile.TarInfo('revision.json')
            info.size = len(revision)
            archive.addfile(info, io.BytesIO(revision))
            info = tarfile.TarInfo('workspace')
            info.type, info.mode = tarfile.DIRTYPE, 0o755
            archive.addfile(info)
            for name, (content, executable) in files.items():
                info = tarfile.TarInfo('workspace/' + name)
                info.size, info.mode = len(content), 0o755 if executable else 0o644
                archive.addfile(info, io.BytesIO(content))


def verify(bundle: Path, base: str, candidate: str, expected: str,
           snapshot: Path, destination: Path) -> None:
    with tempfile.TemporaryDirectory() as directory:
        repo = Path(directory) / 'repo'
        import_bundle(repo, bundle.resolve(), candidate, CANDIDATE_REF, expected)
        ancestry(repo, sha(base), candidate)
        files = tree(repo, candidate)
        compare(files, snapshot)
        materialize(files, destination)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for name, fields in [('freeze', ['source', 'base', 'bundle']),
                         ('prepare', ['bundle', 'base', 'digest', 'workspace']),
                         ('export', ['workspace', 'base']),
                         ('verify', ['bundle', 'base', 'candidate', 'digest', 'snapshot', 'destination'])]:
        command = commands.add_parser(name)
        for field in fields:
            command.add_argument(field, type=str if field in {'base', 'candidate', 'digest'} else Path)
    args = vars(parser.parse_args())
    command = args.pop('command')
    if 'digest' in args:
        args['expected'] = args.pop('digest')
    if command == 'export':
        export(**args, output=sys.stdout.buffer)
    else:
        result = globals()[command](**args)
        if result is not None:
            print(json.dumps(result))


if __name__ == '__main__':
    main()
