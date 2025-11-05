%% Real sockets distinguish shared gateway lifetime from a transient API caller.
-module(test_network_owner).
-export([run/0]).

run() ->
    Cases=[refusal_contract,{tree,normal},{tree,shutdown},{tree,kill},{tree,lease_kill},
           {registrar,active,shutdown},{registrar,active,kill},
           {registrar,backoff,shutdown},{registrar,backoff,kill},
           {registrar,active,lease_kill},{registrar,backoff,lease_kill},
           {initial_backoff,shutdown},{initial_backoff,kill},{initial_backoff,lease_kill},
           {detached_caller,normal},{detached_caller,kill},driver_isolation],
    Failed=lists:foldl(fun(Case,N) ->
        put(case_sockets,[]),put(case_processes,[]),
        try exercise(Case),io:format("NETWORK_OWNER_OK ~p~n",[Case]),N
        catch C:R:Stack -> io:format("NETWORK_OWNER_FAIL ~p ~p:~p ~p~n",[Case,C,R,Stack]),N+1
        after cleanup() end
    end,0,Cases),
    case Failed of 0 -> ok; _ -> error({network_owner_failures,Failed}) end.

exercise(refusal_contract) ->
    ok=refused_result({error,econnrefused}),
    ok=refused_result({error,econnreset}),
    [expect_error(fun() -> refused_result({error,Reason}) end,
        {unexpected_listener_result,{error,Reason}}) || Reason <- [timeout,ehostunreach,enetunreach]],
    {ok,L}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),track_socket(L),
    {ok,Port}=inet:port(L),
    expect_error(fun() -> refused(Port) end,{contract,listener_must_be_closed_after_owner_death}),
    {ok,S}=gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}],500),track_socket(S),
    expect_error(fun() -> refused_result({ok,S}) end,{contract,listener_must_not_accept_after_owner_death}),
    until(fun() -> erlang:port_info(S)=:=undefined end,3000,rejected_probe_socket_must_close),
    ok=gen_tcp:close(L),refused(Port);

exercise({tree,Cause}) ->
    Self=self(),
    {Root,Port,Listener}=gateway(fun(K,P) ->
        Self!{blocked_work,self()},receive resume -> parrot_gw:service(K,P) end
    end),
    Parent=client(Port),Driver=opposite_owner(Parent),
    Sup=task_supervisor(Driver),
    ok=gen_tcp:send(Parent,parrot_gw:build_frame(16#10,100,<<"/user/test">>,<<"bin:u:Ping">>,
        <<0:32/little,8:64/little>>)),
    Work=receive {blocked_work,W} -> W after 3000 -> error(connection_work_must_enter) end,
    track_process(Work),pulse(Parent),
    {Direct,DirectDriver,DirectSup}=direct(Parent),
    {ok,Half}=gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),track_socket(Half),
    ok=gen_tcp:send(Half,<<40:32/little,1>>),
    until(fun() -> opposite_owners(Half)=/=[] end,3000,half_handshake_must_be_accepted),
    HalfDriver=opposite_owner(Half),track_process(HalfDriver),
    until(fun() -> is_pid(task_supervisor_or_none(HalfDriver)) end,3000,half_handshake_must_own_tasks),
    HalfSup=task_supervisor(HalfDriver),Owner=whereis(parrot_gw_lease_owner),
    Pids=[{listener,Listener},{driver,Driver},{tasks,Sup},{work,Work},{direct_driver,DirectDriver},
          {direct_tasks,DirectSup},{handshake_driver,HalfDriver},{handshake_tasks,HalfSup},{owner,Owner}],
    observe(Cause,Pids),terminate_owner(Cause,Root,Owner),
    retired(Pids),
    [closed(S) || S <- [Parent,Direct,Half]],
    refused(Port);

