"""Keep task workers within the lifetime of their source's deployment.

Workers in a verified deployment-owned job use Ray's normal reusable task pool
and explicit max_calls limits. Closing that job retires the entire pool before
source removal. Leased workers without a dedicated job fall back to max_calls=1
at Ray's native boundary after result/error/stream handling. That fallback costs
a process startup per task. Neither path exits inside a user function's finally
block, guesses task idleness, nor changes actor lifetimes or task timeouts.
"""


def install_task_worker_lifetime(manager=None):
    if manager is None:
        import os
        from ray._private.worker import global_worker
        owned_job = os.environ.get('PARROT_DEPLOYMENT_JOB')
        if owned_job is not None:
            if owned_job != global_worker.current_job_id.hex():
                raise RuntimeError('deployment worker belongs to a different Ray job')
            # The dedicated driver owns the complete reusable worker pool.
            # Its job must end before the coordinator removes the snapshot.
            return
        manager = global_worker.function_actor_manager
    original = manager.get_execution_info
    if getattr(original, '_parrot_owned_tasks', False):
        return

    def execution_info(job_id, descriptor):
        info = original(job_id, descriptor)
        if descriptor.is_actor_method():
            return info
        # Both supported Ray versions initialize the task counter while loading
        # this info, then cache it and apply max_calls after task output handling.
        # A user-specified max_calls is an upper bound, not a minimum lifetime.
        return info._replace(max_calls=1)

    execution_info._parrot_owned_tasks = True
    manager.get_execution_info = execution_info
