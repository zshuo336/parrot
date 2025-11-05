%% Failed bind callers must not capture a later successful gateway's lifetime.
-module(test_startup_admission).
-export([run/0]).

run() ->
    Cases=[failed_bind_has_no_shared_state,{failed_caller,normal},{failed_caller,kill},
           existing_gateway_survives_failed_bind,occupied_port_reusable,initialization_failure_cleanup,
           concurrent_failed_binds],
    Failed=lists:foldl(fun(Case,N) ->
        put(case_sockets,[]),put(case_processes,[]),
        try exercise(Case),io:format("STARTUP_ADMISSION_OK ~p~n",[Case]),N
        catch C:R:Stack -> io:format("STARTUP_ADMISSION_FAIL ~p ~p:~p ~p~n",[Case,C,R,Stack]),N+1
        after cleanup() end
    end,0,Cases),
    case Failed of 0 -> ok; _ -> error({startup_admission_failures,Failed}) end.

exercise(failed_bind_has_no_shared_state) ->
    {_Busy,Port}=occupied(),{Failed,Result}=root(Port),bind_failed(Result),
    State=[{T,ets:info(T,owner)} || T <- tables()],
    io:format("STARTUP_ADMISSION_FAILED_BIND_STATE caller=~p tables=~p owner=~p~n",
        [Failed,State,whereis(parrot_gw_lease_owner)]),
    check(lists:all(fun({_,O}) -> O=:=undefined end,State) andalso whereis(parrot_gw_lease_owner)=:=undefined,
        failed_bind_must_not_create_shared_state);
exercise({failed_caller,Cause}) ->
    {_Busy,Port}=occupied(),{Failed,Result}=root(Port),bind_failed(Result),
    {Successful,{ok,LivePort}}=root(0),Listener=listener(LivePort),track_process(Listener),
    S=client(LivePort),Driver=opposite_owner(S),track_process(Driver),
    check(probe(S),fixture_must_serve_before_failed_caller_exit),
    Shared=ets:info(parrot_gw_metrics,owner),Owner=whereis(parrot_gw_lease_owner),
    OwnerMon=monitor(process,Owner),FailedMon=monitor(process,Failed),
    io:format("STARTUP_ADMISSION_LIFETIME cause=~p failed=~p successful=~p shared=~p lease=~p driver=~p~n",
        [Cause,Failed,Successful,Shared,Owner,Driver]),
    case Cause of normal -> Failed!return; kill -> exit(Failed,kill) end,
    await_down(FailedMon,Failed,failed_caller_must_exit),
    case Shared=:=Failed of
        true -> await_down(OwnerMon,Owner,old_owner_must_observe_failed_caller_exit);
        false -> demonitor(OwnerMon,[flush])
    end,
    check(is_process_alive(Successful) andalso probe(S) andalso
        ets:info(parrot_gw_metrics,owner)=:=Successful andalso is_process_alive(Listener),
        failed_caller_must_not_own_later_gateway);
exercise(existing_gateway_survives_failed_bind) ->
    {Successful,{ok,Port}}=root(0),Listener=listener(Port),track_process(Listener),
    S=client(Port),D=direct(S),
    Identity=ets:lookup(parrot_gw_metrics,identity),Routes=ets:tab2list(parrot_direct_links),
    {_Busy,BusyPort}=occupied(),{Failed,Result}=root(BusyPort),bind_failed(Result),
    Ref=monitor(process,Failed),Failed!return,await_down(Ref,Failed,failed_caller_must_exit),
    Metrics=parrot_gw:admin_metrics(),
    check(ets:info(parrot_gw_metrics,owner)=:=Successful andalso
        ets:lookup(parrot_gw_metrics,identity)=:=Identity andalso ets:tab2list(parrot_direct_links)=:=Routes andalso
        maps:get(connections,Metrics)=:=2 andalso maps:get(handshakes_ok,Metrics)=:=2 andalso probe(S) andalso probe(D),
        failed_bind_must_preserve_existing_gateway_state_and_traffic);
