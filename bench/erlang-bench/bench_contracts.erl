-module(bench_contracts).
-export([main/0]).
-import(bench_runtime, [expect/2]).
main() ->
    Cases=[{kernel_literals,fun kernel/0},{forty_two_is_not_terminal,fun forty_two/0},
      {false_row_fails,fun false_row/0},{latency_statistics,fun stats/0},
      {timestamp_windows,fun windows/0},{monotonic_clock,fun monotonic/0},{exact_state,fun exact_state/0},
      {wrong_state_is_rejected,fun wrong_state/0},{fifo_gate,fun fifo/0},{timeout_no_late_reply,fun timeout/0},
      {dead_actor_fails_promptly,fun dead/0},{worker_failure_propagates,fun worker_failure/0},
      {later_worker_failure_is_prompt,fun later_worker_failure/0},{worker_deadline,fun worker_deadline/0},{scope_failure_reaps_children,fun scope_failure/0},
      {owner_death_reaps_children,fun owner_death/0},{all_actors_retired,fun all_retired/0},
      {probe_failure_propagates,fun probe_failure/0},{io_exact_completion,fun io_completion/0},
      {invalid_scale_rejected,fun invalid_scale/0},{full_plan_keeps_work,fun full_plan/0}],
    [run(Name,F)||{Name,F}<-Cases],io:format("ERLANG_CONTRACTS_PASS count=~b~n",[length(Cases)]).
run(Name,F) ->
    {P,M}=spawn_monitor(F),
    receive {'DOWN',M,process,P,normal}->io:format("contract ~p ok~n",[Name]);
            {'DOWN',M,process,P,R}->error({contract_failed,Name,R})
    after 10000->exit(P,kill),error({contract_timeout,Name}) end.
kernel() ->
    [expect(bench_runtime:burn_cpu(I,S),V)||{I,S,V}<-[
      {0,0,1},{1,0,7806831264735756412},{2,0,9396908728118811418},
      {10,7,6404011227278016048},{64,15,2931105770476240399},
      {1,18446744073709551615,13525302890751722018}]].
forty_two() ->
    expect(bench_runtime:burn_cpu(1,3639755004476521255),42),
    expect(bench_runtime:burn_cpu(2,3639755004476521255),10481999410520546992).
false_row() ->
    try bench_runtime:row("injected",1,1.0,[],false,"") of _->error(false_pass)
    catch error:{benchmark_failed,"injected"}->ok end.
