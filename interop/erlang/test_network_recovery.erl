%% Restart network ownership under the same live shared ETS root.
-module(test_network_recovery).
-export([run/0]).

run() ->
    Cases=[zero_gauge_after_owner_death,restart_gauge_and_history,relearn_same_direct_node,late_stale_route],
    Failed=lists:foldl(fun(Case,N) ->
        put(case_sockets,[]),put(case_processes,[]),
        try exercise(Case),io:format("NETWORK_RECOVERY_OK ~p~n",[Case]),N
        catch C:R:Stack -> io:format("NETWORK_RECOVERY_FAIL ~p ~p:~p ~p~n",[Case,C,R,Stack]),N+1
        after cleanup() end
    end,0,Cases),
    case Failed of 0 -> ok; _ -> error({network_recovery_failures,Failed}) end.

exercise(zero_gauge_after_owner_death) ->
    {_Root,Graph}=fixture(),kill_network(Graph),
    check(connection_count()=:=0,dead_network_must_report_zero_active_connections);
exercise(restart_gauge_and_history) ->
    {Root,Graph}=fixture(),cycles(Root,Graph,3);
exercise(Case) when Case=:=relearn_same_direct_node; Case=:=late_stale_route ->
    {Root,Graph}=fixture(),kill_network(Graph),
    %% Keep the public shared table's old row as observed evidence, then change
    %% the peer address. The production restart must permit the same node again.
    [OldRoute]=ets:lookup(parrot_direct_links,<<"recovery-direct">>),
    io:format("NETWORK_RECOVERY_OLD_ROUTE ~p~n",[OldRoute]),
    {Port,_Listener}=restart(Root),
    case Case of
        relearn_same_direct_node ->
            check(ets:tab2list(parrot_direct_links)=:=[],new_generation_must_clear_old_direct_rows);
        late_stale_route ->
            %% Controlled late insertion uses the actual retired socket/PID row.
            %% This tests cleanup, not a claim that a scheduler race occurred.
            true=ets:insert(parrot_direct_links,OldRoute),
            io:format("NETWORK_RECOVERY_INJECTED_RETIRED_ROUTE ~p~n",[OldRoute])
    end,
    Fresh=client(Port),
    {Direct,Driver,Sup}=direct(Fresh),
    check(is_process_alive(Driver) andalso is_process_alive(Sup),relearned_direct_tree_must_be_alive),
    ok=gen_tcp:send(Direct,parrot_gw:build_frame(16#10,200,<<"/user/probe">>,<<"bin:u:Ping">>,
        <<0:32/little,7:64/little>>)),
    check(recv(Direct)=:={16#11,200,<<"bin:u:Pong">>,<<10:64/little>>},relearned_direct_must_serve_real_request).

cycles(_Root,_Graph,0) -> ok;
cycles(Root,Graph,Remaining) ->
    Identity=ets:lookup(parrot_gw_metrics,identity),History=counter(hs_ok),
    check(History>=2,history_must_include_completed_real_handshakes),
    kill_network(Graph),
    {Port,Listener}=restart(Root),
    check(ets:lookup(parrot_gw_metrics,identity)=:=Identity,restart_must_preserve_shared_metrics_epoch),
    check(counter(hs_ok)=:=History,restart_must_preserve_cumulative_handshakes),
    Fresh=client(Port),
    check(connection_count()=:=1,restarted_network_must_count_only_new_active_connections),
    {Direct,DirectDriver,DirectSup}=direct(Fresh),
    check(connection_count()=:=2,restarted_direct_must_add_one_active_connection),
    Next=graph(Listener,Fresh,Direct,DirectDriver,DirectSup),
    io:format("NETWORK_RECOVERY_CYCLE remaining=~p owner=~p connections=~p history=~p~n",
        [Remaining,maps:get(owner,Next),connection_count(),counter(hs_ok)]),
    cycles(Root,Next,Remaining-1).

fixture() ->
    Self=self(),Root=spawn(fun() ->
        {ok,Port}=parrot_gw:start(0,{127,0,0,1},fun parrot_gw:service/3),
        Self!{gateway_ready,self(),Port},root_loop(Self)
    end),track_process(Root),
    Port=receive {gateway_ready,Root,N} -> N after 3000 -> error(gateway_start_timeout) end,
    check(ets:info(parrot_gw_metrics,owner)=:=Root,fixture_must_hold_actual_shared_ets_owner),
    Listener=listener(Port),track_process(Listener),Parent=client(Port),
    {Direct,Driver,Sup}=direct(Parent),
    check(connection_count()=:=2,fixture_must_count_two_real_active_connections),
    {Root,graph(Listener,Parent,Direct,Driver,Sup)}.

root_loop(Parent) ->
    receive
        {restart,Ref} ->
            Result=parrot_gw:start(0,{127,0,0,1},fun parrot_gw:service/3),
            Parent!{restarted,Ref,Result},root_loop(Parent)
    end.
restart(Root) ->
    Ref=make_ref(),Root!{restart,Ref},
    Port=receive {restarted,Ref,{ok,N}} -> N after 6000 -> error(network_restart_timeout) end,
    Listener=listener(Port),track_process(Listener),track_process(whereis(parrot_gw_lease_owner)),
    {Port,Listener}.

graph(Listener,Parent,Direct,DirectDriver,DirectSup) ->
    Driver=opposite_owner(Parent),Sup=task_supervisor(Driver),Owner=whereis(parrot_gw_lease_owner),
    Pids=[Listener,Driver,Sup,DirectDriver,DirectSup,Owner],
    [track_process(P) || P <- Pids],
    #{owner=>Owner,pids=>Pids,sockets=>[Parent,Direct]}.
kill_network(#{owner:=Owner,pids:=Pids,sockets:=Sockets}) ->
    check(lists:all(fun is_process_alive/1,Pids),all_measured_network_resources_must_be_alive),
    Monitors=[{P,monitor(process,P)} || P <- Pids],exit(Owner,kill),
    [receive {'DOWN',Ref,process,P,_} -> ok after 2000 -> error(network_resource_survived_owner_kill) end
     || {P,Ref} <- Monitors],
    [check(gen_tcp:recv(S,0,1000)=:={error,closed},network_socket_must_close) || S <- Sockets],
    io:format("NETWORK_RECOVERY_TERMINATED owner=~p raw_connections=~p raw_routes=~p~n",
        [Owner,counter(connections),ets:tab2list(parrot_direct_links)]).

connection_count() -> maps:get(connections,maps:get(snapshot,parrot_gw:admin_measured_metrics())).
counter(Key) -> case ets:lookup(parrot_gw_metrics,Key) of [{Key,N}] -> N; [] -> 0 end.
client(Port) ->
    {ok,S}=gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),track_socket(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_body("recovery-peer")))),
    {2,1,_,_}=recv(S),S.