exercise({registrar,Phase,Cause}) ->
    {ok,Hub}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),track_socket(Hub),
    {ok,HubPort}=inet:port(Hub),
    Root=spawn(fun() -> parrot_gw:main(["0","listen=127.0.0.1",
        "parrot=127.0.0.1:"++integer_to_list(HubPort)]) end),track_process(Root),
    {ok,S}=gen_tcp:accept(Hub,3000),track_socket(S),
    {1,1,_,_}=recv(S),Driver=opposite_owner(S),track_process(Driver),
    ok=gen_tcp:send(S,parrot_gw:build_frame(2,1,<<>>,<<>>,
        iolist_to_binary(parrot_gw:handshake_ack_body("owner-hub")))),
    pulse(S),Sup=task_supervisor(Driver),
    [{Port,Listener}]=listeners_except(Hub),track_process(Listener),
    Owner=whereis(parrot_gw_lease_owner),
    case Phase of
        active -> ok;
        backoff ->
            ok=gen_tcp:close(S),
            until(fun() -> in_backoff(Driver) end,3000,registrar_must_enter_real_reconnect_backoff),
            io:format("NETWORK_OWNER_BACKOFF ~p ~p~n",[Driver,process_info(Driver,[status,current_function,current_stacktrace])]),
            check(is_process_alive(Root),root_must_remain_alive_during_backoff)
    end,
    Pids=[{listener,Listener},{registrar,Driver},{owner,Owner}]++
         case Phase of active -> [{tasks,Sup}]; backoff -> [] end,
    observe({Phase,Cause},Pids),terminate_owner(Cause,Root,Owner),retired(Pids),
    case Phase of active -> closed(S); backoff -> ok end,
    refused(Port),
    check(gen_tcp:accept(Hub,1100)=:={error,timeout},registrar_must_not_redial_after_owner_death);

exercise({initial_backoff,Cause}) ->
    {ok,Unused}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),
    {ok,HubPort}=inet:port(Unused),ok=gen_tcp:close(Unused),
    Root=spawn(fun() -> parrot_gw:main(["0","listen=127.0.0.1",
        "parrot=127.0.0.1:"++integer_to_list(HubPort)]) end),track_process(Root),
    until(fun() -> [P || P <- processes(),in_backoff(P)]=/=[] end,3000,first_dial_must_enter_real_backoff),
    [Driver]=[P || P <- processes(),in_backoff(P)],track_process(Driver),
    [{Port,Listener}]=listeners_except(none),track_process(Listener),
    check(ets:info(parrot_gw_metrics,owner)=:=Root,initial_backoff_must_use_actual_table_owner),
    Owner=whereis(parrot_gw_lease_owner),
    {monitors,LeaseMonitors}=process_info(Owner,monitors),Snapshot=parrot_gw:admin_metrics(),
    check(LeaseMonitors=:=[{process,Root}] andalso maps:get(connections,Snapshot)=:=0 andalso
        maps:get(handshakes_ok,Snapshot)=:=0 andalso maps:get(handshakes_failed,Snapshot)=:=0,
        initial_backoff_must_precede_first_lease),
    Pids=[{listener,Listener},{registrar,Driver},{owner,Owner}],
    io:format("NETWORK_OWNER_INITIAL_BACKOFF ~p ~p~n",[Driver,process_info(Driver,[status,current_function,current_stacktrace])]),
    observe({initial_backoff,Cause},Pids),terminate_owner(Cause,Root,Owner),retired(Pids),refused(Port);

exercise({detached_caller,Cause}) ->
    {Root,_First,_Listener}=gateway(),Self=self(),
    {Caller,Ref}=spawn_monitor(fun() ->
        {ok,P}=parrot_gw:start(0),Self!{detached_port,self(),P},receive return -> ok end
    end),
    Port=receive {detached_port,Caller,P} -> P after 3000 -> error(detached_start_timeout) end,
    Listener=listener(Port),track_process(Listener),
    case Cause of normal -> Caller!return; kill -> exit(Caller,kill) end,
    Expected=case Cause of normal -> normal; kill -> killed end,
    receive {'DOWN',Ref,process,Caller,Expected} -> ok after 3000 -> error(detached_caller_must_exit) end,
    check(ets:info(parrot_gw_metrics,owner)=:=Root,transient_caller_must_not_own_shared_tables),
    check(is_process_alive(Listener),listener_must_survive_transient_start_caller),
    S=client(Port),pulse(S),
    check(is_process_alive(whereis(parrot_gw_lease_owner)),transient_exit_must_preserve_network_owner);

