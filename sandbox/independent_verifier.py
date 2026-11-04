"""Independent snapshot verifier prototype. Runtime execution is intentionally disabled.

No Git command, shell, repository hook, check process or Docker invocation is used.
Input Git objects are independently content-address verified before extraction. The
container plan is for review/offline tests, not executable approval or test evidence.
"""
from dataclasses import dataclass
import hashlib
import io
import json
import os
import pathlib
import re
import stat
import tarfile
import zlib

MAX_OBJECT = 16 * 1024 * 1024
MAX_SNAPSHOT = 32 * 1024 * 1024
MAX_FILES = 4096
MAX_DEPTH = 32
OID = re.compile(r'(?:[0-9a-f]{40}|[0-9a-f]{64})\Z')
PIN = re.compile(r'[a-zA-Z0-9._:/-]+@sha256:[0-9a-f]{64}\Z')
IMAGE_ID = re.compile(r'sha256:[0-9a-f]{64}\Z')


class Rejected(ValueError):
    pass


class ExecutionDisabled(RuntimeError):
    pass


def digest_object(kind, body, length):
    if kind not in ('commit', 'tree', 'blob') or length not in (40, 64):
        raise Rejected('unsupported object kind or hash')
    raw = kind.encode() + b' ' + str(len(body)).encode() + b'\0' + body
    return (hashlib.sha1(raw) if length == 40 else hashlib.sha256(raw)).hexdigest()


class LooseObjectStore:
    """Read an operator-selected loose object directory; never resolve refs/config.

    Packed objects are deliberately unsupported. Provisioning/export must be a
    separate trusted bounded operation, not a checkout of untrusted .git metadata.
    """
    def __init__(self, objects_dir):
        self.objects_dir = pathlib.Path(objects_dir)
        if not self.objects_dir.is_absolute():
            raise Rejected('operator object directory must be absolute')

    def read(self, oid):
        if not OID.fullmatch(oid):
            raise Rejected('invalid object ID')
        # Open first in nonblocking mode: a malicious FIFO must not hang before
        # fstat can reject it. This has no effect on ordinary regular-file reads.
        flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
        root = os.open(self.objects_dir, flags | os.O_DIRECTORY)
        prefix = fd = None
        try:
            prefix = os.open(oid[:2], flags | os.O_DIRECTORY, dir_fd=root)
            fd = os.open(oid[2:], flags, dir_fd=prefix)
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_OBJECT + 65536:
                raise Rejected('not a bounded regular object')
            with os.fdopen(fd, 'rb', closefd=False) as file:
                compressed = file.read(MAX_OBJECT + 65537)
            if len(compressed) > MAX_OBJECT + 65536:
                raise Rejected('object changed size')
            inflater = zlib.decompressobj()
            raw = inflater.decompress(compressed, MAX_OBJECT + 129)
            if len(raw) > MAX_OBJECT + 128 or not inflater.eof or inflater.unused_data or inflater.unconsumed_tail:
                raise Rejected('oversized, truncated or trailing compressed object')
            header, separator, body = raw.partition(b'\0')
            if not separator or len(header) > 127:
                raise Rejected('object header')
            kind, size = header.decode('ascii').split(' ', 1)
            if not size.isdecimal() or str(int(size)) != size or int(size) != len(body):
                raise Rejected('object length')
            if digest_object(kind, body, len(oid)) != oid:
                raise Rejected('object content hash mismatch')
            return kind, body
        except (OSError, UnicodeError, zlib.error, ValueError) as error:
            if isinstance(error, Rejected):
                raise
            raise Rejected('object unavailable or corrupt') from error
        finally:
            if fd is not None:
                os.close(fd)
            if prefix is not None:
                os.close(prefix)
            os.close(root)


@dataclass(frozen=True)
class SnapshotFile:
    path: str
    executable: bool
    content: bytes


@dataclass(frozen=True)
class Snapshot:
    commit: str
    tree: str
    files: tuple
    digest: str


