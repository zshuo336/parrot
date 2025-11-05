%% One BEAM VM per deployment, with an owner-bound immutable source snapshot.
-module(parrot_beam_host).
-behaviour(gen_server).
-export([start/7,invoke/4,invoke/5,retire/2,abort/1,status/2,describe/1,bootstrap/0,retire_orphans/1]).
-export([init/1,handle_call/3,handle_cast/2,handle_info/2]).
-define(TAB,parrot_beam_hosts).
-define(RESERVE,68157440). % 64 MiB source plus 1 MiB trusted bootstrap.

start(Owner,App,Uri,Name,Version,Paths,Config) ->
    {unix,_}=os:type(),
    true=not lists:member(App,[<<"parrot_gw">>,<<"parrot_component_runtime">>,
        <<"parrot_beam_guest">>,<<"parrot_beam_host">>,<<"parrot_beam_snapshot">>,<<"parrot_trace">>]),
    %% This does not intern the entry name or modify the gateway code path.
    {File,Entry}=parrot_gw:artifact_source(App,Uri),
    _=bootstrap(),
    case retire_orphans(<<"/user/",Name/binary>>) of
        not_found -> ok; ok -> ok; Error -> erlang:error(Error)
    end,
    Rows=ets:tab2list(?TAB),
    case length(Rows)<64 andalso lists:sum([B || {_,_,_,B,_,_} <- Rows])+?RESERVE=<268435456 of
        true -> ok; false -> erlang:error(beam_host_overloaded)
    end,
    Ref=make_ref(),
    Dir="/tmp/parrot-beam-host-"++os:getpid()++"-"++binary_to_list(binary:encode_hex(rand:bytes(16))),
    true=ets:insert_new(?TAB,{Ref,undefined,undefined,?RESERVE,Dir,Name}),
    try gen_server:start_link(?MODULE,{Owner,Ref,Dir,App,File,Entry,Name,Version,Paths,Config},[{timeout,25000}]) of
        {ok,Host} -> {isolated,Host};
        {error,R} -> await_failed_cleanup(Ref),erlang:error({beam_host_start,R})
    catch C:R:S ->
        %% The janitor owns cleanup once installed. Before that there are no
        %% processes or files to retire, so the reservation can be removed.
        case ets:lookup(?TAB,Ref) of [{Ref,undefined,undefined,_,_,_}] -> ets:delete(?TAB,Ref); _ -> ok end,
        erlang:raise(C,R,S)
    end.
await_failed_cleanup(Ref) ->
    case ets:lookup(?TAB,Ref) of
        [{Ref,_,Guard,_,_,_}] when is_pid(Guard) ->
            Mon=monitor(process,Guard),
            receive {'DOWN',Mon,process,Guard,_} -> ok
            after 15000 -> demonitor(Mon,[flush]),ok end;
        _ -> ok
    end.