exercise(driver_isolation) ->
    {_Root,Port,Listener}=gateway(),
    A=client(Port),B=client(Port),Driver=opposite_owner(A),Sibling=opposite_owner(B),
    Owner=whereis(parrot_gw_lease_owner),Ref=monitor(process,Driver),exit(Driver,kill),
    await_down(Ref,Driver,individual_driver_must_die),closed(A),
    Sent=gen_tcp:send(B,parrot_gw:build_frame(3,91,<<>>,<<>>,<<>>)),
    check(Sent=:=ok andalso recv(B)=:={4,91,<<>>,<<>>},driver_failure_must_preserve_sibling_traffic),
    Fresh=client(Port),pulse(Fresh),
    check(lists:all(fun is_process_alive/1,[Owner,Listener,Sibling]),driver_failure_must_preserve_sibling_and_listener).

gateway() -> gateway(fun parrot_gw:service/3).
gateway(Service) ->
    Self=self(),Root=spawn(fun() ->
        {ok,Port}=parrot_gw:start(0,{127,0,0,1},Service),
        Self!{gateway_ready,self(),Port},receive normal_return -> ok end
    end),track_process(Root),
    Port=receive {gateway_ready,Root,N} -> N after 3000 -> error(gateway_start_timeout) end,
    Listener=listener(Port),track_process(Listener),
    check(ets:info(parrot_gw_metrics,owner)=:=Root,fixture_must_use_actual_shared_table_owner),
    {Root,Port,Listener}.

terminate_owner(lease_kill,Root,Owner) ->
    exit(Owner,kill),check(is_process_alive(Root),lease_owner_kill_must_leave_table_owner_alive);
terminate_owner(normal,Root,_) ->
    Ref=monitor(process,Root),Root!normal_return,await_down(Ref,Root,shared_root_must_return_normally);
terminate_owner(Cause,Root,_) ->
    Ref=monitor(process,Root),exit(Root,Cause),await_down(Ref,Root,shared_root_must_die).

observe(Cause,Pids) ->
    check(lists:all(fun({_,P}) -> is_pid(P) andalso is_process_alive(P) end,Pids),all_network_resources_must_start_alive),
    [track_process(P) || {_,P} <- Pids],
    put(observation_monitors,[{Role,P,monitor(process,P)} || {Role,P} <- Pids]),
    io:format("NETWORK_OWNER_OBSERVED ~p ~p~n",[Cause,Pids]).

retired(_Pids) ->
    Deadline=erlang:monotonic_time(millisecond)+2000,
    Missing=lists:filter(fun({_Role,P,Ref}) ->
        Remaining=max(0,Deadline-erlang:monotonic_time(millisecond)),
        receive {'DOWN',Ref,process,P,_} -> false after Remaining -> true end
    end,get(observation_monitors)),
    check(Missing=:=[],{network_resources_survived_owner,[Role || {Role,_,_} <- Missing]}).

in_backoff(Pid) ->
    case process_info(Pid,[status,current_stacktrace]) of
        [{status,waiting},{current_stacktrace,Stack}] ->
            lists:any(fun({timer,sleep,_,_}) -> true; (_) -> false end,Stack) andalso
            lists:any(fun({parrot_gw,registrar,4,_}) -> true; (_) -> false end,Stack);
        _ -> false
    end.

client(Port) ->
    {ok,S}=gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),track_socket(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,
        iolist_to_binary(parrot_gw:handshake_body("owner-peer")))),
    {2,1,_,_}=recv(S),S.

direct(Parent) ->
    {ok,L}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),track_socket(L),
    {ok,Port}=inet:port(L),Node = <<"owner-direct">>,
    Addr=list_to_binary("127.0.0.1:"++integer_to_list(Port)),
    Hint = <<(byte_size(Node)):16/little,Node/binary,(byte_size(Addr)):16/little,Addr/binary>>,
    ok=gen_tcp:send(Parent,parrot_gw:build_frame(16#24,7,<<>>,<<>>,Hint)),
    {ok,S}=gen_tcp:accept(L,3000),track_socket(S),{1,1,_,_}=recv(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(2,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_ack_body(Node)))),
    pulse(S),Driver=opposite_owner(S),track_process(Driver),{S,Driver,task_supervisor(Driver)}.

pulse(S) -> ok=gen_tcp:send(S,parrot_gw:build_frame(3,91,<<>>,<<>>,<<>>)),{4,91,_,_}=recv(S),ok.
recv(S) ->
    case gen_tcp:recv(S,4,3000) of
        {ok,<<N:32/little>>} ->
            case gen_tcp:recv(S,N,3000) of
                {ok,B} -> {ok,Ft,_,Cid,_,Key,Payload,<<>>}=parrot_gw:parse_frame(<<N:32/little,B/binary>>),
                    {Ft,Cid,Key,Payload};
                {error,R} -> {socket_error,R}
            end;
        {error,R} -> {socket_error,R}
    end.
closed(S) -> check(gen_tcp:recv(S,0,1000)=:={error,closed},owned_socket_must_close).
refused(Port) ->
    check(not lists:keymember(Port,1,listeners_except(none)),listener_must_be_closed_after_owner_death),
    refused_result(gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}],500)).