def _name(raw):
    try:
        name = raw.decode('utf-8')
    except UnicodeError as error:
        raise Rejected('only UTF-8 paths supported') from error
    if not name or name in ('.', '..') or name.casefold() == '.git' or any(c in name for c in '/\\:\0') or any(ord(c) < 32 or ord(c) == 127 for c in name):
        raise Rejected('unsafe tree path')
    return name


def extract_snapshot(store, commit):
    """Extract regular files from exact content-verified commit/tree/blob objects.

    Even a custom object store is untrusted: verify type, size and hash on every
    object here too. No refs, replace refs, index, attributes, symlinks or submodules.
    """
    if not OID.fullmatch(commit):
        raise Rejected('invalid candidate commit')
    total = 0
    files = []
    paths = set()
    active = set()
    object_reads = 0
    object_bytes = 0
    entries = 0

    def read(oid, expected):
        nonlocal object_reads, object_bytes
        object_reads += 1
        if object_reads > MAX_FILES * 4:
            raise Rejected('object traversal limit')
        kind, body = store.read(oid)
        object_bytes += len(body) if isinstance(body, bytes) else 0
        if object_bytes > MAX_SNAPSHOT * 4:
            raise Rejected('object read budget')
        if not isinstance(body, bytes) or len(body) > MAX_OBJECT or kind != expected or digest_object(kind, body, len(commit)) != oid:
            raise Rejected('object identity mismatch')
        return body

    raw = read(commit, 'commit')
    first = raw.split(b'\n', 1)[0]
    if not first.startswith(b'tree '):
        raise Rejected('commit has no first tree header')
    try:
        tree = first[5:].decode('ascii')
    except UnicodeError as error:
        raise Rejected('invalid tree ID') from error
    if len(tree) != len(commit) or not OID.fullmatch(tree):
        raise Rejected('tree hash format')

    def walk(oid, parent, depth):
        nonlocal total, entries
        if depth > MAX_DEPTH or oid in active:
            raise Rejected('tree depth/cycle')
        active.add(oid)
        body = read(oid, 'tree')
        offset = 0
        names = set()
        while offset < len(body):
            entries += 1
            if entries > MAX_FILES * 2:
                raise Rejected('tree entry limit')
            space = body.find(b' ', offset)
            zero = body.find(b'\0', space + 1)
            size = len(commit) // 2
            if space <= offset or zero < space or zero + 1 + size > len(body):
                raise Rejected('tree encoding')
            mode = body[offset:space]
            name = _name(body[space + 1:zero])
            if name in names:
                raise Rejected('duplicate tree name')
            names.add(name)
            path = parent + name
            if len(path.encode()) > 4096:
                raise Rejected('path length')
            child = body[zero + 1:zero + 1 + size].hex()
            offset = zero + 1 + size
            if mode in (b'40000', b'040000'):
                walk(child, path + '/', depth + 1)
            elif mode in (b'100644', b'100755'):
                blob = read(child, 'blob')
                total += len(blob)
                if len(files) >= MAX_FILES or total > MAX_SNAPSHOT or path in paths:
                    raise Rejected('snapshot limit or duplicate path')
                paths.add(path)
                files.append(SnapshotFile(path, mode == b'100755', blob))
            else:
                raise Rejected('symlink, submodule or special file forbidden')
        active.remove(oid)

    walk(tree, '', 0)
    ordered = tuple(sorted(files, key=lambda file: file.path.encode()))
    manifest = [{'path': f.path, 'mode': '100755' if f.executable else '100644',
                 'size': len(f.content), 'sha256': hashlib.sha256(f.content).hexdigest()}
                for f in ordered]
    canonical = json.dumps({'commit': commit, 'tree': tree, 'files': manifest},
                           sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode()
    return Snapshot(commit, tree, ordered, hashlib.sha256(canonical).hexdigest())


def source_image_context(snapshot, base_image):
    """Deterministic build context, no RUN step and no repository Dockerfile execution.

    Before a future build, the trusted daemon must attest the pinned base has no
    ONBUILD instructions or declared volumes; neither may be inherited silently.
    """
    if not PIN.fullmatch(base_image):
        raise Rejected('pin audited base image digest')
    # Defense in depth: never trust a manually constructed Snapshot for tar paths.
    seen = set()
    size = 0
    for file in snapshot.files:
        if not isinstance(file.content, bytes) or not isinstance(file.executable, bool) or len(file.path.encode()) > 4096:
            raise Rejected('snapshot file schema')
        components = file.path.split('/')
        if len(components) > MAX_DEPTH + 1:
            raise Rejected('snapshot path depth')
        for component in components:
            _name(component.encode())
        if file.path in seen:
            raise Rejected('duplicate snapshot path')
        seen.add(file.path)
        size += len(file.content)
        if len(seen) > MAX_FILES or size > MAX_SNAPSHOT:
            raise Rejected('snapshot size')
    if not OID.fullmatch(snapshot.commit) or not re.fullmatch(r'[0-9a-f]{64}', snapshot.digest):
        raise Rejected('snapshot binding format')
    dockerfile = ('FROM ' + base_image + '\nCOPY snapshot/ /candidate/\n'
                  'LABEL org.agenct.snapshot=' + snapshot.digest + '\n'
                  'LABEL org.agenct.commit=' + snapshot.commit + '\n'
                  'USER 65534:65534\nWORKDIR /candidate\n').encode()
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode='w', format=tarfile.PAX_FORMAT) as archive:
        def add(path, content, mode):
            info = tarfile.TarInfo(path)
            info.size = len(content)
            info.mode = mode
            info.uid = info.gid = info.mtime = 0
            info.uname = info.gname = ''
            archive.addfile(info, io.BytesIO(content))
        add('Dockerfile', dockerfile, 0o444)
        # Explicit root even for an empty source tree.
        root = tarfile.TarInfo('snapshot/')
        root.type = tarfile.DIRTYPE
        root.mode = 0o555
        archive.addfile(root)
        for file in snapshot.files:
            add('snapshot/' + file.path, file.content, 0o555 if file.executable else 0o444)
    return buffer.getvalue()