init({Owner,Ref,Dir,App,File,Entry,Name,Version,Paths,Config}) ->
    process_flag(trap_exit,true),Host=self(),
    Guard=spawn(fun() -> janitor(Owner,Host,Ref,Dir,undefined,undefined) end),
    true=ets:update_element(?TAB,Ref,[{2,Host},{3,Guard}]),
    receive
        {Guard,root_owned} -> ok;
        {Guard,{root_error,RootError}} -> erlang:error({snapshot_root,RootError})
    after 5000 -> erlang:error(snapshot_root_timeout) end,
    Deadline=erlang:monotonic_time(millisecond)+20000,
    {Data,Boot,Bytes}=parrot_beam_snapshot:create(Dir,File,Entry,Deadline),
    true=ets:update_element(?TAB,Ref,{4,Bytes}),
    Options=#{connection=>standard_io,peer_down=>crash,wait_boot=>remaining(Deadline,10000),
              args=>["+S","2:2","+A","2","-no_dot_erlang","-pa",Boot],
              env=>[{"ERL_LIBS",""},{"ERL_FLAGS",""},{"ERL_AFLAGS",""},{"ERL_ZFLAGS",""}]},
    {ok,Peer,nonode@nohost}=peer:start_link(Options),
    %% Register the controller before any subsequent RPC can fail.
    Guard!{peer,Peer,undefined},
    GuestPid=peer:call(Peer,os,getpid,[],remaining(Deadline,2000)),
    true=lists:all(fun(C)->C>=$0 andalso C=<$9 end,GuestPid),true=GuestPid=/=[],
    Guard!{peer,Peer,GuestPid},
    ok=peer:call(Peer,file,set_cwd,[Data],remaining(Deadline,2000)),
    [begin {module,M}=peer:call(Peer,code,load_abs,[filename:join(Boot,atom_to_list(M))],remaining(Deadline,2000)) end
     || M <- [parrot_trace,parrot_beam_guest,parrot_component_runtime,parrot_gw]],
    Timeout=remaining(Deadline,6500),
    case peer:call(Peer,parrot_beam_guest,boot,[App,list_to_binary(Data),Name,Version,Paths,Config,5000],Timeout) of
        <<0>> -> {ok,#{peer=>Peer,os_pid=>GuestPid,guard=>Guard,ref=>Ref,dir=>Dir,paths=>Paths}};
        <<4,Error/binary>> -> erlang:error({component_boot,Error});
        _ -> erlang:error(invalid_guest_boot_reply)
    end.
remaining(Deadline,Max) -> N=Deadline-erlang:monotonic_time(millisecond),true=N>0,min(N,Max).

describe(Host) -> gen_server:call(Host,describe,1000).
invoke(Host,Path,Key,Body) -> invoke(Host,Path,Key,Body,16777216).
invoke(Host,Path,Key,Body,ReplyBytes) ->
    Reply=try
        Peer=gen_server:call(Host,peer,1000),
        peer:call(Peer,parrot_beam_guest,invoke,
                  [Path,Key,Body,ReplyBytes,parrot_trace:current(),parrot_trace:operation()],infinity)
    catch exit:_ -> erlang:error(component_stopped) end,
    case Reply of
        <<0,N:32/big,K:N/binary,B/binary>> when N+byte_size(B)=<ReplyBytes -> {K,B};
        <<1>> -> erlang:error({unknown_service,Key});
        <<2>> -> erlang:error(component_overloaded);
        <<3>> -> erlang:error(component_stopped);
        <<4,Error/binary>> when byte_size(Error)=<8192 -> erlang:error({component_error,Error});
        _ -> erlang:error(invalid_guest_reply)
    end.
status(Host,Path) ->
    try
        Peer=gen_server:call(Host,peer,1000),
        case peer:call(Peer,parrot_beam_guest,status,[Path],1000) of <<0>> -> <<"running">>; _ -> <<"failed">> end
    catch _:_ -> <<"failed">> end.
retire(Host,Path) -> call_retire(Host,{retire,Path}).
abort(Host) -> call_retire(Host,abort).
call_retire(Host,Command) ->
    try gen_server:call(Host,Command,infinity)
    catch exit:_ ->
        case ets:info(?TAB) of
            undefined -> {error,beam_host_owner_stopped};
            _ -> case [R || {_,H,_,_,_,_}=R <- ets:tab2list(?TAB),H=:=Host] of
                [] -> ok; Rows -> cleanup_rows(Rows) end
        end
    end.
handle_call(describe,_,S) -> {reply,maps:without([guard],S),S};
handle_call(peer,_,#{peer:=P}=S) -> {reply,P,S};
handle_call(abort,_,S) -> finish(S#{paths:=[]});
handle_call({retire,Path},_,#{peer:=P,paths:=Paths}=S) ->
    Reply=case lists:member(Path,Paths) of
        false -> <<0>>;
        true -> try peer:call(P,parrot_beam_guest,retire,[Path],6500) catch _:_ -> failed end
    end,
    case Reply of
        <<0>> ->
            Next=S#{paths:=lists:delete(Path,Paths)},
            case maps:get(paths,Next) of [] -> finish(Next); _ -> {reply,ok,Next} end;
        _ -> {reply,{error,guest_retirement_unconfirmed},S}
    end.
finish(#{guard:=Guard,ref:=Resource}=S) ->
    Ref=make_ref(),Mon=monitor(process,Guard),Guard!{cleanup,self(),Ref},
    try receive
        {Ref,ok} -> {stop,normal,ok,S};
        {Ref,Error} -> {reply,Error,S};
        {'DOWN',Mon,process,Guard,_} ->
            %% An earlier timed-out cleanup may have completed while this
            %% retry was queued. Its receipt belongs to the earlier call.
            case ets:info(?TAB)=/=undefined andalso not ets:member(?TAB,Resource) of
                true -> {stop,normal,ok,S};
                false -> {reply,{error,beam_cleanup_unconfirmed},S}
            end
    after 15000 -> {reply,{error,beam_host_cleanup_timeout},S} end
    after demonitor(Mon,[flush]) end.

handle_cast(_,S) -> {noreply,S}.
handle_info({'EXIT',P,_},#{peer:=P}=S) -> {stop,normal,S};
handle_info(_,S) -> {noreply,S}.

%% Cleanup is independent of blocked init, RPC, and trap_exit in user code.
janitor(Owner,Host,Ref,Dir,Peer,Pid) ->
    O=monitor(process,Owner),H=monitor(process,Host),
    case parrot_beam_snapshot:claim(Dir) of
        {owned,ok} -> Host!{self(),root_owned},janitor_loop(O,H,Host,Ref,Dir,Peer,Pid);
        {owned,Error} -> Host!{self(),{root_error,Error}},orphan_cleanup(Ref,Dir,Peer,Pid);
        {unowned,Error} ->
            %% In particular, EEXIST must never delete a previous owner's data.
            Host!{self(),{root_error,Error}},
            try ets:delete(?TAB,Ref) catch _:_ -> ok end
    end.
janitor_loop(O,H,Host,Ref,Dir,Peer,Pid) ->
    receive
        {peer,P,Id} -> janitor_loop(O,H,Host,Ref,Dir,P,Id);
        {cleanup,Caller,Tag} ->
            Result=cleanup(Ref,Dir,Peer,Pid),Caller!{Tag,Result},
            case Result of ok -> ok; _ -> janitor_loop(O,H,Host,Ref,Dir,Peer,Pid) end;
        {'DOWN',O,process,_,_} ->
            exit(Host,kill),
            receive {'DOWN',H,process,Host,_} -> ok end,
            {P1,Id1}=pending_peer(Peer,Pid),orphan_cleanup(Ref,Dir,P1,Id1);
        {'DOWN',H,process,Host,_} -> orphan_cleanup(Ref,Dir,Peer,Pid)
    end.
pending_peer(Peer,Pid) ->
    receive {peer,P,Id} -> pending_peer(P,Id) after 0 -> {Peer,Pid} end.
cleanup(Ref,Dir,Peer,Pid) ->
    try cleanup_resources(Ref,Dir,Peer,Pid)
    catch C:R -> {error,{beam_cleanup,C,R}} end.
orphan_cleanup(Ref,Dir,Peer,Pid) ->
    case cleanup(Ref,Dir,Peer,Pid) of
        ok -> ok;
        _ -> orphan_loop(Ref,Dir,Peer,Pid)
    end.
orphan_loop(Ref,Dir,Peer,Pid) ->
    receive
        {cleanup,Caller,Tag} ->
            R=cleanup(Ref,Dir,Peer,Pid),Caller!{Tag,R},
            case R of ok -> ok; _ -> orphan_loop(Ref,Dir,Peer,Pid) end
    after 1000 -> orphan_cleanup(Ref,Dir,Peer,Pid) end.
cleanup_resources(Ref,Dir,Peer,Pid) ->
    case Peer of undefined -> ok; _ -> try peer:stop(Peer) catch _:_ -> ok end end,
    case await_os_exit(Pid,erlang:monotonic_time(millisecond)+6000) of
        ok -> case parrot_beam_snapshot:remove(Dir) of
            ok -> try ets:delete(?TAB,Ref) catch _:_ -> ok end,ok;
            Error -> {error,{snapshot_cleanup,Error}}
        end;
        Error -> Error
    end.
await_os_exit(undefined,_) -> ok;
await_os_exit(Pid,Deadline) ->
    case os_alive(Pid) of
        false -> ok;
        true -> case erlang:monotonic_time(millisecond)<Deadline of
            true -> timer:sleep(25),await_os_exit(Pid,Deadline);
            false -> {error,beam_os_exit_unconfirmed}
        end
    end.
os_alive(Pid) ->
    Port=open_port({spawn_executable,"/bin/sh"},[exit_status,binary,stderr_to_stdout,
        {args,["-c","kill -0 \"$1\" 2>/dev/null","parrot-owned-pid-probe",Pid]}]),
    try probe_exit(Port) after try port_close(Port) catch _:_ -> ok end end.
probe_exit(Port) ->
    receive
        {Port,{exit_status,0}} -> true;
        {Port,{exit_status,_}} -> false;
        {Port,{data,_}} -> probe_exit(Port)
    after 1000 -> true end.

%% Escript registers its in-memory compiled bytes under the same fixed keys.
%% Normal launch pins the trusted modules on first use, before source artifacts
%% can be removed or replaced by an application deployment.
bootstrap() ->
    [begin
        Key={parrot_beam_bootstrap,M},
        B=case persistent_term:get(Key,undefined) of
            undefined ->
                {module,M}=code:ensure_loaded(M),{M,Bytes,_}=code:get_object_code(M),
                true=byte_size(Bytes)=<1048576,
                true=code:module_md5(Bytes)=:=erlang:get_module_info(M,md5),
                persistent_term:put(Key,Bytes),Bytes;
            Bytes when is_binary(Bytes) -> Bytes
        end,
        {M,B}
     end || M <- [parrot_trace,parrot_beam_guest,parrot_component_runtime,parrot_gw]].

retire_orphans(Prefix) ->
    Rows=try [R || {_,H,_,_,_,Name}=R<-ets:tab2list(?TAB),
                       is_pid(H),not is_process_alive(H),Prefix=:= <<"/user/",Name/binary>>]
         catch error:badarg -> [] end,
    case Rows of [] -> not_found; _ -> cleanup_rows(Rows) end.
cleanup_rows(Rows) ->
    Errors=[R || {_,_,Guard,_,_,_}<-Rows,R<-[cleanup_guard(Guard)],R=/=ok],
    case Errors of [] -> ok; _ -> {error,{beam_host_cleanup_pending,Errors}} end.
cleanup_guard(Guard) ->
    Ref=make_ref(),Mon=monitor(process,Guard),Guard!{cleanup,self(),Ref},
    try receive
        {Ref,R} -> R;
        {'DOWN',Mon,process,Guard,_} ->
            case [R || {_,_,G,_,_,_}=R<-ets:tab2list(?TAB),G=:=Guard] of
                [] -> ok; _ -> {error,beam_cleanup_guard_unavailable} end
    after 15000 -> {error,beam_cleanup_pending} end
    after demonitor(Mon,[flush]) end.
