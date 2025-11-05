%% Actual work, exact results, bounded workers and per-stage retirement.
%% Scaling is an explicit workload parameter, never a claim of time equivalence.
-module(parrot_bench).
-export([main/1, config/2, keys/1, run/2]).
-import(bench_runtime, [actor/0, ask/3, expect/2, now_us/0, check_state/3]).
-define(TIMEOUT, 3600000).

main(["keys", Mode, Scale, Path]) ->
    C = config(Mode, Scale),
    ok = file:write_file(Path, [io_lib:format("~b ~b~n", [I,S]) || {I,S} <- keys(C)]);
main(["run", Mode, Scale, Path]) ->
    C = config(Mode, Scale), {ok, Entries} = file:consult(Path), O = maps:from_list(Entries),
    expect(length(Entries), map_size(O)), expect(lists:sort(maps:keys(O)), keys(C)),
    io:format("oracle_keys=~b mode=~s scale=~b schedulers=~b~n",
              [map_size(O), Mode, maps:get(scale,C), erlang:system_info(schedulers_online)]),
    run(C, O);
main(_) -> error({usage, "keys|run full|contract POSITIVE_SCALE FILE"}).
config(Mode, ScaleText) ->
    true = lists:member(Mode, ["full", "contract"]),
    Scale = list_to_integer(ScaleText), true = Scale >= 1 andalso Scale =< 1000000,
    #{mode => Mode, scale => Scale}.
n(#{mode := "full"}, K) -> maps:get(K, #{warm=>3000,seq=>1000,c1=>8,m1=>1000,c2=>64,m2=>200,
    tell=>100000,cpu=>200,cpua=>64,cpum=>25,flood=>500000,herd=>20000,ioa=>64,iom=>20,ms=>10,
    longa=>8,meda=>4,medm=>10,fifom=>6,tiny=>4000,ping=>10000,chain=>20000,slowa=>8,slowm=>5000,
    waves=>5,wave=>1000});
n(#{mode := "contract"}, K) -> maps:get(K, #{warm=>10,seq=>20,c1=>3,m1=>10,c2=>4,m2=>8,
    tell=>100,cpu=>8,cpua=>4,cpum=>3,flood=>200,herd=>40,ioa=>4,iom=>3,ms=>1,
    longa=>3,meda=>2,medm=>3,fifom=>3,tiny=>20,ping=>20,chain=>30,slowa=>3,slowm=>10,
    waves=>2,wave=>12}).
i(C, N) ->
    Work = max(1, N div maps:get(scale,C)),
    case maps:get(mode,C) of "full" -> Work; "contract" -> min(64, Work) end.
seq(N) -> lists:seq(1,N).
keys(C) -> lists:usort(
    [{i(C,200000),S} || S <- seq(n(C,cpu))] ++
    [{i(C,200000),A*1000+K} || A <- seq(n(C,cpua)), K <- seq(n(C,cpum))] ++
    [{i(C,50000000)*10,1},{i(C,2000000000),7},{i(C,1000000000),7},
     {i(C,30000000000),1},{i(C,30000000000),7},{i(C,1500000000),7}] ++
    [{i(C,57200000000),S} || S <- seq(n(C,longa))] ++
    [{i(C,2400000000),S} || S <- seq(max(n(C,medm),n(C,fifom)))] ++
    [{i(C,1000),S} || S <- lists:seq(0,n(C,tiny))] ++
    [{i(C,170000),A*10000+K} || A <- lists:seq(0,n(C,slowa)-1), K <- seq(n(C,slowm))]).
v(O,I,S) -> maps:get({I,S},O).
metrics(N,T,Samples,Note) -> #{messages=>N,seconds=>(now_us()-T)/1000000,samples=>Samples,note=>Note}.
measured(A, Job, Expected) -> S=now_us(), expect(ask(A,Job,?TIMEOUT),Expected), now_us()-S.
stage(Name,F) ->
    {M,Retired} = bench_runtime:scope(F),
    Note=io_lib:format("~s; retired_processes=~b",[maps:get(note,M),Retired]),
    bench_runtime:row(Name,maps:get(messages,M),maps:get(seconds,M),maps:get(samples,M),true,Note).