exercise(occupied_port_reusable) ->
    {Busy,Port}=occupied(),{Caller,Result}=root(Port),bind_failed(Result),
    ok=gen_tcp:close(Busy),Ref=make_ref(),Caller!{start,Ref,Port},
    Reply=receive {started_again,Ref,R} -> R after 6000 -> error(retry_start_timeout) end,
    check(Reply=:={ok,Port},failed_bind_must_leave_port_reusable),
    Listener=listener(Port),track_process(Listener),S=client(Port),
    check(probe(S) andalso ets:info(parrot_gw_metrics,owner)=:=Caller,
        successful_retry_must_own_and_serve_gateway);
exercise(initialization_failure_cleanup) ->
    Self=self(),Seed=spawn(fun() ->
        ets:new(parrot_gw_metrics,[named_table,private,set]),Self!{private_metrics_ready,self()},receive return -> ok end
    end),track_process(Seed),
    receive {private_metrics_ready,Seed} -> ok after 3000 -> error(private_metrics_fixture_timeout) end,
    {Reserved,Port}=occupied(),ok=gen_tcp:close(Reserved),
    {Caller,Reply}=root(Port),
    check(Reply=:={exception,error,badarg},fixture_must_trigger_real_initialization_exception),
    check(is_process_alive(Caller),initialization_caller_must_stay_alive_for_leak_observation),
    Rebound=gen_tcp:listen(Port,[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}]),
    io:format("STARTUP_ADMISSION_INITIALIZATION_EXCEPTION caller=~p result=~p rebound=~p~n",[Caller,Reply,Rebound]),
    case Rebound of
        {ok,L} -> track_socket(L),ok;
        _ -> check(false,initialization_failure_must_release_bound_socket)
    end;
exercise(concurrent_failed_binds) ->
    {_Busy,Port}=occupied(),Self=self(),
    Callers=[spawn(fun() ->
        Self!{bind_waiting,self()},receive go -> Self!{bind_result,self(),start(Port)},root_loop(Self) end
    end) || _ <- lists:seq(1,16)],
    [track_process(P) || P <- Callers],
    [receive {bind_waiting,P} -> ok after 3000 -> error(concurrent_caller_not_ready) end || P <- Callers],
    [P!go || P <- Callers],
    Results=[receive {bind_result,P,R} -> R after 6000 -> error(concurrent_bind_timeout) end || P <- Callers],
    [bind_failed(R) || R <- Results],
    io:format("STARTUP_ADMISSION_CONCURRENT_FAILURES callers=~p results=~p~n",[length(Callers),Results]),
    check(lists:all(fun(T) -> ets:info(T)=:=undefined end,tables()) andalso whereis(parrot_gw_lease_owner)=:=undefined,
        concurrent_failed_binds_must_not_create_shared_state).

root(Port) ->
    Self=self(),Pid=spawn(fun() -> Self!{started,self(),start(Port)},root_loop(Self) end),track_process(Pid),
    Reply=receive {started,Pid,R} -> R after 6000 -> error(root_start_timeout) end,{Pid,Reply}.
start(Port) ->
    try parrot_gw:start(Port,{127,0,0,1},fun parrot_gw:service/3)
    catch C:R -> {exception,C,R} end.
root_loop(Parent) ->
    receive
        return -> ok;
        {start,Ref,Port} -> Parent!{started_again,Ref,start(Port)},root_loop(Parent)
    end.
bind_failed(Result) -> check(Result=:={error,eaddrinuse},bind_failure_must_preserve_eaddrinuse).
occupied() ->
    {ok,L}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}]),track_socket(L),
    {ok,Port}=inet:port(L),{L,Port}.
client(Port) ->
    {ok,S}=gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),track_socket(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_body("startup-peer")))),
    {2,1,_,_}=recv(S),S.
