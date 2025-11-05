%% Actual TCP and isolated guest state distinguish connection tasks from business.
-module(test_terminal_workers).
-export([run/0]).

run() ->
    [begin
        {M,Bin,_}=code:get_object_code(M),persistent_term:put({parrot_beam_bootstrap,M},Bin)
     end || M <- [parrot_trace,parrot_beam_guest,parrot_component_runtime,parrot_gw]],
    Base=case os:getenv("PARROT_TERMINAL_WORKER_ARTIFACTS") of
        false -> "/tmp/parrot-terminal-workers-"++os:getpid(); Path -> Path
    end,
    ok=file:make_dir(Base),
    Cases=[{listener,ask,terminal},{listener,tell,terminal},{listener,completion,terminal},
           {direct,ask,terminal},{direct,tell,terminal},{direct,completion,terminal},
           {direct,ask,parent_terminal},{direct,ask,parent_eof},{direct,ask,business_error}],
    Failed=lists:foldl(fun({Role,Mode,Cause}=Case,Count) ->
        Label=atom_to_list(Role)++"-"++atom_to_list(Mode)++"-"++atom_to_list(Cause),
        try fixture(Base,Label,Case),io:format("TERMINAL_WORKER_OK ~s~n",[Label]),Count
        catch C:R:Stack -> io:format("TERMINAL_WORKER_FAIL ~s ~p:~p ~p~n",[Label,C,R,Stack]),Count+1 end
    end,0,Cases),
    case Failed of 0 -> ok; _ -> error({terminal_worker_failures,Failed}) end.

fixture(Base,Label,Case) ->
    Dir=filename:join(Base,Label),ok=file:make_dir(Dir),
    Self=self(),{Root,RootMon}=spawn_monitor(fun() ->
        {ok,Port}=parrot_gw:start(0),Self!{gateway_ready,self(),Port},receive stop -> ok end
    end),
    Port=receive {gateway_ready,Root,N} -> N after 3000 -> error(gateway_start_timeout) end,
    put(case_sockets,[]),
    Prefix=filename:join(Dir,binary_to_list(binary:encode_hex(erlang:md5(<<"/user/work">>)))),
    try
        Control=list_to_binary(Dir),Config = <<(byte_size(Control)):32/big,Control/binary,"normal">>,
        Artifact=filename:dirname(code:which(test_beam_host_fixture)),
        {deployed,[<<"/user/work">>]}=parrot_gw:admin_deploy({<<"work">>,<<"1">>,
            {beam,<<"test_beam_host_fixture">>,list_to_binary("file://"++Artifact)},singleton,Config}),
        [{<<"work">>,_,[Path],{owned,{isolated,Host},Entries}}]=ets:lookup(parrot_admin_components,<<"work">>),
        Gate=maps:get(pid,maps:get(Path,Entries)),
        #{os_pid:=GuestPid}=parrot_beam_host:describe(Host),
        check(GuestPid=/=os:getpid(),business_must_use_actual_child_vm),
        ok=file:write_file(filename:join(Dir,"guest-os-pid.txt"),GuestPid,[exclusive]),
        exercise(Port,Prefix,Gate,Case)
    after
        file:write_file(Prefix++".resume",<<1>>),
        [try gen_tcp:close(S) catch _:_ -> ok end || S <- get(case_sockets)],erase(case_sockets),
        Stop=try parrot_gw:admin_stop(<<"/user/work">>) catch C:R -> {C,R} end,
        exit(Root,kill),
        receive {'DOWN',RootMon,process,Root,_} -> ok after 3000 -> error(root_cleanup_timeout) end,
        close_listener(Port),
        until(fun() -> whereis(parrot_component_runtime)=:=undefined end,3000,component_owner_cleanup_timeout),
        check(Stop=:=stopped,component_cleanup_must_confirm_retirement)
    end.