run(C,O) ->
    {ok,_} = bench_runtime:scope(fun() -> A=actor(),
        [expect(ask(A,{echo,K},10000),K) || K<-seq(n(C,warm))],
        check_state(A,n(C,warm),lists:sum(seq(n(C,warm)))) end),
    Stages=[{"seq-ask-echo",fun()->echo(C,seq) end},
      {"conc-ask-echo-low-concurrency",fun()->concurrent_echo(C,c1,m1) end},
      {"conc-ask-echo-high-concurrency",fun()->concurrent_echo(C,c2,m2) end},
      {"tell-echo",fun()->tell_echo(C) end},
      {"cpu-serial",fun()->cpu_serial(C,O) end},
      {"cpu-parallel",fun()->cpu_parallel(C,O) end},
      {"flood-tell",fun()->flood(C) end},
      {"ask-timeout-short",fun()->short_timeout(C,O) end},
      {"send-after-stop",fun()->after_stop() end},
      {"herd-actors",fun()->herd(C) end},
      {"longrun",fun()->longrun(C,O) end},
      {"starve-echo-during-longrun",fun()->starve(C,O) end},
      {"io-serial-per-actor",fun()->io_work(C) end},
      {"mixed-long-cpu-plus-incoming",fun()->mixed(C,O) end},
      {"mixed-same-actor-fifo",fun()->fifo(C,O) end},
      {"chunked-vs-solid-longrun",fun()->chunked(C,O) end},
      {"pingpong-rtt",fun()->pingpong(C) end},
      {"self-chain-ask-tell",fun()->chain(C) end},
      {"slow-consumer",fun()->slow(C,O) end},
      {"spawn-stop-storm",fun()->storm(C) end}],
    20=length(Stages), [stage(Name,F) || {Name,F}<-Stages],
    io:format("ERLANG_BENCH_PASS mode=~s scale=~b rows=20~n",[maps:get(mode,C),maps:get(scale,C)]), ok.

echo(C,Key) -> A=actor(), N=n(C,Key), T=now_us(),
    L=[measured(A,{echo,K},K)||K<-seq(N)],check_state(A,N,lists:sum(seq(N))),metrics(N,T,L,"").
concurrent_echo(C,CK,MK) ->
    A=actor(), Workers=n(C,CK), N=n(C,MK), T=now_us(),
    Lists=bench_runtime:parallel([fun()->[measured(A,{echo,K+P},K+P)||K<-seq(N)] end
                                  ||P<-lists:seq(0,Workers-1)],?TIMEOUT),
    check_state(A,Workers*N,lists:sum([K+P||P<-lists:seq(0,Workers-1),K<-seq(N)])),
    metrics(Workers*N,T,lists:append(Lists),"joined all producers").
tell_echo(C) -> A=actor(),N=n(C,tell),T=now_us(),
    [A!{tell,{echo,K}}||K<-seq(N)],check_state(A,N,lists:sum(seq(N))),metrics(N,T,[],"exact count and checksum").
cpu_serial(C,O) -> A=actor(),N=n(C,cpu),I=i(C,200000),T=now_us(),
    L=[measured(A,{cpu,I,S},v(O,I,S))||S<-seq(N)],check_state(A,N,lists:sum([v(O,I,S)||S<-seq(N)])),
    metrics(N,T,L,io_lib:format("iterations_per_message=~b",[I])).
cpu_parallel(C,O) ->
    Actors=[{K,actor()}||K<-seq(n(C,cpua))],N=n(C,cpum),I=i(C,200000),T=now_us(),
    L=bench_runtime:parallel([fun()->
        Lat=[measured(A,{cpu,I,P*1000+K},v(O,I,P*1000+K))||K<-seq(N)],
        check_state(A,N,lists:sum([v(O,I,P*1000+K)||K<-seq(N)])),Lat end||{P,A}<-Actors],?TIMEOUT),
    metrics(length(Actors)*N,T,lists:append(L),io_lib:format("actors=~b iterations_per_message=~b",[length(Actors),I])).
flood(C) ->
    A=actor(),N=n(C,flood) div 4,T=now_us(),
    Ends=bench_runtime:parallel([fun()->[A!{tell,{echo,K+P*100000}}||K<-seq(N)],
        Sent=now_us(),_ = ask(A,state,?TIMEOUT),Sent end||P<-lists:seq(0,3)],?TIMEOUT),
    check_state(A,4*N,lists:sum([K+P*100000||P<-lists:seq(0,3),K<-seq(N)])),
    metrics(4*N,T,[],io_lib:format("all_producer_sends_us=~b; per-producer drain fences",[lists:max(Ends)-T])).