direct(Parent) ->
    {ok,L}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),track_socket(L),
    {ok,Port}=inet:port(L),Node = <<"recovery-direct">>,
    Addr=list_to_binary("127.0.0.1:"++integer_to_list(Port)),
    Hint = <<(byte_size(Node)):16/little,Node/binary,(byte_size(Addr)):16/little,Addr/binary>>,
    ok=gen_tcp:send(Parent,parrot_gw:build_frame(16#24,7,<<>>,<<>>,Hint)),
    Accepted=gen_tcp:accept(L,1200),
    check(element(1,Accepted)=:=ok,same_direct_node_must_reconnect_after_network_restart),
    {ok,S}=Accepted,track_socket(S),{1,1,_,_}=recv(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(2,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_ack_body(Node)))),
    ok=gen_tcp:send(S,parrot_gw:build_frame(3,91,<<>>,<<>>,<<>>)),{4,91,_,_}=recv(S),
    Driver=opposite_owner(S),track_process(Driver),{S,Driver,task_supervisor(Driver)}.
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
task_supervisor(Pid) ->
    {dictionary,D}=process_info(Pid,dictionary),Sup=proplists:get_value(task_sup,D),
    check(is_pid(Sup),connection_must_own_real_task_supervisor),track_process(Sup),Sup.
track_socket(S) -> put(case_sockets,[S|get(case_sockets)]),ok.
track_process(P) -> put(case_processes,[P|get(case_processes)]),ok.
cleanup() ->
    [try gen_tcp:close(S) catch _:_ -> ok end || S <- get(case_sockets)],
    Pids=lists:usort(get(case_processes)++case whereis(parrot_gw_lease_owner) of undefined -> []; P -> [P] end),
    [exit(P,kill) || P <- Pids],
    until(fun() -> lists:all(fun(P) -> not is_process_alive(P) end,Pids) andalso
        whereis(parrot_gw_lease_owner)=:=undefined andalso ets:info(parrot_gw_metrics)=:=undefined end,
        erlang:monotonic_time(millisecond)+3000),
    erase(case_sockets),erase(case_processes),ok.
until(F,Deadline) ->
    case F() of true -> ok; false -> check(erlang:monotonic_time(millisecond)<Deadline,fixture_cleanup_timeout),
        timer:sleep(5),until(F,Deadline) end.
check(true,_) -> ok;
check(false,Reason) -> error({contract,Reason}).
