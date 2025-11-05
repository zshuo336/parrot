"""Portable TOML preflight and Ray environment construction for owned hosts."""
from copy import deepcopy
import os

from .ray_snapshot_lease import SETUP_HOOK, effective_setup_hook

MAX_ENV_BYTES = 64 * 1024
_FIELDS = {'env_vars','pip','conda','uv','working_dir','py_modules','py_executable',
           'worker_process_setup_hook','config'}
_RESERVED = {'PARROT_SNAPSHOT_CONTROL', 'PARROT_SNAPSHOT_USER_HOOK', 'PARROT_DEPLOYMENT_JOB',
             '__RAY_WORKER_PROCESS_SETUP_HOOK_ENV_VAR'}


def parse_environment(raw):
    if raw is None or raw == '':
        return {}
    if not isinstance(raw,str) or len(raw.encode('utf-8')) > MAX_ENV_BYTES:
        raise ValueError('runtime_env must be TOML of at most 64 KiB')
    try:
        import tomllib
    except ImportError:
        import tomli as tomllib
    env = tomllib.loads(raw)
    validate_environment(env)
    return env


def validate_environment(env):
    if not isinstance(env,dict) or set(env)-_FIELDS:
        raise ValueError('unsupported runtime_env fields')
    if env.get('worker_process_setup_hook') == SETUP_HOOK:
        raise ValueError('snapshot setup hook is internal; configure your own function path')
    variables = env.get('env_vars',{})
    if not isinstance(variables,dict) or len(variables)>256:
        raise ValueError('runtime_env env_vars must be a table of at most 256 strings')
    for key,value in variables.items():
        if (not isinstance(key,str) or not key or '=' in key or '\x00' in key or
                not isinstance(value,str) or '\x00' in value):
            raise ValueError('invalid runtime_env environment variable')
        if key in _RESERVED:
            raise ValueError('runtime_env environment variable is reserved for snapshot ownership')
    if sum(key in env for key in ('pip','conda','uv')) > 1:
        raise ValueError('runtime_env pip, conda and uv are mutually exclusive')
    config = env.get('config',{})
    if not isinstance(config,dict) or set(config)-{'setup_timeout_seconds','eager_install','log_files'}:
        raise ValueError('unsupported runtime_env config')
    timeout = config.get('setup_timeout_seconds',600)
    if type(timeout) is not int or not 1<=timeout<=600:
        raise ValueError('runtime_env setup_timeout_seconds must be between 1 and 600')
    if 'eager_install' in config and type(config['eager_install']) is not bool:
        raise ValueError('runtime_env eager_install must be a boolean')
    if 'log_files' in config and (not isinstance(config['log_files'],list) or
            any(not isinstance(value,str) for value in config['log_files'])):
        raise ValueError('runtime_env log_files must be a list of strings')
    if not env:
        return
    # Validate with the actual installed Ray version, including mutually
    # exclusive managers and package schemas, before allocating a host/snapshot.
    from ray.runtime_env import RuntimeEnv, RuntimeEnvConfig
    if set(config)-RuntimeEnvConfig.known_fields:
        raise ValueError('runtime_env config is unsupported by the installed Ray version')
    if set(env)-RuntimeEnv.known_fields:
        raise ValueError('runtime_env field is unsupported by the installed Ray version')
    RuntimeEnv(**deepcopy(env)).serialize()


def host_environment(env, root, protocol_root, lease=None, inherited_env=None):
    validate_environment(env)
    # A separate deployment job has no implicit parent runtime_env. Preserve
    # Ray's actor inheritance: override top-level fields, merge env_vars by key.
    # Ownership fields belong to this deployment, never to a parent job/lease.
    result = deepcopy(inherited_env or {})
    inherited_variables = result.pop('env_vars', {})
    result.update(deepcopy(env))
    result['env_vars'] = {key: value for key, value in inherited_variables.items() if key not in _RESERVED}
    result['env_vars'].update(deepcopy(env.get('env_vars', {})))
    variables = result.setdefault('env_vars',{})
    paths = [root,protocol_root]
    for path in [variables.get('PYTHONPATH',''),os.environ.get('PYTHONPATH','')]:
        paths.extend(p for p in path.split(os.pathsep) if p and p not in paths)
    variables['PYTHONPATH'] = os.pathsep.join(paths)
    variables['PYTHONDONTWRITEBYTECODE'] = '1'
    if lease is not None:
        from ray._private.ray_constants import WORKER_PROCESS_SETUP_HOOK_ENV_VAR
        user_hook = env.get('worker_process_setup_hook')
        if user_hook is None:
            user_hook = effective_setup_hook(inherited_env or {})
        if user_hook is not None and not isinstance(user_hook, str):
            raise ValueError('actor setup hook must be an importable function path')
        variables['PARROT_SNAPSHOT_CONTROL'] = lease
        variables['PARROT_SNAPSHOT_USER_HOOK'] = user_hook or ''
        hook = SETUP_HOOK
        result['worker_process_setup_hook'] = hook
        # Ray exports job-level hooks to this field, but actor-level options
        # only serialize their runtime_env. Populate the same importable hook
        # explicitly; do not rely on an ignored actor-level callable.
        variables[WORKER_PROCESS_SETUP_HOOK_ENV_VAR] = hook
    # The snapshot and protocol locations take precedence over extra user paths.
    # Other variables apply only to this host's Ray runtime environment; never
    # mutate the gateway/coordinator's os.environ.
    return result


def child_environment(env, control):
    """Carry an explicit child hook through Ray's independent env_vars merge."""
    if not isinstance(env, dict) or env.get('worker_process_setup_hook') == SETUP_HOOK:
        return env  # Includes already composed internal deployment environments.
    variables = env.get('env_vars', {})
    if isinstance(variables, dict) and _RESERVED.intersection(variables):
        raise ValueError('child runtime_env environment variable is reserved for snapshot ownership')
    hook = env.get('worker_process_setup_hook')
    if hook is None:
        return env  # Ray will inherit the parent's exported hook and lease.
    if not isinstance(hook, str):
        raise ValueError('actor/task setup hook must be an importable function path')
    result = deepcopy(env)
    from ray._private.ray_constants import WORKER_PROCESS_SETUP_HOOK_ENV_VAR
    variables = result.setdefault('env_vars', {})
    variables['PARROT_SNAPSHOT_CONTROL'] = control
    variables['PARROT_SNAPSHOT_USER_HOOK'] = hook
    variables[WORKER_PROCESS_SETUP_HOOK_ENV_VAR] = SETUP_HOOK
    result['worker_process_setup_hook'] = SETUP_HOOK
    return result


def install_child_hook_adapter():
    """Install only in leased workers, before any user imports/decorators.

    Ray actor/task option parsers otherwise serialize the hook field without
    exporting it. Adapt both decorator and .options paths in this process;
    unconfigured environments retain Ray's normal validation and inheritance.
    """
    import ray.actor
    import ray.remote_function
    for module in (ray.actor, ray.remote_function):
        original = module.parse_runtime_env_for_task_or_actor
        if getattr(original, '_parrot_snapshot_adapter', False):
            continue
        def parse(env, _original=original):
            control = os.environ.get('PARROT_SNAPSHOT_CONTROL')
            return _original(child_environment(env, control) if control is not None else env)
        parse._parrot_snapshot_adapter = True
        module.parse_runtime_env_for_task_or_actor = parse
