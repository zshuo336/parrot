%% Unit tests of the engine reused inside each isolated guest.
-module(test_instance_lifecycle).
-export([run/0]).
run() ->
    true=register(instance_contract_observer,self()),
    Tests=[{"failed pool initialization releases all private state",fun(_) ->
        {failed,16#0A06,_}=deploy(<<"bad">>,{pool,2},<<"fail-second">>),
        Entries=created(2),[dead(P,T) || {_,P,T} <- Entries],
        {failed,16#0A03,_}=parrot_gw:admin_status(<<"/user/bad">>) end},
      {"failed replacement retains the prior instance state",fun(_) ->
        {deployed,_}=deploy(<<"keep">>,singleton,undefined),[{_,Old,Tab}]=created(1),
        {<<"count">>,<<1:64/little>>}=call(<<"/user/keep">>,<<"inc">>),
        {failed,16#0A06,_}=deploy(<<"keep">>,{pool,2},<<"fail-second">>),
        [dead(P,T) || {_,P,T} <- created(2)],
        true=is_process_alive(Old),true=ets:info(Tab)=/=undefined,
        {<<"count">>,<<1:64/little>>}=call(<<"/user/keep">>,<<"count">>) end},
      {"stop joins a busy instance and removes its tables",fun(_) ->
        {Pid,Tab,Task}=busy(),stopped=parrot_gw:admin_stop(<<"/user/work">>),
        dead(Pid,Tab),task_error(Task),{failed,16#0A03,_}=parrot_gw:admin_status(<<"/user/work">>) end},
      {"drain closes admission and waits for accepted work",fun(_) ->
        {Pid,Tab,Task}=busy(),D=async(fun() -> parrot_gw:admin_drain(<<"/user/work">>,2000) end),
        until(fun() -> parrot_gw:admin_status(<<"/user/work">>)=:=
            {status_reply,[{<<"/user/work">>,<<"draining">>,<<"1">>}]} end,500),
        rejected=try call(<<"/user/work">>,<<"inc">>),accepted catch error:component_stopped -> rejected end,
        Pid!resume,{ok,{<<"count">>,<<1:64/little>>}}=result(Task),
        {ok,{drained_reply,1,0}}=result(D),dead(Pid,Tab) end},
      {"drain deadline reports aborted work and confirms death",fun(_) ->
        {Pid,Tab,Task}=busy(),{drained_reply,0,1}=parrot_gw:admin_drain(<<"/user/work">>,10),
        dead(Pid,Tab),task_error(Task) end},
      {"caller cancellation cannot destroy accepted component work",fun(_) ->
        {Pid,Tab,Task}=busy(),exit(element(2,Task),kill),Pid!resume,
        {<<"count">>,<<1:64/little>>}=call(<<"/user/work">>,<<"count">>),
        true=is_process_alive(Pid),true=ets:info(Tab)=/=undefined end},
      {"instance death is failed and does not kill its sibling",fun(_) ->
        {deployed,_}=deploy(<<"pool">>,{pool,2},undefined),Entries=created(2),
        [{_,P0,T0},{_,P1,T1}]=lists:sort(Entries),
        rejected=try call(<<"/user/pool-0">>,<<"die">>),accepted catch error:component_stopped -> rejected end,
        dead(P0,T0),true=is_process_alive(P1),true=ets:info(T1)=/=undefined,
        {status_reply,[{<<"/user/pool-0">>,<<"failed">>,<<"1">>}]}=parrot_gw:admin_status(<<"/user/pool-0">>),
        {<<"count">>,<<0:64/little>>}=call(<<"/user/pool-1">>,<<"count">>) end},
      {"gateway owner death kills busy instances even with trap_exit",fun(Root) ->
        {Pid,Tab,Task}=busy_trapping(),exit(Root,kill),dead(Pid,Tab),task_error(Task),
        until(fun() -> whereis(parrot_component_runtime)=:=undefined end,500) end},
      {"coordinator death kills busy instances even with trap_exit",fun(_) ->
        {Pid,Tab,Task}=busy_trapping(),exit(whereis(parrot_component_runtime),kill),
        dead(Pid,Tab),task_error(Task) end},
      {"component count budget survives caller cancellation and is reusable",fun(_) ->
        {Pid,_Tab,First}=busy(),Gate=gate(<<"work">>),
        Jobs=[async(fun() -> call(<<"/user/work">>,<<"inc">>) end) || _ <- lists:seq(1,63)],
        until(fun() -> maps:get(count,sys:get_state(Gate))=:=64 end,500),
        [exit(element(2,J),kill) || J <- Jobs],
        rejected=try call(<<"/user/work">>,<<"inc">>),accepted catch error:component_overloaded -> rejected end,
        64=maps:get(count,sys:get_state(Gate)),Pid!resume,result(First),
        until(fun() -> maps:get(count,sys:get_state(Gate))=:=0 end,1000),
        {<<"count">>,<<64:64/little>>}=call(<<"/user/work">>,<<"count">>),
        {<<"count">>,<<65:64/little>>}=call(<<"/user/work">>,<<"inc">>) end},
      {"component byte budget includes payloads",fun(_) -> byte_budget(payload) end},
      {"component byte budget includes type keys",fun(_) -> byte_budget(key) end},
      {"management count budget is retained after caller cancellation",fun(_) ->
        {Pid,_Tab,First}=busy(),D=async(fun() -> parrot_gw:admin_drain(<<"/user/work">>,5000) end),
        until(fun() -> maps:get(count,sys:get_state(parrot_component_runtime))=:=1 end,500),
        Jobs=[async(fun() -> parrot_gw:admin_stop(<<"/user/absent">>) end) || _ <- lists:seq(1,63)],
        until(fun() -> maps:get(count,sys:get_state(parrot_component_runtime))=:=64 end,500),
        [exit(element(2,J),kill) || J <- Jobs],
        {failed,10,_}=parrot_gw:admin_stop(<<"/user/absent">>),
        64=maps:get(count,sys:get_state(parrot_component_runtime)),Pid!resume,result(First),
        {ok,{drained_reply,1,0}}=result(D),
        until(fun() -> maps:get(count,sys:get_state(parrot_component_runtime))=:=0 end,1000),
        {deployed,_}=deploy(<<"fresh">>,singleton,undefined),created(1) end},
      {"management byte budget rejects direct API oversized config",fun(_) ->
        {failed,10,_}=deploy(<<"large">>,singleton,binary:copy(<<0>>,16*1024*1024)),
        {failed,16#0A03,_}=parrot_gw:admin_status(<<"/user/large">>) end},
      {"initialization deadline retires trapping user code",fun(_) ->
        Job=async(fun() -> deploy(<<"blocked">>,singleton,<<"block-init">>) end),
        [{_,Pid,Tab}]=created(1),
        {ok,{failed,16#0A06,_}}=result_long(Job),dead(Pid,Tab) end},
      {"owner death interrupts blocked initialization",fun(Root) ->
        Job=async(fun() -> deploy(<<"blocked">>,singleton,<<"block-init">>) end),
        [{_,Pid,Tab}]=created(1),exit(Root,kill),dead(Pid,Tab),
        {error,exit,{killed,{gen_server,call,_}}}=result(Job) end},
      {"deployment bounds reject before path expansion or initialization",fun(_) ->
        [begin
            {failed,16#0A06,_}=parrot_gw:admin_deploy({Name,<<"1">>,{beam,<<"test_instance_fixture">>,undefined},Policy,undefined})
         end || {Name,Policy} <- [{<<>>,singleton},{<<"a/b">>,singleton},
             {<<"a?b">>,singleton},{<<"a b">>,singleton},{binary:copy(<<"x">>,257),{pool,4096}},
             {<<"zero">>,{pool,0}},{<<"huge">>,{pool,4097}}]],
        []=ets:tab2list(parrot_admin_components),
        Max=binary:copy(<<"x">>,256),{deployed,_}=deploy(Max,singleton,undefined),created(1),
        {deployed,_}=deploy(unicode:characters_to_binary("实例"),singleton,undefined),created(1) end},
      {"path collision is rejected without disturbing the existing actor",fun(_) ->
        {deployed,_}=deploy(<<"a-0">>,singleton,undefined),created(1),
        {<<"count">>,<<1:64/little>>}=call(<<"/user/a-0">>,<<"inc">>),
        {failed,16#0A06,_}=deploy(<<"a">>,{pool,2},undefined),
        {<<"count">>,<<1:64/little>>}=call(<<"/user/a-0">>,<<"count">>) end},
      {"separate dash-named component is not retired as a pool",fun(_) ->
        {deployed,_}=deploy(<<"a">>,singleton,undefined),created(1),
        {deployed,_}=deploy(<<"a-other">>,singleton,undefined),created(1),
        stopped=parrot_gw:admin_stop(<<"/user/a">>),
        {<<"count">>,<<0:64/little>>}=call(<<"/user/a-other">>,<<"count">>) end}],
    Failed=lists:foldl(fun({Name,F},N) ->
        try fixture(F),io:format("INSTANCE_LIFECYCLE_OK ~s~n",[Name]),N
        catch C:R:St -> io:format("INSTANCE_LIFECYCLE_FAIL ~s ~p:~p ~p~n",[Name,C,R,St]),N+1 end
    end,0,Tests),
    unregister(instance_contract_observer),
    case Failed of 0 -> ok; _ -> erlang:error({lifecycle_failures,Failed}) end.
fixture(F) ->
    Observer=self(),{Root,Mon}=spawn_monitor(fun() -> parrot_gw:admin_init(),Observer!{ready,self()},receive stop -> ok end end),
    receive {ready,Root} -> ok after 1000 -> erlang:error(root_timeout) end,
    try F(Root)
    after
        exit(Root,kill),receive {'DOWN',Mon,process,Root,_} -> ok after 1000 -> erlang:error(root_cleanup_timeout) end,
        until(fun() -> whereis(parrot_component_runtime)=:=undefined end,1000),
        flush_created()
    end.
deploy(Name,Policy,Config) ->
    parrot_gw:admin_init(),parrot_component_runtime:ensure(ets:info(parrot_admin_components,owner)),
    Paths=parrot_gw:admin_instance_paths(Name,Policy),
    Result=parrot_component_runtime:deploy(Name,<<"1">>,Paths,
        fun() -> parrot_gw:load_beam(<<"test_instance_fixture">>,undefined) end,Config),
    case Result of
        {deployed,_} -> Result;
        {error,component_management_overloaded} -> {failed,10,<<"capacity">>};
        {error,_} -> {failed,16#0A06,<<"initialization">>}
    end.
call(Path,Key) -> parrot_gw:service(Path,Key,<<>>).
created(0) -> [];
created(N) -> receive {created,P,Pid,T} -> [{P,Pid,T}|created(N-1)] after 1000 -> erlang:error(missing_instance_creation) end.
flush_created() -> receive {created,_,P,T} -> dead(P,T),flush_created() after 0 -> ok end.
async(F) ->
    Parent=self(),Tag=make_ref(),Pid=spawn(fun() ->
        R=try {ok,F()} catch C:E -> {error,C,E} end,Parent!{Tag,R}
    end),{Tag,Pid}.
result({Tag,_}) -> receive {Tag,R} -> R after 5000 -> erlang:error(task_timeout) end.
task_error(Task) -> {error,error,component_stopped}=result(Task).
busy() -> busy(false).
busy_trapping() -> busy(true).
busy(Trap) ->
    {deployed,_}=deploy(<<"work">>,singleton,undefined),[{_,Pid,Tab}]=created(1),
    case Trap of true -> call(<<"/user/work">>,<<"trap">>); false -> ok end,
    Task=async(fun() -> call(<<"/user/work">>,<<"block">>) end),
    receive {entered,Pid} -> ok after 1000 -> erlang:error(handler_did_not_enter) end,{Pid,Tab,Task}.
dead(Pid,Tab) ->
    Mon=erlang:monitor(process,Pid),receive {'DOWN',Mon,process,Pid,_} -> ok after 5000 -> erlang:error(instance_alive) end,
    undefined=ets:info(Tab).
until(F,0) -> true=F();
until(F,N) -> case F() of true -> ok; false -> timer:sleep(2),until(F,N-1) end.

gate(Name) ->
    [{Name,_,[Path],{owned,_,Entries}}]=ets:lookup(parrot_admin_components,Name),maps:get(pid,maps:get(Path,Entries)).
byte_budget(Part) ->
    {_Pid,_Tab,_First}=busy(),Gate=gate(<<"work">>),Blob=binary:copy(<<0>>,1024*1024),
    {Key,Body}=case Part of payload -> {<<"inc">>,Blob}; key -> {Blob,<<>>} end,
    Jobs=[async(fun() -> parrot_gw:service(<<"/user/work">>,Key,Body) end) || _ <- lists:seq(1,15)],
    until(fun() -> maps:get(count,sys:get_state(Gate))=:=16 end,500),
    rejected=try parrot_gw:service(<<"/user/work">>,Key,Body),accepted catch error:component_overloaded -> rejected end,
    [exit(element(2,J),kill) || J <- Jobs],
    16=maps:get(count,sys:get_state(Gate)),stopped=parrot_gw:admin_stop(<<"/user/work">>).
result_long({Tag,_}) -> receive {Tag,R} -> R after 7000 -> erlang:error(initialization_deadline_failed) end.
