-module(test_lease_activation).
-export([run/0,register_gate/2]).

run() ->
    instrument_gateway(),
    Cases=[{starter,kill},{starter,shutdown},{root,kill},{root,normal},{loser,kill}],
    Failed=lists:foldl(fun(Case,N) ->
        put(case_sockets,[]),put(case_processes,[]),
        try exercise(Case),io:format("LEASE_ACTIVATION_OK ~p~n",[Case]),N
        catch C:R:Stack -> io:format("LEASE_ACTIVATION_FAIL ~p ~p:~p ~p~n",[Case,C,R,Stack]),N+1
        after cleanup() end
    end,0,Cases),
    case Failed of 0 -> ok; _ -> error({lease_activation_failures,Failed}) end.

instrument_gateway() ->
    Source=case os:getenv("PARROT_LEASE_SOURCE") of false -> "parrot_gw.erl"; P -> P end,
    Output=case os:getenv("PARROT_LEASE_ARTIFACTS") of
        false -> filename:join("/tmp","parrot-lease-activation-"++os:getpid()); P1 -> P1 end,
    ok=file:make_dir(Output),
    {parrot_gw,Original,_}=code:get_object_code(parrot_gw),
    ok=file:write_file(filename:join(Output,"original-gateway.beam"),Original,[exclusive]),
    {ok,Forms}=epp:parse_file(Source,[],[]),put(register_sites,0),
    Instrumented=instrument(Forms),
    check(get(register_sites)=:=1,one_actual_lease_registration_site_required),
    {ok,parrot_gw,Binary,[]}=compile:forms(Instrumented,
        [binary,debug_info,warnings_as_errors,return_errors,return_warnings]),
    ok=file:write_file(filename:join(Output,"instrumented-gateway.forms"),term_to_binary(Instrumented),[exclusive]),
    ok=file:write_file(filename:join(Output,"instrumented-gateway.beam"),Binary,[exclusive]),
    {module,parrot_gw}=code:load_binary(parrot_gw,Source,Binary),
    io:format("LEASE_ACTIVATION_INSTRUMENTED one_real_register_site artifacts=~s~n",[Output]).

instrument({call,L,{atom,_,register},[{atom,_,parrot_gw_lease_owner}=Name,Pid]}) ->
    put(register_sites,get(register_sites)+1),
    {call,L,{remote,L,{atom,L,?MODULE},{atom,L,register_gate}},[Name,instrument(Pid)]};
instrument(T) when is_tuple(T) -> list_to_tuple([instrument(V) || V <- tuple_to_list(T)]);
instrument(L) when is_list(L) -> [instrument(V) || V <- L];
instrument(V) -> V.

register_gate(Name,Pid) ->
    registration_gate(before,Pid),
    Result=erlang:register(Name,Pid),
    registration_gate(after_registration,Pid),
    Result.

registration_gate(Position,Pid) ->
    case persistent_term:get({?MODULE,gate},none) of
        {Controller,Ref,Position} ->
            Controller!{lease_registered,Ref,self(),Pid},
            receive {release,Ref} -> ok after 10000 -> error(test_registration_gate_timeout) end;
        _ -> ok
    end.

initial_root() ->
    Root=start_caller(),
    {ok,Port}=started(Root),RootSocket=client(Port),check(probe(RootSocket),initial_business_must_work),
    check(ets:info(parrot_gw_metrics,owner)=:=Root,initial_caller_must_own_shared_tables),
    Old=whereis(parrot_gw_lease_owner),track_process(Old),
    OldRef=monitor(process,Old),exit(Old,kill),await_down(OldRef,Old),
    check(gen_tcp:recv(RootSocket,0,2000)=:={error,closed},old_network_must_retire),
    check(is_process_alive(Root),shared_root_must_survive_lease_death),
    Root.

