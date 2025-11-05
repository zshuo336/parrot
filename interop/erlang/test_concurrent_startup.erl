%% Public concurrent successful starts, with real shared tables and TCP probes.
-module(test_concurrent_startup).
-export([run/0]).

run() ->
    check(erlang:system_info(schedulers_online)>=2,fixture_requires_two_schedulers),
    Cases=[socket_identity,socket_protocol_identity]++[{first_start,I} || I <- lists:seq(1,8)]++[lock_holder_dies,waiting_caller_dies],
    Failed=lists:foldl(fun(Case,N) ->
        put(case_sockets,[]),put(case_processes,[]),
        try exercise(Case),io:format("CONCURRENT_STARTUP_OK ~p~n",[Case]),N
        catch C:R:Stack -> io:format("CONCURRENT_STARTUP_FAIL ~p ~p:~p ~p~n",[Case,C,R,Stack]),N+1
        after cleanup() end
    end,0,Cases),
    case Failed of 0 -> ok; _ -> error({concurrent_startup_failures,Failed}) end.

exercise(socket_identity) ->
    {ok,L1}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}]),track_socket(L1),
    {ok,L2}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}]),track_socket(L2),
    {ok,P1}=inet:port(L1),{ok,P2}=inet:port(L2),
    {ok,C1}=gen_tcp:connect({127,0,0,1},P1,[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}]),track_socket(C1),
    {ok,{_,LocalPort}}=inet:sockname(C1),
    {ok,C2}=gen_tcp:connect({127,0,0,1},P2,[binary,{active,false},{ip,{127,0,0,1}},
        {port,LocalPort},{reuseaddr,true}]),track_socket(C2),
    check(inet:sockname(C1)=:=inet:sockname(C2),fixture_must_reuse_exact_local_endpoint),
    {ok,S1}=gen_tcp:accept(L1,1000),track_socket(S1),
    {ok,S2}=gen_tcp:accept(L2,1000),track_socket(S2),
    H1=spawn(fun() -> receive return -> ok end end),track_process(H1),
    H2=spawn(fun() -> receive return -> ok end end),track_process(H2),
    ok=gen_tcp:controlling_process(S1,H1),ok=gen_tcp:controlling_process(S2,H2),
    check(opposite_owner(C1)=:=H1 andalso opposite_owner(C2)=:=H2,
        full_tcp_tuple_must_identify_socket_owner);

exercise(socket_protocol_identity) ->
    {ok,L}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}]),track_socket(L),
    {ok,Port}=inet:port(L),
    {ok,C}=gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}],1000),track_socket(C),
    {ok,S}=gen_tcp:accept(L,1000),track_socket(S),
    H=spawn(fun() -> receive return -> ok end end),track_process(H),
    ok=gen_tcp:controlling_process(S,H),
    {ok,{_,ClientPort}}=inet:sockname(C),
    UOpts=[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}],
    {ok,U1}=gen_udp:open(Port,UOpts),track_socket(U1),
    ok=gen_udp:connect(U1,{127,0,0,1},ClientPort),
    {ok,U2}=gen_udp:open(Port,UOpts),track_socket(U2),
    check(inet:sockname(U1)=:=inet:sockname(S) andalso
        inet:peername(U1)=:=inet:peername(S),fixture_must_collide_across_protocols),
    check(opposite_owner(C)=:=H,tcp_owner_must_ignore_identical_udp_endpoints),
    check(listener(Port)=:=self(),tcp_listener_must_ignore_unconnected_udp),
    gen_udp:close(U1),gen_udp:close(U2);