probe(S) ->
    try
        ok=gen_tcp:send(S,parrot_gw:build_frame(16#10,51,<<"/user/probe">>,<<"bin:u:Ping">>,<<0:32/little,9:64/little>>)),
        recv(S)=:={16#11,51,<<"bin:u:Pong">>,<<12:64/little>>}
    catch _:_ -> false end.
direct(Parent) ->
    {L,Port}=occupied(),Node = <<"startup-direct">>,Addr=list_to_binary("127.0.0.1:"++integer_to_list(Port)),
    Hint = <<(byte_size(Node)):16/little,Node/binary,(byte_size(Addr)):16/little,Addr/binary>>,
    ok=gen_tcp:send(Parent,parrot_gw:build_frame(16#24,7,<<>>,<<>>,Hint)),
    {ok,S}=gen_tcp:accept(L,3000),track_socket(S),{1,1,_,_}=recv(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(2,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_ack_body(Node)))),
    ok=gen_tcp:send(S,parrot_gw:build_frame(3,91,<<>>,<<>>,<<>>)),{4,91,_,_}=recv(S),S.
recv(S) ->
    case gen_tcp:recv(S,4,1500) of
        {ok,<<N:32/little>>} ->
            case gen_tcp:recv(S,N,1500) of
                {ok,B} -> {ok,Ft,_,Cid,_,Key,Payload,<<>>}=parrot_gw:parse_frame(<<N:32/little,B/binary>>),
                    {Ft,Cid,Key,Payload};
                {error,R} -> {socket_error,R}
            end;
        {error,R} -> {socket_error,R}
    end.
opposite_owner(S) ->
    {ok,Endpoint}=inet:sockname(S),
    {ok,Peer}=inet:peername(S),
    [Pid]=[Owner || P <- erlang:ports(),
        erlang:port_info(P,name)=:={name,"tcp_inet"},(try inet:peername(P) catch _:_ -> error end)=:={ok,Endpoint},
        (try inet:sockname(P) catch _:_ -> error end)=:={ok,Peer},
        {connected,Owner} <- [erlang:port_info(P,connected)]],Pid.
listener(Number) ->
    [Pid]=[Owner || P <- erlang:ports(),
        erlang:port_info(P,name)=:={name,"tcp_inet"},(try inet:sockname(P) catch _:_ -> error end)=:={ok,{{127,0,0,1},Number}},
        (try inet:peername(P) catch _:_ -> error end)=:={error,enotconn},
        {connected,Owner} <- [erlang:port_info(P,connected)]],Pid.
tables() -> [parrot_gw_metrics,parrot_admin_components,parrot_direct_links].
track_socket(S) -> put(case_sockets,[S|get(case_sockets)]),ok.
track_process(P) -> put(case_processes,[P|get(case_processes)]),ok.
cleanup() ->
    [try gen_tcp:close(S) catch _:_ -> ok end || S <- get(case_sockets)],
    Pids=lists:usort(get(case_processes)++case whereis(parrot_gw_lease_owner) of undefined -> []; P -> [P] end),
    [exit(P,kill) || P <- Pids],
    until(fun() -> lists:all(fun(P) -> not is_process_alive(P) end,Pids) andalso
        whereis(parrot_gw_lease_owner)=:=undefined andalso lists:all(fun(T) -> ets:info(T)=:=undefined end,tables()) end,
        erlang:monotonic_time(millisecond)+3000),
    erase(case_sockets),erase(case_processes),ok.
await_down(Ref,Pid,Reason) -> receive {'DOWN',Ref,process,Pid,_} -> ok after 3000 -> error(Reason) end.
until(F,Deadline) ->
    case F() of true -> ok; false -> check(erlang:monotonic_time(millisecond)<Deadline,fixture_cleanup_timeout),
        timer:sleep(5),until(F,Deadline) end.
check(true,_) -> ok;
check(false,Reason) -> error({contract,Reason}).