exercise({loser,kill}=Case) ->
    Root=initial_root(),GateRef=make_ref(),persistent_term:put({?MODULE,gate},{self(),GateRef,before}),
    Starter=start_caller(),
    {Gate,Pending}=receive {lease_registered,GateRef,G,L} -> {G,L}
        after 3000 -> error(pre_registration_gate_not_reached) end,
    track_process(Pending),
    check(whereis(parrot_gw_lease_owner)=:=undefined,gate_must_precede_actual_registration),
    [BoundPort]=bound_ports(Starter),
    io:format("LEASE_ACTIVATION_OBSERVED case=~p root=~p starter=~p registration_caller=~p lease=~p bound=~p~n",
        [Case,Root,Starter,Gate,Pending,BoundPort]),
    Ref=monitor(process,Starter),exit(Starter,kill),await_down(Ref,Starter),
    persistent_term:erase({?MODULE,gate}),
    Fresh=start_caller(),{ok,FreshPort}=started(Fresh),S=client(FreshPort),
    check(probe(S),winning_generation_must_serve_before_loser_resumes),
    Direct=direct(S),check(probe(Direct),winning_direct_connection_must_serve),
    DirectRows=ets:lookup(parrot_direct_links,<<"lease-direct">>),
    check(length(DirectRows)=:=1,winning_generation_must_publish_direct_route),
    Winner=whereis(parrot_gw_lease_owner),track_process(Winner),
    check(Winner=/=Pending,winner_must_be_a_distinct_generation),
    Gate!{release,GateRef},
    until(fun() -> not is_process_alive(Pending) end,erlang:monotonic_time(millisecond)+6500),
    check(whereis(parrot_gw_lease_owner)=:=Winner andalso probe(S) andalso probe(Direct) andalso
        ets:lookup(parrot_direct_links,<<"lease-direct">>)=:=DirectRows andalso
        ets:info(parrot_gw_metrics,owner)=:=Root andalso
        maps:get(connections,parrot_gw:admin_metrics())=:=2,
        losing_child_must_not_reset_winning_generation),
    io:format("LEASE_ACTIVATION_RESULT ~p ~p~n",[Case,{ok,FreshPort}]);
exercise({Subject,Cause}=Case) ->
    Root=initial_root(),
    GateRef=make_ref(),persistent_term:put({?MODULE,gate},{self(),GateRef,after_registration}),
    Starter=start_caller(),
    {Gate,Lease}=receive {lease_registered,GateRef,G,L} -> {G,L}
        after 3000 -> error(registration_gate_not_reached) end,
    track_process(Lease),
    check(whereis(parrot_gw_lease_owner)=:=Lease,gate_must_follow_actual_registration),
    [BoundPort]=bound_ports(Starter),
    io:format("LEASE_ACTIVATION_OBSERVED case=~p root=~p starter=~p registration_caller=~p lease=~p bound=~p~n",
        [Case,Root,Starter,Gate,Lease,BoundPort]),
    case Subject of
        starter ->
            Ref=monitor(process,Starter),exit(Starter,Cause),await_down(Ref,Starter),
            persistent_term:erase({?MODULE,gate}),Gate!{release,GateRef},
            Fresh=start_caller(),Result=started(Fresh),
            io:format("LEASE_ACTIVATION_RESULT ~p ~p~n",[Case,Result]),
            check(element(1,Result)=:=ok,surviving_root_must_recover_after_starter_exit),
            {ok,FreshPort}=Result,S=client(FreshPort),
            check(probe(S),recovered_listener_must_serve_real_business),
            check(is_process_alive(Lease) andalso whereis(parrot_gw_lease_owner)=:=Lease,
                recovery_must_use_the_registered_generation),
            check(ets:info(parrot_gw_metrics,owner)=:=Root andalso
                maps:get(connections,parrot_gw:admin_metrics())=:=1,
                recovery_must_preserve_root_and_count_new_connection);
        root ->
            Ref=monitor(process,Root),
            case Cause of normal -> Root!quit; kill -> exit(Root,kill) end,
            await_down(Ref,Root),
            persistent_term:erase({?MODULE,gate}),Gate!{release,GateRef},
            Result=started(Starter),
            io:format("LEASE_ACTIVATION_RESULT ~p ~p~n",[Case,Result]),
            check(element(1,Result)=/=ok,dead_root_must_not_publish_successful_listener),
            until(fun() -> not is_process_alive(Lease) andalso
                whereis(parrot_gw_lease_owner)=:=undefined andalso
                lists:all(fun(T) -> ets:info(T)=:=undefined end,tables()) end,
                erlang:monotonic_time(millisecond)+3000),
            Rebound=await_rebind(BoundPort,erlang:monotonic_time(millisecond)+3000),track_socket(Rebound),
            NewRoot=start_caller(),{ok,NewPort}=started(NewRoot),S=client(NewPort),
            check(probe(S) andalso ets:info(parrot_gw_metrics,owner)=:=NewRoot,
                new_root_must_recover_after_pre_activation_root_death)
    end.