def verify_source_archive(snapshot, archive_bytes):
    """Compare a stopped-container source archive from trusted daemon to exact manifest.

    Never extract to host filesystem. The container must not have been started, and
    its image ID must equal the immutable ID used by every later check container.
    Reject extra files as well as changed/missing files, so stale base-image source
    content cannot silently contaminate the candidate input.
    """
    if len(archive_bytes) > MAX_SNAPSHOT + 4 * 1024 * 1024:
        raise Rejected('source archive size')
    expected = {'candidate/' + f.path: f for f in snapshot.files}
    directories = {'candidate'}
    for path in expected:
        parts = path.split('/')
        directories.update('/'.join(parts[:i]) for i in range(1, len(parts)))
    seen = set()
    count = 0
    try:
        with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode='r:') as archive:
            for member in archive:
                count += 1
                if count > MAX_FILES * 2 + MAX_DEPTH:
                    raise Rejected('source archive entry limit')
                name = member.name.rstrip('/')
                for component in name.split('/'):
                    _name(component.encode())
                if member.isdir():
                    if name not in directories:
                        raise Rejected('extra source directory')
                    continue
                if not member.isfile() or member.sparse is not None or any('sparse' in key for key in member.pax_headers):
                    raise Rejected('source archive contains link or special file')
                expected_file = expected.get(name)
                if expected_file is None or name in seen:
                    raise Rejected('extra or duplicate source file')
                if member.size != len(expected_file.content) or member.mode != (0o555 if expected_file.executable else 0o444):
                    raise Rejected('source archive size/mode mismatch')
                content = archive.extractfile(member).read(MAX_OBJECT + 1)
                if content != expected_file.content:
                    raise Rejected('source archive content mismatch')
                seen.add(name)
    except (tarfile.TarError, OSError, UnicodeError) as error:
        raise Rejected('invalid source archive') from error
    if seen != set(expected):
        raise Rejected('source archive missing files')
    return snapshot.digest


def validate_base_fixture(base_image, observation):
    """Offline preflight: no inherited code execution or writable image volumes."""
    if not PIN.fullmatch(base_image) or base_image not in observation.get('RepoDigests', []):
        raise Rejected('base digest mismatch')
    config = observation.get('Config', {})
    if config.get('OnBuild') not in (None, []) or config.get('Volumes') not in (None, {}):
        raise Rejected('base ONBUILD/volume prohibited')