exercise({first_start,Round}) ->
    Self=self(),
    Callers=[spawn_opt(fun() ->
        Self!{waiting,self()},receive go ->
            Result=try parrot_gw:start(0,{127,0,0,1},fun parrot_gw:service/3) catch C:R -> {exception,C,R} end,
            Self!{started,self(),Result},receive return -> ok end
        end
    end,[{scheduler,1+(I rem 2)}]) || I <- lists:seq(1,32)],
    [track_process(P) || P <- Callers],
    [receive {waiting,P} -> ok after 3000 -> error(start_caller_not_ready) end || P <- Callers],
    [P!go || P <- Callers],
    Results=[receive {started,P,R} -> {P,R} after 15000 -> error(concurrent_start_timeout) end || P <- Callers],
    check(lists:all(fun({_,{ok,Port}}) -> is_integer(Port); (_) -> false end,Results),all_concurrent_starts_must_succeed),
    Ports=[Port || {_,{ok,Port}} <- Results],
    check(length(lists:usort(Ports))=:=32,all_listening_endpoints_must_be_distinct),
    Listeners=[listener(Port) || Port <- Ports],[track_process(P) || P <- Listeners],
    Clients=[client(Port) || Port <- Ports],
    check(lists:all(fun probe/1,Clients),every_successful_start_must_serve_before_retirement),
    Owners=[{T,ets:info(T,owner)} || T <- tables()],
    Root=ets:info(parrot_gw_metrics,owner),Lease=whereis(parrot_gw_lease_owner),
    io:format("CONCURRENT_STARTUP_OBSERVED round=~p callers=~p owners=~p lease=~p connections=~p~n",
        [Round,length(Callers),Owners,Lease,maps:get(connections,parrot_gw:admin_metrics())]),
    [Other]=lists:sublist([P || P <- Callers,P=/=Root],1),
    %% Every non-root caller must be disposable even if it won a different table.
    NonRoots=[P || P <- Callers,P=/=Root],
    Monitors=[{P,monitor(process,P)} || P <- NonRoots],
    [exit(P,kill) || P <- NonRoots],
    [await_down(Ref,P,nonroot_caller_must_die) || {P,Ref} <- Monitors],
    io:format("CONCURRENT_STARTUP_NONROOTS_RETIRED round=~p example=~p root=~p~n",[Round,Other,Root]),
    Direct=direct(hd(Clients)),
    check(probe(Direct),nonroot_exit_must_preserve_direct_business),
    check(lists:all(fun({_,O}) -> O=:=Root end,Owners),first_concurrent_starts_must_share_one_table_owner),
    check(lists:all(fun(T) -> ets:info(T,owner)=:=Root end,tables()) andalso
        lists:all(fun probe/1,Clients),nonroot_retirement_must_preserve_all_shared_state_and_traffic),
    check(maps:get(connections,parrot_gw:admin_metrics())=:=33,all_live_connections_must_remain_counted),
    Pids=[Lease|Listeners]++[opposite_owner(S) || S <- [Direct|Clients]],
    Refs=[{P,monitor(process,P)} || P <- Pids],exit(Root,kill),
    [await_down(Ref,P,shared_root_death_must_retire_every_listener_and_driver) || {P,Ref} <- Refs],
    check(lists:all(fun(S) -> gen_tcp:recv(S,0,1000)=:={error,closed} end,[Direct|Clients]),all_owned_sockets_must_close_with_root);
exercise(Case) when Case=:=lock_holder_dies; Case=:=waiting_caller_dies ->
    Self=self(),
    Holder=spawn(fun() ->
        true=global:set_lock({parrot_gw_initialization,self()},[node()]),
        Self!{held,self()},receive release -> global:del_lock({parrot_gw_initialization,self()},[node()]) end
    end),track_process(Holder),
    receive {held,Holder} -> ok after 3000 -> error(lock_holder_not_ready) end,
    {ok,Reserved}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),
    {ok,Port}=inet:port(Reserved),ok=gen_tcp:close(Reserved),
    Caller=spawn(fun() ->
        Self!{blocked_start_result,self(),parrot_gw:start(Port,{127,0,0,1},fun parrot_gw:service/3)},
        receive return -> ok end
    end),track_process(Caller),
    wait_for_local_lock(Caller,erlang:monotonic_time(millisecond)+3000),
    check(listener(Port)=:=Caller,waiting_start_must_own_bound_socket),
    check(lists:all(fun(T) -> ets:info(T)=:=undefined end,tables()),waiting_start_must_not_create_shared_tables),
    io:format("CONCURRENT_STARTUP_LOCK_OBSERVED case=~p holder=~p caller=~p stack=~p~n",
        [Case,Holder,Caller,process_info(Caller,current_stacktrace)]),
    case Case of
        lock_holder_dies ->
            Ref=monitor(process,Holder),exit(Holder,kill),await_down(Ref,Holder,lock_holder_must_die),
            receive {blocked_start_result,Caller,{ok,Port}} -> ok after 10000 -> error(start_must_recover_dead_lock_holder) end,
            Listener=listener(Port),track_process(Listener),S=client(Port),
            check(lists:all(fun(T) -> ets:info(T,owner)=:=Caller end,tables()) andalso probe(S),
                dead_lock_holder_must_release_initialization);
        waiting_caller_dies ->
            Ref=monitor(process,Caller),exit(Caller,kill),await_down(Ref,Caller,waiting_caller_must_die),
            Rebound=await_rebind(Port,erlang:monotonic_time(millisecond)+3000),track_socket(Rebound),
            check(lists:all(fun(T) -> ets:info(T)=:=undefined end,tables()),dead_waiter_must_leave_no_shared_state),
            HRef=monitor(process,Holder),Holder!release,await_down(HRef,Holder,lock_holder_must_release),
            FreshRoot=spawn(fun() ->
                {ok,N}=parrot_gw:start(0,{127,0,0,1},fun parrot_gw:service/3),
                Self!{fresh_started,self(),N},receive return -> ok end
            end),track_process(FreshRoot),
            FreshPort=receive {fresh_started,FreshRoot,N} -> N after 6000 -> error(fresh_root_start_timeout) end,
            Listener=listener(FreshPort),track_process(Listener),S=client(FreshPort),
            check(lists:all(fun(T) -> ets:info(T,owner)=:=FreshRoot end,tables()) andalso probe(S),
                start_must_recover_dead_waiting_caller)
    end.