refused_result({error,econnrefused}) -> ok;
%% Linux can reset a connection racing with final kernel socket teardown.
refused_result({error,econnreset}) -> ok;
refused_result({ok,S}) -> gen_tcp:close(S),check(false,listener_must_not_accept_after_owner_death);
refused_result(Other) -> error({unexpected_listener_result,Other}).

expect_error(F,Expected) ->
    try F() of _ -> error({expected_error,Expected})
    catch error:Expected -> ok end.

opposite_owner(S) ->
    Owners=opposite_owners(S),check(length(Owners)=:=1,opposite_socket_owner_must_be_unique),hd(Owners).
opposite_owners(S) ->
    {ok,Endpoint}=inet:sockname(S),
    {ok,Peer}=inet:peername(S),
    [Pid || Port <- erlang:ports(),
        erlang:port_info(Port,name)=:={name,"tcp_inet"},(try inet:peername(Port) catch _:_ -> error end)=:={ok,Endpoint},
        (try inet:sockname(Port) catch _:_ -> error end)=:={ok,Peer},
        {connected,Pid} <- [erlang:port_info(Port,connected)]].
listener(Number) ->
    [{Number,Pid}]=[{N,P} || {N,P} <- listeners_except(none),N=:=Number],Pid.
listeners_except(Excluded) ->
    [{N,Pid} || P <- erlang:ports(),
        erlang:port_info(P,name)=:={name,"tcp_inet"},P=/=Excluded,
        {ok,{_IP,N}} <- [(try inet:sockname(P) catch _:_ -> error end)],
        (try inet:peername(P) catch _:_ -> error end)=:={error,enotconn},
        {connected,Pid} <- [erlang:port_info(P,connected)]].
task_supervisor(Pid) ->
    Sup=task_supervisor_or_none(Pid),check(is_pid(Sup),connection_must_own_task_supervisor),track_process(Sup),Sup.
task_supervisor_or_none(Pid) ->
    case process_info(Pid,dictionary) of
        {dictionary,D} -> proplists:get_value(task_sup,D);
        _ -> undefined
    end.
track_socket(S) -> put(case_sockets,[S|get(case_sockets)]),ok.
track_process(P) -> put(case_processes,[P|get(case_processes)]),ok.

cleanup() ->
    [try gen_tcp:close(S) catch _:_ -> ok end || S <- get(case_sockets)],
    Pids=lists:usort(get(case_processes)++case whereis(parrot_gw_lease_owner) of undefined -> []; P -> [P] end),
    [exit(P,kill) || P <- Pids],
    until(fun() -> lists:all(fun(P) -> not is_process_alive(P) end,Pids) andalso
        whereis(parrot_gw_lease_owner)=:=undefined andalso ets:info(parrot_gw_metrics)=:=undefined end,
        3000,fixture_cleanup_must_finish),
    case erase(observation_monitors) of
        undefined -> ok;
        Monitors -> [demonitor(Ref,[flush]) || {_,_,Ref} <- Monitors],ok
    end,
    erase(case_sockets),erase(case_processes),ok.
await_down(Ref,Pid,Reason) -> receive {'DOWN',Ref,process,Pid,_} -> ok after 3000 -> error(Reason) end.
until(F,Timeout,Reason) -> until_deadline(F,erlang:monotonic_time(millisecond)+Timeout,Reason).
until_deadline(F,Deadline,Reason) ->
    case F() of true -> ok; false -> check(erlang:monotonic_time(millisecond)<Deadline,Reason),
        timer:sleep(5),until_deadline(F,Deadline,Reason) end.
check(true,_) -> ok;
check(false,Reason) -> error({contract,Reason}).