short_timeout(C,O) ->
    A=actor(),I=i(C,50000000)*10,{Token,Heavy}=bench_runtime:gate(A,{cpu,I,1}),T=now_us(),
    Probe=bench_runtime:begin_ask(A,{echo,1}),
    try bench_runtime:await(Probe,5) of _->error(timeout_was_not_enforced) catch exit:timeout->ok end,
    bench_runtime:release(A,Token),{completed,R,_,_}=bench_runtime:await(Heavy,?TIMEOUT),expect(R,v(O,I,1)),
    check_state(A,2,R+1),metrics(2,T,[],"probe enqueued while heavy handler is gated; timeout retires reply alias").
after_stop() -> A=actor(),bench_runtime:retire([A]),T=now_us(),
    try ask(A,{echo,1},10000) of _->error(dead_actor_replied)
    catch error:{actor_down,noproc}->ok end,metrics(1,T,[],"normal DOWN observed before send; monitored request rejects noproc").
herd(C) -> T=now_us(),Actors=[actor()||_<-seq(n(C,herd))],Spawn=now_us()-T,
    [A!{tell,{echo,1}}||A<-Actors],[check_state(A,1,1)||A<-Actors],
    metrics(length(Actors),T,[],io_lib:format("spawn_us=~b; every actor processed its message",[Spawn])).
longrun(C,O) -> A=actor(),I=i(C,2000000000),T=now_us(),L=measured(A,{cpu,I,7},v(O,I,7)),
    check_state(A,1,v(O,I,7)),metrics(1,T,[L],io_lib:format("iterations=~b",[I])).
starve(C,O) ->
    Heavy=actor(),Probe=actor(),I=i(C,1000000000),
    Baseline=[measured(Probe,{echo,1},1)||_<-seq(100)],
    {Token,Request}=bench_runtime:gate(Heavy,{cpu,I,7}),
    Worker=bench_runtime:start_probes(Probe,{echo,1},1),T=now_us(),
    bench_runtime:release(Heavy,Token),{completed,R,Start,End}=bench_runtime:await(Request,?TIMEOUT),
    expect(R,v(O,I,7)),Samples=bench_runtime:stop_probes(Worker,bench_runtime:deadline(30000)),
    During=bench_runtime:window(Samples,Start,End),
    case maps:get(mode,C) of "full"->true=length(During)>0; "contract"->ok end,
    check_state(Heavy,1,R),check_state(Probe,100+length(Samples),100+length(Samples)),
    metrics(length(During),T,During,io_lib:format("actual_compute_us=~b baseline_us=~w; only overlapping probe intervals reported",[End-Start,bench_runtime:lat_stats(Baseline)])).
io_work(C) ->
    Actors=[actor()||_<-seq(n(C,ioa))],N=n(C,iom),Ms=n(C,ms),T=now_us(),
    [[A!{tell,{io,Ms,K}}||K<-seq(N)]||A<-Actors],
    [check_state(A,N,lists:sum(seq(N)))||A<-Actors],
    metrics(length(Actors)*N,T,[],io_lib:format("per-actor serial sleeps; actors=~b messages_each=~b sleep_ms=~b",[length(Actors),N,Ms])).
mixed(C,O) ->
    Long=[{K,actor()}||K<-seq(n(C,longa))],Medium=[actor()||_<-seq(n(C,meda))],Probe=actor(),
    LI=i(C,57200000000),MI=i(C,2400000000),Tiny=i(C,1000),N=n(C,medm),
    Gates=[{K,A,bench_runtime:gate(A,{cpu,LI,K})}||{K,A}<-Long],
    Probes=bench_runtime:start_probes(Probe,{cpu,Tiny,1},v(O,Tiny,1)),T=now_us(),
    [bench_runtime:release(A,Token)||{_,A,{Token,_}}<-Gates],
    Workers=[bench_runtime:work(fun()->
        [begin A!{tell,{cpu,MI,K}},timer:sleep(50) end||K<-seq(N)],
        check_state(A,N,lists:sum([v(O,MI,K)||K<-seq(N)])),ok end)||A<-Medium],
    [begin {completed,R,_,_}=bench_runtime:await(Request,?TIMEOUT),expect(R,v(O,LI,K)),check_state(A,1,R) end
     ||{K,A,{_,Request}}<-Gates],
    D=bench_runtime:deadline(?TIMEOUT),[bench_runtime:join(W,D)||W<-Workers],End=now_us(),
    Samples=bench_runtime:stop_probes(Probes,bench_runtime:deadline(30000)),
    check_state(Probe,length(Samples),length(Samples)*v(O,Tiny,1)),
    During=bench_runtime:window(Samples,T,End),true=length(During)>0,
    metrics(length(Long)+length(Medium)*N,T,During,
        io_lib:format("long_iters=~b medium_iters=~b; all long and medium work drained; samples overlap admission/compute/drain window",[LI,MI])).