%% Process DOWN does not join the asynchronous termination of its socket port.
await_rebind(Port,Deadline) ->
    case gen_tcp:listen(Port,[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}]) of
        {ok,Socket} -> Socket;
        {error,eaddrinuse} ->
            check(erlang:monotonic_time(millisecond)<Deadline,dead_waiter_must_release_bound_port),
            timer:sleep(5),await_rebind(Port,Deadline);
        {error,Reason} -> error({dead_waiter_rebind_failed,Reason})
    end.

wait_for_local_lock(Caller,Deadline) ->
    receive {blocked_start_result,Caller,_} -> check(false,initialization_must_wait_for_shared_local_lock)
    after 0 ->
        case process_info(Caller,current_stacktrace) of
            {current_stacktrace,Stack} ->
                case lists:any(fun({global,_,_,_}) -> true; (_) -> false end,Stack) of
                    true -> ok;
                    false -> check(erlang:monotonic_time(millisecond)<Deadline,initialization_must_reach_shared_local_lock),
                        timer:sleep(1),wait_for_local_lock(Caller,Deadline)
                end;
            _ -> error(start_caller_died_before_lock_observation)
        end
    end.

direct(Parent) ->
    {ok,L}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),track_socket(L),
    {ok,Port}=inet:port(L),Node = <<"startup-direct">>,Addr=list_to_binary("127.0.0.1:"++integer_to_list(Port)),
    Hint = <<(byte_size(Node)):16/little,Node/binary,(byte_size(Addr)):16/little,Addr/binary>>,
    ok=gen_tcp:send(Parent,parrot_gw:build_frame(16#24,7,<<>>,<<>>,Hint)),
    Accepted=gen_tcp:accept(L,1500),
    check(element(1,Accepted)=:=ok,nonroot_exit_must_preserve_direct_admission),
    {ok,S}=Accepted,track_socket(S),{1,1,_,_}=recv(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(2,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_ack_body(Node)))),
    ok=gen_tcp:send(S,parrot_gw:build_frame(3,91,<<>>,<<>>,<<>>)),{4,91,_,_}=recv(S),S.

client(Port) ->
    {ok,S}=gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),track_socket(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_body("concurrent-peer")))),
    {2,1,_,_}=recv(S),S.
probe(S) ->
    try
        ok=gen_tcp:send(S,parrot_gw:build_frame(16#10,51,<<"/user/probe">>,<<"bin:u:Ping">>,<<0:32/little,9:64/little>>)),
        recv(S)=:={16#11,51,<<"bin:u:Pong">>,<<12:64/little>>}
    catch _:_ -> false end.
recv(S) ->
    case gen_tcp:recv(S,4,2000) of
        {ok,<<N:32/little>>} ->
            case gen_tcp:recv(S,N,2000) of
                {ok,B} -> {ok,Ft,_,Cid,_,Key,Payload,<<>>}=parrot_gw:parse_frame(<<N:32/little,B/binary>>),
                    {Ft,Cid,Key,Payload};
                {error,R} -> {socket_error,R}
            end;
        {error,R} -> {socket_error,R}
    end.
listener(Number) ->
    [Pid]=[Owner || P <- erlang:ports(),
        erlang:port_info(P,name)=:={name,"tcp_inet"},(try inet:sockname(P) catch _:_ -> error end)=:={ok,{{127,0,0,1},Number}},
        (try inet:peername(P) catch _:_ -> error end)=:={error,enotconn},
        {connected,Owner} <- [erlang:port_info(P,connected)]],Pid.
opposite_owner(S) ->
    {ok,Endpoint}=inet:sockname(S),
    {ok,Peer}=inet:peername(S),
    [Pid]=[Owner || P <- erlang:ports(),
        erlang:port_info(P,name)=:={name,"tcp_inet"},(try inet:peername(P) catch _:_ -> error end)=:={ok,Endpoint},
        (try inet:sockname(P) catch _:_ -> error end)=:={ok,Peer},
        {connected,Owner} <- [erlang:port_info(P,connected)]],Pid.
tables() -> [parrot_direct_links,parrot_admin_components,parrot_gw_metrics].
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