start_caller() ->
    Parent=self(),Pid=spawn(fun() ->
        Result=try parrot_gw:start(0,{127,0,0,1},fun parrot_gw:service/3)
            catch C:R -> {exception,C,R} end,
        Parent!{started,self(),Result},receive quit -> ok end
    end),track_process(Pid),Pid.

started(Pid) ->
    receive {started,Pid,Result} -> Result after 8000 -> error(start_result_not_observed) end.

client(Port) ->
    {ok,S}=gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),track_socket(S),
    Body=iolist_to_binary(parrot_gw:handshake_body("lease-activation-peer")),
    ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,Body)),{2,1,_,_}=recv(S),S.

direct(Parent) ->
    {ok,L}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),track_socket(L),
    {ok,Port}=inet:port(L),Node = <<"lease-direct">>,Addr=list_to_binary("127.0.0.1:"++integer_to_list(Port)),
    Hint = <<(byte_size(Node)):16/little,Node/binary,(byte_size(Addr)):16/little,Addr/binary>>,
    ok=gen_tcp:send(Parent,parrot_gw:build_frame(16#24,7,<<>>,<<>>,Hint)),
    {ok,S}=gen_tcp:accept(L,2000),track_socket(S),{1,1,_,_}=recv(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(2,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_ack_body(Node)))),
    ok=gen_tcp:send(S,parrot_gw:build_frame(3,91,<<>>,<<>>,<<>>)),{4,91,_,_}=recv(S),S.

probe(S) ->
    ok=gen_tcp:send(S,parrot_gw:build_frame(16#10,51,<<"/user/probe">>,<<"bin:u:Ping">>,<<0:32/little,9:64/little>>)),
    recv(S)=:={16#11,51,<<"bin:u:Pong">>,<<12:64/little>>}.

recv(S) ->
    {ok,<<N:32/little>>}=gen_tcp:recv(S,4,2000),{ok,B}=gen_tcp:recv(S,N,2000),
    {ok,Ft,_,Cid,_,Key,Payload,<<>>}=parrot_gw:parse_frame(<<N:32/little,B/binary>>),{Ft,Cid,Key,Payload}.

bound_ports(Pid) ->
    [Number || P <- erlang:ports(),erlang:port_info(P,connected)=:={connected,Pid},
        {ok,{{127,0,0,1},Number}} <- [(try inet:sockname(P) catch _:_ -> error end)],
        (try inet:peername(P) catch _:_ -> error end)=:={error,enotconn}].

await_rebind(Port,Deadline) ->
    case gen_tcp:listen(Port,[binary,{active,false},{ip,{127,0,0,1}},{reuseaddr,true}]) of
        {ok,S} -> S;
        {error,eaddrinuse} -> check(erlang:monotonic_time(millisecond)<Deadline,bound_port_must_be_released),
            timer:sleep(5),await_rebind(Port,Deadline);
        {error,R} -> error({rebind_failed,R})
    end.

tables() -> [parrot_direct_links,parrot_admin_components,parrot_gw_metrics].
track_socket(S) -> put(case_sockets,[S|get(case_sockets)]),ok.
track_process(P) -> put(case_processes,[P|get(case_processes)]),ok.
await_down(Ref,Pid) -> receive {'DOWN',Ref,process,Pid,_} -> ok after 3000 -> error(process_must_exit) end.
until(F,Deadline) ->
    case F() of true -> ok; false -> check(erlang:monotonic_time(millisecond)<Deadline,lifecycle_must_converge),
        timer:sleep(5),until(F,Deadline) end.
cleanup() ->
    persistent_term:erase({?MODULE,gate}),
    [try gen_tcp:close(S) catch _:_ -> ok end || S <- get(case_sockets)],
    Pids=lists:usort(get(case_processes)++case whereis(parrot_gw_lease_owner) of undefined -> []; P -> [P] end),
    [exit(P,kill) || P <- Pids],
    until(fun() -> lists:all(fun(P) -> not is_process_alive(P) end,Pids) andalso
        whereis(parrot_gw_lease_owner)=:=undefined andalso
        lists:all(fun(T) -> ets:info(T)=:=undefined end,tables()) end,
        erlang:monotonic_time(millisecond)+3000),
    erase(case_sockets),erase(case_processes),ok.
check(true,_) -> ok;
check(false,Reason) -> error({contract,Reason}).
