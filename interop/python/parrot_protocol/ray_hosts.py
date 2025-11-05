"""Production Ray deployment process ownership; no user imports in coordinator."""
import os
import sys
import time

from .ray_environments import host_environment, validate_environment
from .ray_artifacts import MAX_OWNED_BYTES, Snapshot, source_directory
from .ray_job import DeploymentJob
from .trace import capture_parent
from .ray_gw import _ComponentRuntime, ParrotDispatcher, _detail, _logical_path, _kill_and_confirm, _require_trace_finish

MAX_HOSTS = 64


class _DeploymentHost(_ComponentRuntime):
    def __init__(self, root):
        # This actor and its native children receive the same unique PYTHONPATH
        # through Ray runtime_env. No global import swapping in production.
        sys.path.insert(0, root)
        sys.dont_write_bytecode = True
        super().__init__(ParrotDispatcher(), prepared_root=root)


class IsolatedRuntime(_ComponentRuntime):
    def __init__(self, dispatcher, ray_module=None, trace_config=None):
        super().__init__(dispatcher, ray_module, trace_config=trace_config)
        self._deployments = {}
        self._closing = False

    def close(self):
        """Seal admissions, retire every deployment, retain uncertain cleanup.

        A driver timeout does not cancel this operation. Repeating it is safe:
        confirmed paths/hosts are already gone, uncertain ones keep ownership.
        """
        with self._lock:
            self._closing = True
            for name in list(self._deployments):
                paths = [p for p, entry in self._entries.items() if entry['owner'] == name]
                if paths:
                    self.retire_paths(name, paths)
                else:
                    try:
                        self._dispose(name)
                    except Exception:
                        pass  # Keep the cleanup-only deployment for the next call.
            pending = sorted(self._deployments)
            if self._trace_config is not None and self._trace_config.export is not None:
                receipt = self.trace_finish()
                if not receipt.get('ok'):
                    pending.append('<trace-export>')
            return dict(ok=not pending, pending=pending)

    def _dispose(self, name, deadline=None):
        deployment = self._deployments[name]
        host = deployment['host']
        if deployment.get('trace_uncertain'):
            raise RuntimeError('deployment died without a confirmed trace export receipt')
        if host is not None and not deployment['dead']:
            if (self._trace_config is not None and self._trace_config.export is not None
                    and deployment.get('trace_config_attempted') and not deployment.get('trace_finished')):
                remaining = 5.0 if deadline is None else deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError('deployment trace retirement deadline elapsed')
                receipt = self.ray.get(host.trace_finish.remote(remaining), timeout=remaining)
                _require_trace_finish(receipt, allow_disabled=not deployment.get('trace_configured'))
                deployment['trace_finished'] = True
            _kill_and_confirm(self.ray, host, host.ready, deadline)
            deployment['dead'] = True
        if deployment['job'] is not None:
            deployment['job'].close(deadline)
        deployment['snapshot'].close()
        del self._deployments[name]

    def mount_module(self, name, module_name, module_dir, env, paths, version, config):
        with self._lock:
            if self._closing:
                return dict(ok=False, paths=[], detail='gateway runtime is closing')
            if name in self._deployments or any(p in self._entries for p in paths):
                return dict(ok=False, paths=[p for p in paths if p in self._entries], detail='deployment already owned')
            if len(self._deployments) >= MAX_HOSTS:
                return dict(ok=False, paths=[], detail='deployment host capacity exceeded')
            try:
                validate_environment(env)
                source = module_dir or source_directory(module_name, None)
                if source is None:
                    raise ValueError('production deployment requires physical Python source')
                size = sum(d['snapshot'].size for d in self._deployments.values())
                snapshot = Snapshot(source, MAX_OWNED_BYTES-size)
            except BaseException as error:
                return dict(ok=False, paths=[], detail=_detail(error))
            deployment = dict(snapshot=snapshot, host=None, job=None, dead=False, setup_failed=False)
            self._deployments[name] = deployment
            for path in paths:
                self._entries[path] = dict(owner=name, handle=None, state='starting')
            try:
                context = self.ray.get_runtime_context()
                runtime_env = host_environment(env, snapshot.root, os.path.dirname(os.path.dirname(__file__)),
                                               lease=str(snapshot._lease.path), inherited_env=context.runtime_env)
                job = DeploymentJob(self.ray, context, snapshot.root, runtime_env)
                deployment['job'] = job
                host = job.start()
                deployment['host'] = host
                if self._trace_config is not None:
                    deployment['trace_config_attempted'] = True
                    if self.ray.get(host.configure_tracing.remote(self._trace_config), timeout=5) is not True:
                        raise ValueError('invalid deployment trace configuration receipt')
                    deployment['trace_configured'] = True
                receipt = self.ray.get(host.mount_module.remote(name, module_name, snapshot.root, env, paths, version, config))
                if (not isinstance(receipt, dict) or type(receipt.get('ok')) is not bool or
                        not isinstance(receipt.get('paths'), list) or
                        len(set(receipt['paths'])) != len(receipt['paths']) or
                        not set(receipt['paths']).issubset(paths) or
                        (receipt['ok'] and receipt['paths'] != paths)):
                    raise ValueError('invalid deployment host receipt')
                retained = receipt['paths']
                if not retained:
                    self._dispose(name)
                for path in paths:
                    if path not in retained:
                        del self._entries[path]
                    else:
                        self._entries[path]['state'] = 'running' if receipt['ok'] else 'stop_failed'
                return receipt
            except BaseException as error:
                # Unknown mount/kill completion keeps host, snapshot and every
                # requested path; Stop retries retirement through the same owner.
                # Do not classify RayTaskError(RuntimeEnvSetupError) as an
                # installer failure: it may follow an uncertain user commit.
                if type(error) is getattr(self.ray.exceptions, 'RuntimeEnvSetupError', None):
                    deployment['setup_failed'] = True
                detail = 'runtime environment setup failed; inspect owned Ray logs' if deployment['setup_failed'] else _detail(error)
                if deployment['host'] is None or deployment['setup_failed']:
                    try:
                        self._dispose(name)
                        for path in paths: del self._entries[path]
                        return dict(ok=False, paths=[], detail=detail)
                    except BaseException:
                        pass
                for path in paths: self._entries[path]['state'] = 'stop_failed'
                return dict(ok=False, paths=list(paths), detail=detail)

    def retire_paths(self, name, paths, timeout_ms=None):
        with self._lock:
            deadline = None if timeout_ms is None else time.monotonic() + min(timeout_ms, 60000)/1000
            state = 'stop_failed' if deadline is None else 'drain_failed'
            selected = [p for p in paths if p in self._entries]
            if any(self._entries[p]['owner'] != name for p in selected):
                raise ValueError('logical path belongs to another deployment')
            if not selected:
                return dict.fromkeys(paths)
            deployment = self._deployments[name]
            for path in selected: self._entries[path]['state'] = state
            try:
                if deployment.get('trace_uncertain'):
                    raise RuntimeError('trace export completion unknown after deployment death')
                if deployment['host'] is None or deployment['dead'] or deployment['setup_failed']:
                    result = dict.fromkeys(selected)
                else:
                    ref = deployment['host'].retire_paths.remote(name, selected, timeout_ms)
                    try:
                        result = self.ray.get(ref) if deadline is None else self.ray.get(ref, timeout=max(0,deadline-time.monotonic()))
                    except self.ray.exceptions.ActorDiedError as error:
                        # RayTaskError may inherit the exception raised by user
                        # code. Only Ray's own death report establishes this
                        # host's terminal state; remaining workers still protect
                        # their source through the snapshot's file leases.
                        if type(error) is not self.ray.exceptions.ActorDiedError:
                            raise
                        deployment['dead'] = True
                        if (self._trace_config is not None and self._trace_config.export is not None
                                and not deployment.get('trace_finished')):
                            deployment['trace_uncertain'] = True
                            raise
                        result = dict.fromkeys(selected)
                    if (not isinstance(result, dict) or set(result) != set(selected) or
                            any(v not in (None, 'stop_failed', 'drain_failed') for v in result.values())):
                        raise ValueError('invalid deployment host retirement receipt')
                remaining = [p for p,e in self._entries.items() if e['owner'] == name and
                             (p not in result or result[p] is not None)]
                if not remaining:
                    self._dispose(name, deadline)
                for path, status in result.items():
                    if status is None: del self._entries[path]
                    else: self._entries[path]['state'] = status
                return {p:result.get(p) for p in paths}
            except BaseException:
                return {p:state if p in selected else None for p in paths}

    def trace_snapshot(self, path=None):
        with self._lock:
            entry = self._entries.get(_logical_path(path)) if path is not None else None
            if entry is None:
                return super().trace_snapshot(path)
            if self._deployments[entry['owner']].get('trace_uncertain'):
                raise RuntimeError('trace export completion unknown after deployment death')
            host = self._deployments[entry['owner']]['host']
            return self.ray.get(host.trace_snapshot.remote(path), timeout=10)

    def trace_acknowledge(self, path, span_ids):
        with self._lock:
            entry = self._entries.get(_logical_path(path)) if path is not None else None
            if entry is None:
                return super().trace_acknowledge(path, span_ids)
            if entry['state'] != 'running':
                raise KeyError('no live trace owner: ' + path)
            host = self._deployments[entry['owner']]['host']
            return self.ray.get(host.trace_acknowledge.remote(path, span_ids), timeout=10)

    def trace_export_status(self, path=None):
        with self._lock:
            entry = self._entries.get(_logical_path(path)) if path is not None else None
            if entry is None:
                return super().trace_export_status(path)
            deployment = self._deployments[entry['owner']]
            if deployment.get('trace_uncertain'):
                return dict(enabled=True, ok=False, error='TraceCompletionUnknown', retained=True)
            return self.ray.get(deployment['host'].trace_export_status.remote(path), timeout=10)

    def trace_flush(self, path=None, timeout=5.0):
        with self._lock:
            entry = self._entries.get(_logical_path(path)) if path is not None else None
            if entry is None:
                return super().trace_flush(path, timeout)
            deployment = self._deployments[entry['owner']]
            if deployment.get('trace_uncertain'):
                raise RuntimeError('trace export completion unknown after deployment death')
            return self.ray.get(deployment['host'].trace_flush.remote(path, timeout), timeout=timeout)

    def ask(self, path, key, payload, *, _trace_operation='ask'):
        with self._lock:
            if self._closing:
                return dict(ok=False, code=3, detail='gateway runtime is closing')
            entry = self._entries.get(_logical_path(path))
            if entry is None or entry['state'] != 'running':
                return super().ask(path, key, payload, _trace_operation=_trace_operation)
            host = self._deployments[entry['owner']]['host']
            parent = capture_parent()
            return self.ray.get(host.ask.remote(path, key, payload) if parent is None else
                                host.ask_traced.remote(path, key, payload, parent))

    def deliver(self, path, key, payload):
        with self._lock:
            if self._closing:
                return
            entry = self._entries.get(_logical_path(path))
            if entry is None or entry['state'] != 'running':
                return super().deliver(path, key, payload)
            host = self._deployments[entry['owner']]['host']
            parent = capture_parent()
            self.ray.get(host.deliver.remote(path, key, payload) if parent is None else
                         host.deliver_traced.remote(path, key, payload, parent))
