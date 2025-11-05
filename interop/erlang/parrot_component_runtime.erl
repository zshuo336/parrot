%% Gateway-owned registry coordinator and serial, independently owned instances.
-module(parrot_component_runtime).
-behaviour(gen_server).
-export([ensure/1, deploy/5, retire/3, invoke/3, invoke/4, status/1, select_paths/3]).
-export([init/1, handle_call/3, handle_cast/2, handle_info/2, terminate/2]).
-define(TAB,parrot_admin_components).

ensure(Root) ->
    case gen_server:start({local,?MODULE},?MODULE,{management,Root},[]) of
        {ok,_} -> ok;
        {error,{already_started,_}} -> ok;
        {error,R} -> erlang:error({component_owner_start,R})
    end.
deploy(Name,Version,Paths,Load,Config) ->
    manage({deploy,Name,Version,Paths,Load,Config}).
retire(Prefix,Mode,Timeout) -> manage({retire,Prefix,Mode,Timeout}).
manage(Request) -> gen_server:call(?MODULE,{submit,Request,erlang:external_size(Request)},infinity).
invoke(Entry,Key,Payload) -> invoke(Entry,Key,Payload,16777216).
invoke(#{pid:=Pid,status:=running}=Entry,Key,Payload,ReplyBytes) when is_integer(ReplyBytes), ReplyBytes>=0, ReplyBytes=<16777216 ->
    Path=maps:get(path,Entry,<<>>),
    Request={invoke,Key,Payload,ReplyBytes,Path,parrot_trace:current(),parrot_trace:operation()},
    Result=try gen_server:call(Pid,{submit,Request,erlang:external_size(Request)},infinity)
           catch exit:_ -> erlang:error(component_stopped) end,
    case Result of {reply,Value} -> Value; {raise,C,R} -> erlang:raise(C,R,[]) end;
invoke(_,_,_,_) -> erlang:error(component_stopped).
status(#{host:=Host,path:=Path,pid:=Pid,status:=running}) ->
    case is_process_alive(Pid) of true -> parrot_beam_host:status(Host,Path); false -> <<"failed">> end;
status(#{pid:=Pid,status:=Phase}) ->
    case is_process_alive(Pid) of true -> atom_to_binary(Phase,utf8); false -> <<"failed">> end.
select_paths(Prefix,Name,Paths) ->
    case Prefix=:= <<"/user/",Name/binary>> of
        true -> Paths;
        false -> [P || P <- Paths,P=:=Prefix]
    end.

%% Admission stays responsive while the worker executes arbitrary user code.
%% Accepted work retains its count/bytes even if its connection/caller vanishes.
init({management,Root}) ->
    watch_owner(Root,self()),
    case ets:info(parrot_beam_hosts) of undefined -> ets:new(parrot_beam_hosts,[named_table,public,set]); _ -> ok end,
    {ok,Worker}=gen_server:start_link(?MODULE,{registry,self()},[]),
    {ok,gate(Worker,management)};
init({component,Owner,Module,Mode,Context}) ->
    watch_owner(Owner,self()),
    {ok,Worker}=gen_server:start_link(?MODULE,{instance,self(),Module,Mode,Context},[]),
    {ok,gate(Worker,component)};
init({registry,Owner}) ->
    process_flag(trap_exit,true),watch_owner(Owner,self()),{ok,registry};
init({instance,Owner,{isolated,Host}=Module,isolated,Context}) ->
    watch_owner(Owner,self()),watch_owner(Host,self()),
    {ok,{instance,Module,isolated,maps:get(path,Context)}};
init({instance,Owner,Module,stateful,Context}) ->
    watch_owner(Owner,self()),{ok,State}=Module:parrot_init(Context),
    {ok,{instance,Module,stateful,State}};
init({instance,Owner,Module,legacy,_Context}) ->
    watch_owner(Owner,self()),
    case erlang:function_exported(Module,parrot_init,0) of true -> ok=Module:parrot_init(); false -> ok end,
    {ok,{instance,Module,legacy,undefined}}.
gate(Worker,Kind) -> #{kind=>Kind,worker=>Worker,active=>none,queue=>queue:new(),count=>0,bytes=>0,retire=>none}.

handle_call(executor,_From,#{worker:=Pid}=State) -> {reply,Pid,State};
handle_call({submit,Request,Size},From,#{count:=Count,bytes:=Bytes,retire:=Retire,kind:=Kind}=State) ->
    case {Retire,Count<64 andalso Size>=0 andalso Size+Bytes=<16*1024*1024} of
        {none,true} ->
            Next=State#{count:=Count+1,bytes:=Bytes+Size},
            {noreply,enqueue({From,Request,Size},Next)};
        {none,false} ->
            Error=case Kind of management -> {error,component_management_overloaded}; component -> {raise,error,component_overloaded} end,
            {reply,Error,State};
        _ -> {reply,{raise,error,component_stopped},State}
    end;
handle_call(retire,From,#{count:=Count}=State) ->
    case Count of 0 -> {stop,normal,ok,State}; _ -> {noreply,State#{retire:=From}} end.
enqueue(Job,#{active:=none,worker:=Worker}=State) ->
    {From,Request,Size}=Job,Ref=make_ref(),
    gen_server:cast(Worker,{execute,self(),Ref,Request}),
    State#{active:={Ref,From,Size}};
enqueue(Job,#{queue:=Q}=State) -> State#{queue:=queue:in(Job,Q)}.
handle_cast({execute,Gate,Ref,Request},State) ->
    {Reply,Next}=execute(Request,State),Gate!{instance_reply,Ref,Reply},{noreply,Next};
handle_cast(_,State) -> {noreply,State}.
execute({deploy,Name,V,Paths,Load,Config},registry) ->
    Reply=try deploy_owned(Name,V,Paths,Load,Config) catch C:R -> {error,{C,R}} end,
    {Reply,registry};
execute({retire,Prefix,Mode,Timeout},registry) ->
    Reply=try retire_owned(Prefix,Mode,Timeout) catch C:R -> {error,{C,R}} end,
    {Reply,registry};
execute({invoke,K,B,ReplyBytes,Path,Parent,Operation},{instance,M,Mode,Old}=State) ->
    try
        {Value,Next}=case Mode of
            isolated -> {isolated,Host}=M,
                Value0=parrot_trace:scope(Parent,Operation,fun() -> parrot_beam_host:invoke(Host,Old,K,B,ReplyBytes) end),
                {Value0,Old};
            stateful ->
                HandlerReply=parrot_trace:handler(Path,Parent,Operation,fun() ->
                    case erlang:function_exported(M,parrot_service,4) of
                        true -> M:parrot_service(K,B,Old,#{reply_bytes=>ReplyBytes});
                        false -> M:parrot_service(K,B,Old)
                    end
                end),
                {reply,Reply,New}=HandlerReply,{Reply,New};
            legacy -> {parrot_trace:handler(Path,Parent,Operation,fun() -> M:parrot_service(K,B) end),Old}
        end,
        {ReplyKey,ReplyBody}=Value,
        true=is_binary(ReplyKey) andalso is_binary(ReplyBody),
        true=is_binary(unicode:characters_to_binary(ReplyKey,utf8,utf8)),
        true=byte_size(ReplyKey)+byte_size(ReplyBody)=<16777216,
        case byte_size(ReplyKey)+byte_size(ReplyBody)=<ReplyBytes of
            true -> ok; false -> erlang:error(component_overloaded)
        end,
        {{reply,Value},{instance,M,Mode,Next}}
    catch C:R -> {{raise,C,R},State} end.
handle_info({instance_reply,Ref,Reply},#{active:={Ref,From,Size},queue:=Q,count:=Count,bytes:=Bytes,retire:=Retire}=State) ->
    gen_server:reply(From,Reply),Next=State#{active:=none,count:=Count-1,bytes:=Bytes-Size},
    case queue:out(Q) of
        {{value,Job},Rest} -> {noreply,enqueue(Job,Next#{queue:=Rest})};
        {empty,_} when Retire=/=none -> gen_server:reply(Retire,ok),{stop,normal,Next};
        {empty,_} -> {noreply,Next}
    end;
handle_info(_,State) -> {noreply,State}.
terminate(_,#{worker:=Pid}) -> kill_wait(Pid),ok;
terminate(_,registry) ->
    {links,Links}=process_info(self(),links),[kill_wait(P) || P <- Links,is_pid(P)],ok;
terminate(_,_) -> ok.

entries({owned,_Module,Entries}) -> Entries;
entries(_) -> #{}.
deploy_owned(Name,V,Paths,Load,Config) ->
    Rows=ets:tab2list(?TAB),
    true=length(Paths)>0 andalso length(Paths)=<4096,
    OtherPaths=lists:append([Ps || {N,_,Ps,_} <- Rows,N=/=Name]),
    true=length(OtherPaths)+length(Paths)=<4096,
    true=not lists:any(fun(P) -> lists:member(P,OtherPaths) end,Paths),
    case ets:lookup(?TAB,Name) of
        [{_,_,_,Owned0}] ->
            true=not lists:any(fun(E) -> maps:is_key(retiring,E) end,maps:values(entries(Owned0)));
        [] -> ok
    end,
    Module=Load(),
    Mode=component_mode(Module),
    case Mode=:=legacy andalso erlang:function_exported(Module,parrot_init,0) of
        true ->
            %% Global legacy state cannot be made independent by inventing paths.
            true=length(Paths)=:=1,
            true=not lists:any(fun({_,_,_,{owned,M,_}}) -> M=:=Module; (_) -> false end,Rows);
        false -> ok
    end,
    Context=#{name=>Name,version=>V,config=>Config},
    case start_all(Module,Mode,Paths,Context,now_ms()+5000,#{}) of
        {error,Reason,Started} ->
            Cleanup=kill_entries(Started),
            HostCleanup=case Module of {isolated,H} -> parrot_beam_host:abort(H); _ -> ok end,
            case Cleanup++case HostCleanup of ok -> []; E -> [E] end of
                [] -> {error,Reason};
                Errors ->
                    preserve_failed(Name,V,Module,Started),
                    {error,{initialization_cleanup_unconfirmed,Reason,Errors}}
            end;
        {ok,Started} ->
            %% Publish a non-routable ownership record before retiring old state.
            %% Even unconfirmed termination retains every PID for a Stop retry.
            Old=case ets:lookup(?TAB,Name) of [{_,_,_,Owned}] -> entries(Owned); [] -> #{} end,
            Pending=combine_owners(Started,Old),
            ets:insert(?TAB,{Name,V,maps:keys(Pending),{owned,Module,Pending}}),
            case kill_entries(Old) of
                [] ->
                    ets:insert(?TAB,{Name,V,Paths,{owned,Module,Started}}),{deployed,Paths};
                Errors -> {error,{replacement_retirement_unconfirmed,Errors}}
            end
    end.
component_mode({isolated,_}) -> isolated;
component_mode(Module) ->
    case {erlang:function_exported(Module,parrot_init,1),erlang:function_exported(Module,parrot_service,3)} of
        {true,true} -> stateful;
        {false,false} -> legacy;
        _ -> erlang:error(incomplete_stateful_component_contract)
    end.

combine_owners(New,Old) ->
    maps:map(fun(P,E) ->
        Extra=case {maps:find(P,New),maps:find(P,Old)} of
            {{ok,_},{ok,Previous}} -> owned_pids(Previous);
            _ -> []
        end,
        Remote=case maps:find(P,Old) of {ok,Previous0} -> remote_owners(Previous0); error -> [] end,
        E#{status=>failed,retiring=>lists:usort(Extra++maps:get(retiring,E,[])),
           retiring_hosts=>lists:usort(Remote++maps:get(retiring_hosts,E,[]))}
    end,maps:merge(Old,New)).
preserve_failed(Name,V,Module,Started) ->
    Old=case ets:lookup(?TAB,Name) of [{_,_,_,O}] -> entries(O); [] -> #{} end,
    All=combine_owners(Old,Started),
    ets:insert(?TAB,{Name,V,maps:keys(All),{owned,Module,All}}).
owned_pids(E) -> lists:usort([maps:get(pid,E),maps:get(worker,E)|maps:get(retiring,E,[])]).
remote_owners(E) ->
    Own=case E of #{host:=H,path:=P} -> [{H,P}]; _ -> [] end,
    Own++maps:get(retiring_hosts,E,[]).
retire_remote(E) ->
    [R || {H,P} <- lists:usort(remote_owners(E)),R <- [parrot_beam_host:retire(H,P)],R=/=ok].
kill_entries(Entries) ->
    Local=[R || P <- lists:usort(lists:append([owned_pids(E) || E <- maps:values(Entries)])),
          R <- [kill_wait(P)],R=/=ok],
    Local++lists:append([retire_remote(E) || E <- maps:values(Entries)]).

start_all(_,_,[],_,_,Acc) -> {ok,Acc};
start_all(M,Mode,[P|Rest],Context,Deadline,Acc) ->
    Remaining=Deadline-now_ms(),
    case Remaining>0 of
        false -> {error,component_initialization_timeout,Acc};
        true ->
            case gen_server:start_link(?MODULE,{component,self(),M,Mode,Context#{path=>P}},[{timeout,Remaining}]) of
                {ok,Pid} ->
                    Ready=try {ok,gen_server:call(Pid,executor)} catch exit:Why -> {error,Why} end,
                    case Ready of
                        {ok,Worker} ->
                            Entry0=#{pid=>Pid,worker=>Worker,status=>running,path=>P},
                            Entry=case M of {isolated,H} -> Entry0#{host=>H,path=>P}; _ -> Entry0 end,
                            start_all(M,Mode,Rest,Context,Deadline,Acc#{P=>Entry});
                        {error,Why0} -> kill_wait(Pid),{error,{instance_died_during_start,Why0},Acc}
                    end;
                {error,R} -> {error,{component_initialization_failed,R},Acc}
            end
    end.

retire_owned(Prefix,Mode,Timeout) ->
    Rows=[{N,V,Ps,O,select_paths(Prefix,N,Ps)} || {N,V,Ps,O} <- ets:tab2list(?TAB)],
    Selected=[R || {_,_,_,_,[_|_]}=R <- Rows],
    case Selected of
        [] -> not_found;
        _ ->
            Deadline=now_ms()+Timeout,
            %% Close admission to all selected paths before waiting for any one.
            [case O of
                {owned,M,Es} ->
                    Marked=lists:foldl(fun(P,A) -> A#{P:=(maps:get(P,A))#{status=>draining}} end,Es,Sel),
                    ets:insert(?TAB,{N,V,Ps,{owned,M,Marked}});
                _ -> ok
             end || {N,V,Ps,O,Sel} <- Selected],
            Results=lists:append([retire_row(N,V,Ps,O,Sel,Mode,Deadline) || {N,V,Ps,O,Sel} <- Selected]),
            Errors=[R || {error,R} <- Results],
            case Errors of
                [] -> {retired,length([ok || drained <- Results]),length([ok || aborted <- Results])};
                _ -> {error,{retirement_unconfirmed,Errors}}
            end
    end.
retire_row(N,V,Paths,{owned,M,Es},Selected,Mode,Deadline) ->
    {Results,Left}=lists:foldl(fun(P,{Rs,Entries}) ->
        Entry=maps:get(P,Entries),
        Result=case maps:get(retiring,Entry,[]) of
            [] ->
                Retired=retire_pid(maps:get(pid,Entry),Mode,Deadline),
                case {kill_wait(maps:get(worker,Entry)),retire_remote(Entry)} of
                    {ok,[]} -> Retired; {E,Errors} -> {error,{E,Errors}} end;
            _ -> case kill_entries(#{P=>Entry}) of [] -> aborted; Errors -> {error,Errors} end
        end,
        case Result of
            {error,_} -> {[Result|Rs],Entries#{P:=(maps:get(P,Entries))#{status=>draining}}};
            _ -> {[Result|Rs],maps:remove(P,Entries)}
        end
    end,{[],Es},Selected),
    Remaining=[P || P <- Paths,maps:is_key(P,Left)],
    case Remaining of [] -> ets:delete(?TAB,N); _ -> ets:insert(?TAB,{N,V,Remaining,{owned,M,Left}}) end,
    Results;
retire_row(N,V,Paths,Module,Selected,_Mode,_Deadline) ->
    Left=Paths--Selected,
    case Left of [] -> ets:delete(?TAB,N); _ -> ets:insert(?TAB,{N,V,Left,Module}) end,
    [drained || _ <- Selected].
retire_pid(Pid,stop,_) -> case kill_wait(Pid) of ok -> aborted; Error -> Error end;
retire_pid(Pid,drain,Deadline) ->
    Ref=erlang:monitor(process,Pid),
    try
        Remaining=max(0,Deadline-now_ms()),
        Retired=try gen_server:call(Pid,retire,Remaining) catch exit:_ -> failed end,
        case Retired of
            ok ->
                receive {'DOWN',Ref,process,Pid,_} -> drained
                after 5000 -> {error,retirement_confirmation_timeout} end;
            _ -> case kill_wait(Pid) of ok -> aborted; Error -> Error end
        end
    after erlang:demonitor(Ref,[flush]) end.
kill_wait(Pid) ->
    Ref=erlang:monitor(process,Pid),exit(Pid,kill),
    receive {'DOWN',Ref,process,Pid,_} -> ok
    after 5000 -> erlang:demonitor(Ref,[flush]),{error,kill_confirmation_timeout} end.
now_ms() -> erlang:monotonic_time(millisecond).

%% A separate monitor can kill a blocked coordinator and an instance that has
%% enabled trap_exit; neither can ignore owner loss while running user code.
watch_owner(Owner,Child) ->
    spawn(fun() ->
        O=erlang:monitor(process,Owner),C=erlang:monitor(process,Child),
        receive
            {'DOWN',O,process,Owner,_} -> exit(Child,kill);
            {'DOWN',C,process,Child,_} -> ok
        end
    end),ok.