fifo(C,O) ->
    A=actor(),LI=i(C,30000000000),MI=i(C,2400000000),TI=i(C,1000),N=n(C,fifom),Tiny=n(C,tiny),
    {Token,Heavy}=bench_runtime:gate(A,{cpu,LI,1}),T=now_us(),
    [A!{tell,{cpu,MI,K}}||K<-seq(N)],[A!{tell,{cpu,TI,K}}||K<-seq(Tiny)],
    Probe=bench_runtime:begin_ask(A,{cpu,TI,0}),Start=now_us(),
    bench_runtime:release(A,Token),{completed,R,_,_}=bench_runtime:await(Heavy,?TIMEOUT),expect(R,v(O,LI,1)),
    expect(bench_runtime:await(Probe,?TIMEOUT),v(O,TI,0)),Tail=now_us()-Start,
    Sum=R+lists:sum([v(O,MI,K)||K<-seq(N)])+lists:sum([v(O,TI,K)||K<-lists:seq(0,Tiny)]),
    check_state(A,2+N+Tiny,Sum),metrics(2+N+Tiny,T,[Tail],"probe sent before release, behind all FIFO work; probe included in message count").
chunked(C,O) ->
    A=actor(),B=actor(),Solid=i(C,30000000000),Chunk=i(C,1500000000),T=now_us(),
    LS=measured(A,{cpu,Solid,7},v(O,Solid,7)),
    LC=[measured(B,{cpu,Chunk,7},v(O,Chunk,7))||_<-seq(20)],
    check_state(A,1,v(O,Solid,7)),check_state(B,20,20*v(O,Chunk,7)),
    metrics(21,T,[LS,lists:sum(LC)],io_lib:format("one ~b-iteration job vs twenty independently seeded ~b-iteration jobs; distinct results verified",[Solid,Chunk])).
pingpong(C) ->
    A=actor(),B=actor(),N=n(C,ping),T=now_us(),
    L=[begin S=now_us(),expect(ask(A,{echo,K},?TIMEOUT),K),expect(ask(B,{echo,K},?TIMEOUT),K),now_us()-S end||K<-seq(N)],
    [check_state(P,N,lists:sum(seq(N)))||P<-[A,B]],
    metrics(2*N,T,[X||{K,X}<-lists:zip(seq(N),L),K rem 10=:=0],"two sequential client asks; not actor-to-actor forwarding").
chain(C) -> A=actor(),N=n(C,chain),T=now_us(),
    [begin expect(ask(A,{echo,K},?TIMEOUT),K),A!{tell,{echo,K+1}} end||K<-seq(N)],
    check_state(A,2*N,2*lists:sum(seq(N))+N),metrics(2*N,T,[],"same-sender ask/tell order and exact checksum").
slow(C,O) ->
    A=actor(),N=n(C,slowm),Workers=n(C,slowa),I=i(C,170000),T=now_us(),
    Ends=bench_runtime:parallel([fun()->[A!{tell,{cpu,I,P*10000+K}}||K<-seq(N)],
        Sent=now_us(),_=ask(A,state,?TIMEOUT),Sent end||P<-lists:seq(0,Workers-1)],?TIMEOUT),
    check_state(A,N*Workers,lists:sum([v(O,I,P*10000+K)||P<-lists:seq(0,Workers-1),K<-seq(N)])),
    metrics(N*Workers,T,[],io_lib:format("all_producer_sends_us=~b iterations_per_message=~b; all workers joined and messages drained",[lists:max(Ends)-T,I])).
storm(C) -> N=n(C,wave),W=n(C,waves),T=now_us(),
    [begin Actors=[actor()||_<-seq(N)],
        [expect(ask(A,{echo,K},?TIMEOUT),K)||{A,K}<-lists:zip(Actors,seq(N))],
        bench_runtime:retire(Actors) end||_<-seq(W)],
    metrics(2*N*W,T,[],io_lib:format("~b actors each echoed then observed normal DOWN; echo+stop counted",[N*W])).