stats() ->
    expect(bench_runtime:lat_stats([3,1,4,2]),#{n=>4,p50=>2,p90=>4,p99=>4,max=>4,mean=>2.5}),
    expect(maps:get(n,bench_runtime:lat_stats([])),0).
windows() -> expect(bench_runtime:window([{-10,-5},{-5,0},{0,2},{2,3},{3,9},{9,11},{10,12},{4,4}],0,10),[2,1,6,2]).
exact_state() ->
    {ok,1}=bench_runtime:scope(fun()->A=bench_runtime:actor(),
        expect(bench_runtime:ask(A,{cpu,2,0},1000),9396908728118811418),
        A!{tell,{echo,9}},bench_runtime:check_state(A,2,9396908728118811427) end).
fifo() ->
    {ok,1}=bench_runtime:scope(fun()->A=bench_runtime:actor(),
        {Token,R}=bench_runtime:gate(A,{echo,3}),A!{tell,{echo,5}},
        Q=bench_runtime:begin_ask(A,state),{Alias,_,_,_}=Q,
        receive {Alias,_}->error(gate_did_not_hold) after 0->ok end,
        bench_runtime:release(A,Token),{completed,3,_,_}=bench_runtime:await(R,1000),
        expect(bench_runtime:await(Q,1000),{2,8}) end).
timeout() ->
    {ok,1}=bench_runtime:scope(fun()->A=bench_runtime:actor(),{Token,R}=bench_runtime:gate(A,{echo,3}),
        {monitors,Before}=process_info(self(),monitors),
        Q=bench_runtime:begin_ask(A,{echo,5}),{Alias,_,_,_}=Q,
        try bench_runtime:await(Q,5) of _->error(no_timeout) catch exit:timeout->ok end,
        expect(process_info(self(),monitors),{monitors,Before}),
        bench_runtime:release(A,Token),{completed,3,_,_}=bench_runtime:await(R,1000),
        bench_runtime:check_state(A,2,8),
        receive {Alias,_}->error(late_reply_leaked) after 0->ok end end).
dead() ->
    {ok,1}=bench_runtime:scope(fun()->A=bench_runtime:actor(),bench_runtime:retire([A]),
        try bench_runtime:ask(A,{echo,1},5000) of _->error(dead_reply)
        catch error:{actor_down,noproc}->ok end end).
worker_failure() ->
    try bench_runtime:scope(fun()->bench_runtime:parallel([fun()->error(injected) end,
        fun()->receive never->ok end end],1000) end) of _->error(hidden_worker_failure)
    catch error:injected->ok end.
worker_deadline() ->
    try bench_runtime:scope(fun()->bench_runtime:parallel([fun()->receive never->ok end end],5) end)
    of _->error(hidden_worker_timeout) catch error:{worker_timeout,_}->ok end.
scope_failure() ->
    Self=self(),try bench_runtime:scope(fun()->A=bench_runtime:actor(),Self!{owned,A},error(injected) end)
    of _->error(hidden_failure) catch error:injected->ok end,
    receive {owned,P}->expect(is_process_alive(P),false) after 1000->error(missing_pid) end.
owner_death() ->
    Self=self(),{Owner,Mon}=spawn_monitor(fun()->bench_runtime:scope(fun()->
        A=bench_runtime:actor(),Self!{owned,A},receive never->ok end end) end),
    receive {owned,A}->ChildMon=monitor(process,A),exit(Owner,kill),
        receive {'DOWN',Mon,process,Owner,killed}->ok after 1000->error(owner_alive) end,
        receive {'DOWN',ChildMon,process,A,killed}->ok after 1000->error(child_alive) end
    after 1000->error(missing_child) end.
all_retired() ->
    {Actors,30}=bench_runtime:scope(fun()->[bench_runtime:actor()||_<-lists:seq(1,30)] end),
    expect([P||P<-Actors,is_process_alive(P)],[]).
probe_failure() ->
    try bench_runtime:scope(fun()->A=bench_runtime:actor(),W=bench_runtime:start_probes(A,{echo,1},999),
        bench_runtime:stop_probes(W,bench_runtime:deadline(1000)) end)
    of _->error(probe_failure_hidden) catch error:{unexpected_result,1,999}->ok end.
io_completion() ->
    {ok,1}=bench_runtime:scope(fun()->A=bench_runtime:actor(),
        [A!{tell,{io,1,K}}||K<-lists:seq(1,20)],bench_runtime:check_state(A,20,210) end).
invalid_scale() ->
    [try parrot_bench:config("full",S) of _->error(invalid_scale_accepted)
     catch error:{badmatch,false}->ok; error:badarg->ok end||S<-["0","-1","bad","1000001"]].
full_plan() ->
    Full=parrot_bench:keys(parrot_bench:config("full","1")),
    true=lists:member({57200000000,8},Full),true=lists:member({170000,75000},Full),
    Scaled=parrot_bench:keys(parrot_bench:config("full","24")),
    true=lists:member({2383333333,8},Scaled),true=lists:member({20833330,1},Scaled).

monotonic() ->
    Before=erlang:monotonic_time(microsecond),Actual=bench_runtime:now_us(),After=erlang:monotonic_time(microsecond),
    true=Before=<Actual andalso Actual=<After.

wrong_state() ->
    {ok,1}=bench_runtime:scope(fun()->A=bench_runtime:actor(),A!{tell,{echo,5}},
        try bench_runtime:check_state(A,10,99) of _->error(wrong_state_accepted)
        catch error:{unexpected_result,{1,5},{10,99}}->ok end end).

later_worker_failure() ->
    try bench_runtime:scope(fun()->bench_runtime:parallel([
        fun()->receive never->ok end end,fun()->error(later_injected) end],1000) end)
    of _->error(hidden_later_failure) catch error:later_injected->ok end.