exercise(Port,Prefix,Gate,{Role,Mode,Cause}) ->
    Parent=client(Port),
    Conn=case Role of listener -> Parent; direct -> direct(Parent) end,
    Driver=server_owner(Conn),
    {dictionary,Dict}=process_info(Driver,dictionary),
    {task_sup,Sup}=lists:keyfind(task_sup,1,Dict),
    Block=case Mode of
        ask -> parrot_gw:build_frame(16#10,101,<<"/user/work">>,<<"block">>,<<0:32/little>>);
        tell -> parrot_gw:build_frame(16#13,101,<<"/user/work">>,<<"block">>,<<>>);
        completion -> parrot_gw:build_frame(16#13,16#10,101,<<"/user/work">>,<<"block">>,<<>>)
    end,
    ok=gen_tcp:send(Conn,Block),
    until(fun() -> filelib:is_file(Prefix++".entered") end,3000,business_must_enter_before_terminal),
    #{active:={_,{Work,_},_}}=sys:get_state(Gate),
    check(is_process_alive(Work),connection_worker_must_be_live_before_terminal),
    check(maps:get(count,sys:get_state(Gate))=:=1,accepted_business_must_hold_one_budget_slot),
    WorkMon=monitor(process,Work),DriverMon=monitor(process,Driver),SupMon=monitor(process,Sup),
    io:format("TERMINAL_WORKER_OBSERVED role=~p mode=~p cause=~p driver=~p supervisor=~p work=~p~n",
              [Role,Mode,Cause,Driver,Sup,Work]),
    case Cause of
        business_error ->
            Error=parrot_gw:build_frame(16#12,88,<<>>,<<>>,parrot_gw:err_payload(3,"recoverable")),
            ok=gen_tcp:send(Conn,[Error,heartbeat()]),
            check(case recv(Conn) of {4,91,_,_} -> true; _ -> false end,
                  business_error_must_not_close_pending_connection),
            check(is_process_alive(Driver) andalso is_process_alive(Work),business_error_must_keep_pending_request),
            check(maps:get(count,sys:get_state(Gate))=:=1,business_error_must_preserve_budget),
            ok=file:write_file(Prefix++".resume",<<1>>),
            {16#11,101,<<"count">>,<<1:64/little>>}=recv(Conn),
            await_down(WorkMon,Work,connection_worker_must_finish_after_business_reply),
            ok=gen_tcp:send(Conn,terminal()),eof(Conn);
        _ ->
            Failed=case Cause of terminal -> Conn; _ -> Parent end,
            %% The suffix must never reach the component after terminal ERROR.
            Suffix=parrot_gw:build_frame(16#13,102,<<"/user/work">>,<<"inc">>,<<>>),
            case Cause of
                parent_eof -> ok=gen_tcp:close(Parent);
                _ -> ok=gen_tcp:send(Failed,[terminal(),Suffix,heartbeat()]),eof(Failed)
            end,
            case Cause of terminal -> ok; _ -> eof(Conn) end,
            await_down(WorkMon,Work,connection_worker_survived_terminal),
            check(maps:get(count,sys:get_state(Gate))=:=1,connection_exit_must_not_release_accepted_business_budget),
            check(maps:get(bytes,sys:get_state(Gate))>0,accepted_business_bytes_must_remain_charged),
            ok=file:write_file(Prefix++".resume",<<1>>)
    end,
    await_down(DriverMon,Driver,connection_driver_survived_terminal),
    await_down(SupMon,Sup,connection_supervisor_survived_terminal),
    until(fun() -> maps:get(count,sys:get_state(Gate))=:=0 end,3000,completed_business_must_release_budget),
    check(maps:get(bytes,sys:get_state(Gate))=:=0,completed_business_must_release_bytes),
    Fresh=client(Port),
    Query=parrot_gw:build_frame(16#10,500,<<"/user/work">>,<<"count">>,<<0:32/little>>),
    ok=gen_tcp:send(Fresh,Query),
    check(recv(Fresh)=:={16#11,500,<<"count">>,<<1:64/little>>},accepted_business_must_complete_once_without_suffix),
    case {Role,Cause} of
        {direct,terminal} -> ok=gen_tcp:send(Parent,heartbeat()),{4,91,_,_}=recv(Parent);
        _ -> ok
    end.

client(Port) ->
    {ok,S}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}]),track(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_body("worker-peer")))) ,
    {2,1,_,_}=recv(S),S.
direct(Parent) ->
    {ok,L}=gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),track(L),
    {ok,Port}=inet:port(L),Node = <<"worker-direct">>,
    Addr=list_to_binary("127.0.0.1:"++integer_to_list(Port)),
    Hint = <<(byte_size(Node)):16/little,Node/binary,(byte_size(Addr)):16/little,Addr/binary>>,
    ok=gen_tcp:send(Parent,parrot_gw:build_frame(16#24,7,<<>>,<<>>,Hint)),
    {ok,S}=gen_tcp:accept(L,3000),track(S),{1,1,_,_}=recv(S),
    ok=gen_tcp:send(S,parrot_gw:build_frame(2,1,<<>>,<<>>,iolist_to_binary(parrot_gw:handshake_ack_body(Node)))),
    ok=gen_tcp:send(S,heartbeat()),{4,91,_,_}=recv(S),S.
track(S) -> put(case_sockets,[S|get(case_sockets)]),ok.
heartbeat() -> parrot_gw:build_frame(3,91,<<>>,<<>>,<<>>).
terminal() -> parrot_gw:build_frame(16#7f,0,<<>>,<<>>,parrot_gw:err_payload(12,"terminal")).
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
eof(S) -> check(gen_tcp:recv(S,0,2000)=:={error,closed},terminal_connection_must_close_without_reply).
server_owner(Client) ->
    {ok,Endpoint}=inet:sockname(Client),
    {ok,Peer}=inet:peername(Client),
    Owners=[Pid || Port <- erlang:ports(),
        erlang:port_info(Port,name)=:={name,"tcp_inet"},(try inet:peername(Port) catch _:_ -> error end)=:={ok,Endpoint},
        (try inet:sockname(Port) catch _:_ -> error end)=:={ok,Peer},
                    {connected,Pid} <- [erlang:port_info(Port,connected)]],
    check(length(Owners)=:=1,server_driver_identity_must_be_unique),hd(Owners).
close_listener(Number) ->
    [try gen_tcp:close(P) catch _:_ -> ok end || P <- erlang:ports(),
        erlang:port_info(P,name)=:={name,"tcp_inet"},
        (try inet:peername(P) catch _:_ -> error end)=:={error,enotconn},
        case try inet:sockname(P) catch _:_ -> error end of {ok,{_,Number}} -> true; _ -> false end],ok.
await_down(Ref,Pid,Reason) ->
    receive {'DOWN',Ref,process,Pid,_} -> ok after 2000 -> check(false,Reason) end.
until(F,Timeout,Reason) -> until_deadline(F,erlang:monotonic_time(millisecond)+Timeout,Reason).
until_deadline(F,Deadline,Reason) ->
    case F() of true -> ok; false -> check(erlang:monotonic_time(millisecond)<Deadline,Reason),
        timer:sleep(5),until_deadline(F,Deadline,Reason) end.
check(true,_) -> ok;
check(false,Reason) -> error({contract,Reason}).