@dataclass(frozen=True)
class CheckPlan:
    snapshot_digest: str
    commit: str
    image_id: str
    checker: str
    argv: tuple


def plan_check(snapshot, image_id, checker):
    """Checker is an operator-owned image executable, not a repository command."""
    if not IMAGE_ID.fullmatch(image_id) or not re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', checker):
        raise Rejected('invalid immutable image/checker')
    executable = '/opt/agenct-checks/' + checker
    # Docker uses an empty PID mode for its own namespace; 'private' is not
    # a valid PID CLI value (unlike IPC). Still verify the daemon observation.
    argv = ('create', '--network=none', '--pid=', '--ipc=private', '--read-only', '--cap-drop=ALL',
            '--security-opt=no-new-privileges', '--user=65534:65534',
            '--pids-limit=64', '--memory=512m', '--memory-swap=512m', '--cpus=1',
            '--tmpfs=/scratch:rw,nosuid,nodev,size=134217728,mode=1777',
            '--tmpfs=/tmp:rw,nosuid,nodev,noexec,size=33554432,mode=1777',
            '--workdir=/candidate', '--entrypoint=' + executable,
            image_id, '--source=/candidate', '--output=/scratch')
    return CheckPlan(snapshot.digest, snapshot.commit, image_id, executable, argv)


def validate_daemon_fixture(plan, observation):
    """Offline schema validation only, NOT production test evidence.

    Future observations must come from the trusted Docker daemon, never stdout,
    stderr, files written by tests, model responses or caller-supplied HTTP JSON.
    The sealed Rust executor deliberately does not accept this prototype output.
    """
    config = observation.get('Config', {})
    host = observation.get('HostConfig', {})
    state = observation.get('State', {})
    if observation.get('Image') != plan.image_id or config.get('Image') != plan.image_id:
        raise Rejected('image binding')
    if config.get('Entrypoint') != [plan.checker] or config.get('Cmd') != ['--source=/candidate', '--output=/scratch'] or config.get('User') != '65534:65534' or config.get('WorkingDir') != '/candidate':
        raise Rejected('check identity')
    # Missing namespace observations are not evidence of private isolation.
    # Empty is Docker's default private mode; host/container joins are forbidden.
    if host.get('PidMode') not in ('', 'private') or host.get('IpcMode') not in ('', 'private'):
        raise Rejected('PID/IPC namespace must be private')
    if config.get('Volumes') not in (None, {}):
        raise Rejected('inherited image volume')
    if host.get('SecurityOpt') != ['no-new-privileges'] or host.get('PidsLimit') != 64 or host.get('Memory') != 512 * 1024 * 1024 or host.get('MemorySwap') != 512 * 1024 * 1024 or host.get('NanoCpus') != 1_000_000_000 or host.get('VolumesFrom') not in (None, []) or host.get('DeviceRequests') not in (None, []):
        raise Rejected('resource/security policy')
    if host.get('ReadonlyRootfs') is not True or host.get('Privileged') is not False or host.get('NetworkMode') != 'none' or host.get('CapDrop') != ['ALL'] or host.get('CapAdd') not in (None, []) or host.get('Binds') not in (None, []) or host.get('Devices') not in (None, []):
        raise Rejected('container policy')
    mounts = observation.get('Mounts', [])
    if any(m.get('Type') != 'tmpfs' or m.get('Destination') not in ('/scratch', '/tmp') for m in mounts):
        raise Rejected('unexpected writable mount')
    if state.get('Status') != 'exited' or state.get('Running') is not False or type(state.get('ExitCode')) is not int or state.get('ExitCode') != 0 or state.get('OOMKilled') is not False or state.get('Error'):
        raise Rejected('check did not exit successfully')
    return {'status': 'offline-fixture-only', 'snapshot_digest': plan.snapshot_digest,
            'commit': plan.commit, 'checker': plan.checker}


def execute(*_args, **_kwargs):
    raise ExecutionDisabled('independent verifier runtime disabled pending Docker smoke and security review')
