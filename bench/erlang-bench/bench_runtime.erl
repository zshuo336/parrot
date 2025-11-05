%% Owned benchmark processes and measurement primitives, not a production actor API.
-module(bench_runtime).
-export([scope/1, actor/0, retire/1, work/1, join/2, parallel/2,
         ask/3, begin_ask/2, await/2, gate/2, release/2, check_state/3,
         burn_cpu/2, now_us/0, deadline/1, remaining/1, expect/2,
         lat_stats/1, window/3, row/6, start_probes/3, stop_probes/2]).
-define(MASK, 16#ffffffffffffffff).

scope(F) ->
    undefined = get(bench_owned),
    expect(process_info(self(), trap_exit), {trap_exit, false}),
    put(bench_owned, #{}), put(bench_retired, 0),
    try
        Value = F(),
        Owned = get(bench_owned),
        expect([P || {P, {_, worker}} <- maps:to_list(Owned)], []),
        retire([P || {P, {_, actor}} <- maps:to_list(Owned)]),
        expect(get(bench_owned), #{}),
        {Value, get(bench_retired)}
    after
        cleanup(), erase(bench_owned), erase(bench_retired)
    end.

owned(F, Role) ->
    {Pid, Mon} = spawn_opt(F, [link, monitor]),
    put(bench_owned, maps:put(Pid, {Mon, Role}, get(bench_owned))), Pid.
actor() -> owned(fun() -> loop(0, 0) end, actor).
forget(Pid) ->
    put(bench_owned, maps:remove(Pid, get(bench_owned))),
    put(bench_retired, get(bench_retired) + 1).
mon(Pid) -> element(1, maps:get(Pid, get(bench_owned))).
normal_down(Pid, Mon, D) ->
    receive
        {'DOWN', Mon, process, Pid, normal} -> forget(Pid);
        {'DOWN', Mon, process, Pid, Reason} -> forget(Pid), error({child_failed, Reason})
    after remaining(D) -> error({child_retirement_timeout, Pid}) end.
retire(Pids) ->
    D = deadline(10000),
    [P ! stop || P <- Pids],
    [normal_down(P, mon(P), D) || P <- Pids], ok.
cleanup() ->
    Owned = maps:to_list(get(bench_owned)),
    [begin unlink(P), exit(P, kill) end || {P, _} <- Owned],
    D = deadline(10000),
    [receive {'DOWN', M, process, P, _} -> ok
     after remaining(D) -> error({cleanup_timeout, P}) end || {P, {M, _}} <- Owned], ok.

work(F) ->
    Parent = self(), Tag = make_ref(),
    Pid = owned(fun() ->
        Result = try F() of V -> {ok, V} catch C:R:S -> {error, C, R, S} end,
        Parent ! {Tag, self(), Result}
    end, worker),
    {Pid, Tag}.
join({Pid, Tag}, D) ->
    Mon = mon(Pid),
    Result = receive
        {Tag, Pid, R} -> R;
        {'DOWN', Mon, process, Pid, Why} -> forget(Pid), error({worker_failed, Why})
    after remaining(D) -> error({worker_timeout, Pid}) end,
    normal_down(Pid, Mon, D),
    unwrap(Result).
unwrap({ok, V}) -> V;
unwrap({error, C, R0, S}) -> erlang:raise(C, R0, S).
parallel(Funs, Timeout) ->
    D = deadline(Timeout), Workers = [work(F) || F <- Funs],
    Pending = maps:from_list([{Pid, {Tag, Index}} || {{Pid, Tag}, Index} <- lists:zip(Workers, lists:seq(1, length(Workers)))]),
    Results = collect(Pending, #{}, D),
    [maps:get(I, Results) || I <- lists:seq(1, length(Workers))].
collect(Pending, Results, _D) when map_size(Pending) =:= 0 -> Results;
collect(Pending, Results, D) ->
    receive
        {Tag, Pid, Result} when is_map_key(Pid, Pending) ->
            {Tag, Index} = maps:get(Pid, Pending),
            normal_down(Pid, mon(Pid), D),
            Value = unwrap(Result),
            collect(maps:remove(Pid, Pending), maps:put(Index, Value, Results), D);
        {'DOWN', Mon, process, Pid, Why} when is_map_key(Pid, Pending) ->
            Mon = mon(Pid), forget(Pid), error({worker_failed, Why})
    after remaining(D) -> error({worker_timeout, maps:keys(Pending)}) end.

begin_ask(Pid, Msg) ->
    Alias = erlang:alias(), Mon = erlang:monitor(process, Pid), Start = now_us(),
    Pid ! {request, Alias, Msg}, {Alias, Mon, Pid, Start}.
await({Alias, Mon, Pid, _}, Timeout) ->
    try receive
        {Alias, Reply} -> Reply;
        {'DOWN', Mon, process, Pid, Why} -> error({actor_down, Why})
    after Timeout -> exit(timeout) end
    after
        erlang:unalias(Alias), erlang:demonitor(Mon, [flush]),
        receive {Alias, _} -> ok after 0 -> ok end
    end.
ask(Pid, Msg, Timeout) -> await(begin_ask(Pid, Msg), Timeout).
gate(Pid, Job) ->
    Token = make_ref(), Request = begin_ask(Pid, {gated, self(), Token, Job}),
    receive {entered, Token, Pid} -> {Token, Request}
    after 10000 -> error(gate_not_entered) end.
release(Pid, Token) -> Pid ! {release, Token}, ok.
check_state(Pid, Count, Sum) -> expect(ask(Pid, state, 3600000), {Count, Sum band ?MASK}).

loop(Count, Sum) ->
    receive
        {request, Alias, state} -> Alias ! {Alias, {Count, Sum}}, loop(Count, Sum);
        {request, Alias, {gated, Parent, Token, Job}} ->
            Parent ! {entered, Token, self()},
            receive {release, Token} -> ok end,
            Start = now_us(), V = execute(Job), End = now_us(),
            Alias ! {Alias, {completed, V, Start, End}},
            loop(Count + 1, (Sum + V) band ?MASK);
        {request, Alias, Job} ->
            V = execute(Job), Alias ! {Alias, V},
            loop(Count + 1, (Sum + V) band ?MASK);
        {tell, Job} -> V = execute(Job), loop(Count + 1, (Sum + V) band ?MASK);
        stop -> ok
    end.
execute({echo, V}) -> V;
execute({cpu, I, Salt}) -> burn_cpu(I, Salt);
execute({io, Ms, V}) -> timer:sleep(Ms), V.
burn_cpu(I, Salt) when is_integer(I), I >= 0, I =< ?MASK, is_integer(Salt), Salt >= 0, Salt =< ?MASK ->
    burn((Salt bor 1), I, 0).
burn(X, 0, _) -> X;
burn(X, Left, Index) ->
    Next = ((X * 6364136223846793005 + 1442695040888963407) band ?MASK) bxor Index,
    burn(Next, Left - 1, Index + 1).
now_us() -> erlang:monotonic_time(microsecond).
deadline(Ms) -> erlang:monotonic_time(millisecond) + Ms.
remaining(D) -> max(0, D - erlang:monotonic_time(millisecond)).
expect(V, V) -> ok;
expect(Actual, Expected) -> error({unexpected_result, Actual, Expected}).

lat_stats([]) -> #{n => 0, p50 => 0, p90 => 0, p99 => 0, max => 0, mean => 0.0};
lat_stats(Samples) ->
    true = lists:all(fun(V) -> is_integer(V) andalso V >= 0 end, Samples),
    Sorted = lists:sort(Samples), N = length(Sorted),
    Pick = fun(Q) -> lists:nth(max(1, (Q * N + 99) div 100), Sorted) end,
    #{n => N, p50 => Pick(50), p90 => Pick(90), p99 => Pick(99),
      max => lists:last(Sorted), mean => lists:sum(Sorted) / N}.
window(Samples, Start, End) ->
    true = Start =< End,
    [E-S || {S,E} <- Samples, S < End, E > Start, E > S].
row(Name, Messages, Seconds, Samples, Correct, Note) ->
    case Correct of true -> ok; _ -> error({benchmark_failed, Name}) end,
    true = is_integer(Messages) andalso Messages >= 0,
    true = is_number(Seconds) andalso Seconds >= 0,
    Stats = lat_stats(Samples),
    Tput = case Seconds > 0 of true -> Messages / Seconds; false -> 0.0 end,
    io:format("[erlang] ~s | msgs=~b | wall=~.6fs | tput=~.1f/s | stats_us=~w | correct=true~n  note: ~s~n",
              [Name, Messages, Seconds * 1.0, Tput, Stats, Note]), ok.

start_probes(Actor, Job, Expected) ->
    Parent = self(), Token = make_ref(),
    Worker = {Pid, Tag} = work(fun() ->
        S = now_us(), expect(ask(Actor, Job, 30000), Expected), E = now_us(),
        Parent ! {probe_ready, Token}, probes(Actor, Job, Expected, [{S, E}], 1)
    end),
    receive
        {probe_ready, Token} -> Worker;
        {Tag, Pid, {error, C, R, Stack}} ->
            normal_down(Pid, mon(Pid), deadline(10000)), erlang:raise(C, R, Stack)
    after 30000 -> error(probe_start_timeout) end.
probes(A, Job, Expected, Samples, Count) ->
    receive stop_probes -> lists:reverse(Samples)
    after 0 ->
        S = now_us(), expect(ask(A, Job, 30000), Expected), E = now_us(),
        true = Count < 1000000,
        receive stop_probes -> lists:reverse([{S, E} | Samples])
        after 5 -> probes(A, Job, Expected, [{S, E} | Samples], Count + 1) end
    end.
stop_probes({Pid, _} = W, D) -> Pid ! stop_probes, join(W, D).
