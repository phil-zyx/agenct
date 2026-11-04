"""Host-side lifecycle supervisor for fixed benign Docker fixtures only.

Production execution remains disabled. This module neither runs a shell nor
accepts repository content/check commands, credentials, provider or publish APIs.
The Engine boundary must represent the trusted daemon, never container stdout.
"""
from dataclasses import dataclass
import hashlib
import re
import time
import uuid

import independent_verifier as verifier


class StageRejected(RuntimeError):
    pass


class StageRevoked(StageRejected):
    pass


class StageTimeout(StageRejected):
    pass


class CleanupUnconfirmed(StageRejected):
    pass


@dataclass(frozen=True)
class StagingObservation:
    status: str
    production_eligible: bool
    candidate_commit: str
    snapshot_digest: str
    image_id: str
    checker: str
    container_id: str
    policy_digest: str
    cleanup_confirmed: bool


class _FixtureObjects:
    def __init__(self):
        self.objects = {}

    def add(self, kind, body):
        oid = verifier.digest_object(kind, body, 40)
        self.objects[oid] = (kind, body)
        return oid

    def read(self, oid):
        return self.objects[oid]


def fixture_snapshot():
    """The supervisor creates its own immutable fixture; no user source is accepted."""
    store = _FixtureObjects()
    blob = store.add('blob', b'agenct benign fixture\n')
    tree = store.add('tree', b'100644 fixture.txt\0' + bytes.fromhex(blob))
    commit = store.add('commit', ('tree ' + tree + '\n\nfixed fixture\n').encode())
    return verifier.extract_snapshot(store, commit)


def observe_benign(engine, image_id, checker='probe', *, still_authorized=lambda: True,
                   timeout=30.0, monotonic=time.monotonic, sleep=time.sleep, journal=None):
    """Observe fixed probe/hang fixture through a bounded trusted engine interface.

    Engine methods: create(name, plan, labels, timeout), inspect(id_or_name, timeout),
    archive(container_id, timeout), start(container_id, timeout), remove(name, timeout).
    remove must confirm absence, including after uncertain creation. All operations
    must be individually bounded. Container output is never a source of evidence.
    Success is staging-only and is emitted only after cleanup and final authorization.
    """
    if checker not in ('probe', 'hang') or type(timeout) not in (int, float) or not 0 < timeout <= 60:
        raise StageRejected('fixed fixture/check deadline required')
    snapshot = fixture_snapshot()
    plan = verifier.plan_check(snapshot, image_id, checker)
    name = 'agenct-stage-' + uuid.uuid4().hex
    labels = {'agenct.stage': name, 'agenct.snapshot': snapshot.digest}
    deadline = monotonic() + timeout
    registered = False
    receipt = None
    container_id = None

    def budget():
        if not still_authorized():
            raise StageRevoked('staging lease revoked')
        remaining = deadline - monotonic()
        if remaining <= 0:
            raise StageTimeout('staging deadline exceeded')
        return min(remaining, 5.0)

    def identity(observation):
        nonlocal container_id
        observed_id = observation.get('Id')
        if not isinstance(observed_id, str) or not re.fullmatch('[0-9a-f]{64}', observed_id):
            raise StageRejected('invalid daemon container identity')
        if container_id is not None and observed_id != container_id:
            raise StageRejected('container identity changed')
        if observation.get('Name') != '/' + name:
            raise StageRejected('container name changed')
        observed_labels = observation.get('Config', {}).get('Labels', {})
        if any(observed_labels.get(key) != value for key, value in labels.items()):
            raise StageRejected('container lease/snapshot binding changed')
        verifier.validate_container_policy(plan, observation)
        if journal is not None:
            journal.bind(name, observed_id)
        container_id = observed_id

    try:
        budget()
        # Register before create: a timed-out create may still have made a container.
        if journal is not None:
            journal.register(name, image_id, snapshot.digest)
        # Durable registration may block long enough to expire or revoke the lease.
        # Recheck immediately before issuing create; no daemon action has occurred.
        try:
            create_timeout = budget()
        except BaseException:
            if journal is not None:
                journal.cancel_unissued(name)
            raise
        registered = True
        engine.create(name, plan, labels, timeout=create_timeout)
        if journal is not None:
            journal.created(name)
        initial = engine.inspect(name, timeout=budget())
        identity(initial)
        state = initial.get('State', {})
        if state.get('Status') != 'created' or state.get('Running') is not False:
            raise StageRejected('source container must never have started')
        verifier.verify_source_archive(snapshot, engine.archive(container_id, timeout=budget()))
        # A fresh inspection rejects concurrent start/config/identity replacement.
        before_start = engine.inspect(container_id, timeout=budget())
        identity(before_start)
        if before_start.get('State', {}).get('Status') != 'created':
            raise StageRejected('container started outside supervisor')
        engine.start(container_id, timeout=budget())
        while True:
            observed = engine.inspect(container_id, timeout=budget())
            identity(observed)
            state = observed.get('State', {})
            if state.get('Status') == 'exited':
                verifier.validate_daemon_fixture(plan, observed)
                break
            if state.get('Status') != 'running' or state.get('Running') is not True:
                raise StageRejected('unexpected container lifecycle state')
            sleep(min(0.1, budget()))
        # Final rootfs comparison is defense in depth, not a substitute for immutable input.
        verifier.verify_source_archive(snapshot, engine.archive(container_id, timeout=budget()))
        budget()
        receipt = StagingObservation(
            status='benign-stage-observed', production_eligible=False,
            candidate_commit=snapshot.commit, snapshot_digest=snapshot.digest,
            image_id=image_id, checker=checker, container_id=container_id,
            policy_digest=hashlib.sha256('\0'.join(plan.argv).encode()).hexdigest(),
            cleanup_confirmed=True)
    finally:
        if registered:
            try:
                if journal is not None:
                    journal.reconcile(engine)
                    removed = True
                else:
                    removed = engine.remove(name, timeout=5.0)
            except Exception as error:
                raise CleanupUnconfirmed('staging container absence not confirmed') from error
            if removed is not True:
                raise CleanupUnconfirmed('staging container absence not confirmed')
    # Cleanup can outlast a lease. No receipt is emitted after revocation or deadline.
    budget()
    return receipt


def execute_verified_job(*_args, **_kwargs):
    """No configuration flag, image digest or fixture receipt enables production jobs."""
    raise verifier.ExecutionDisabled('production supervisor disabled pending full safety gates')
