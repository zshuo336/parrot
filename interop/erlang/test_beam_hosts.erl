%% Public gateway operations backed by actual child OS processes and source copies.
-module(test_beam_hosts).
-export([run/0,run_reply_validation/0,run_root_collision/0]).
run() ->
    Tests=[{"same module versions coexist and gateway code path stays unchanged",fun() ->
        Before=code:get_path(),ok=deploy(<<"old">>,singleton,<<"normal">>),
        OldPid=identity(<<"/user/old">>),true=OldPid=/=list_to_binary(os:getpid()),
        compile(2),ok=deploy(<<"new">>,singleton,<<"normal">>),
        1=count(<<"/user/old">>,<<"magic">>),2=count(<<"/user/new">>,<<"magic">>),
        true=OldPid=/=identity(<<"/user/new">>),Before=code:get_path(),false=code:is_loaded(test_beam_host_fixture) end},
      {"failed upgrade retains old code and private state",fun() ->
        ok=deploy(<<"keep">>,singleton,<<"normal">>),1=count(<<"/user/keep">>,<<"inc">>),Old=identity(<<"/user/keep">>),
        compile(99),{failed,16#0A06,_}=deploy_result(<<"keep">>,{pool,2},<<"fail-second">>),
        1=count(<<"/user/keep">>,<<"magic">>),1=count(<<"/user/keep">>,<<"count">>),Old=identity(<<"/user/keep">>),
        dead(created(<<"/user/keep-0">>)),dead(created(<<"/user/keep-1">>)),1=ets:info(parrot_beam_hosts,size) end},
      {"deleted original files cannot break lazy dependency or resources",fun() ->
        ok=deploy(<<"old">>,singleton,<<"normal">>),compile(2),
        ok=deploy(<<"new">>,singleton,<<"normal">>),ok=file:del_dir_r(get(source)),
        {<<"lazy">>,<<1:64/little,"resource-1">>}=call(<<"/user/old">>,<<"lazy">>),
        {<<"lazy">>,<<2:64/little,"resource-2">>}=call(<<"/user/new">>,<<"lazy">>) end},
      {"pool members share a host but keep independent state and exact retirement",fun() ->
        ok=deploy(<<"pool">>,{pool,2},<<"normal">>),P=identity(<<"/user/pool-0">>),P=identity(<<"/user/pool-1">>),
        1=count(<<"/user/pool-0">>,<<"inc">>),0=count(<<"/user/pool-1">>,<<"count">>),
        H=host(<<"pool">>),#{dir:=Dir}=parrot_beam_host:describe(H),
        stopped=parrot_gw:admin_stop(<<"/user/pool-0">>),true=alive(P),true=filelib:is_dir(Dir),
        0=count(<<"/user/pool-1">>,<<"count">>),stopped=parrot_gw:admin_stop(<<"/user/pool-1">>),
        dead(P),false=filelib:is_dir(Dir),0=ets:info(parrot_beam_hosts,size) end},
      {"Stop interrupts a trapping blocked guest member",fun() ->
        {P,T}=busy(),0=count(<<"/user/pool-1">>,<<"count">>),
        stopped=parrot_gw:admin_stop(<<"/user/pool-0">>),stopped_task(T),
        true=alive(P),0=count(<<"/user/pool-1">>,<<"count">>) end},
      {"Drain waits for accepted work and then releases the VM",fun() ->
        ok=deploy(<<"work">>,singleton,<<"normal">>),P=identity(<<"/user/work">>),
        T=async(fun()->count(<<"/user/work">>,<<"block">>) end),entered(<<"/user/work">>),
        D=async(fun()->parrot_gw:admin_drain(<<"/user/work">>,3000) end),
        until(fun()->parrot_gw:admin_status(<<"/user/work">>)=:={status_reply,[{<<"/user/work">>,<<"draining">>,<<"1">>}]} end,1000),
        rejected(<<"/user/work">>),resume(<<"/user/work">>),{ok,1}=result(T),{ok,{drained_reply,1,0}}=result(D),dead(P) end},
      {"Drain timeout aborts a guest call and confirms host exit",fun() ->
        {P,T}=busy(),{drained_reply,0,2}=parrot_gw:admin_drain(<<"/user/pool">>,0),stopped_task(T),dead(P) end},
      {"caller death keeps accepted work and quota until actual completion",fun() ->
        ok=deploy(<<"work">>,singleton,<<"normal">>),T=async(fun()->count(<<"/user/work">>,<<"block">>) end),entered(<<"/user/work">>),
        exit(element(2,T),kill),Gate=gate(<<"work">>),1=maps:get(count,sys:get_state(Gate)),
        resume(<<"/user/work">>),1=count(<<"/user/work">>,<<"count">>) end},
      {"guest worker death is observable without killing its sibling",fun() ->
        ok=deploy(<<"pool">>,{pool,2},<<"normal">>),P=identity(<<"/user/pool-0">>),
        rejected_call(<<"/user/pool-0">>,<<"die">>),
        {status_reply,[{<<"/user/pool-0">>,<<"failed">>,<<"1">>}]}=parrot_gw:admin_status(<<"/user/pool-0">>),
        P=identity(<<"/user/pool-1">>),0=count(<<"/user/pool-1">>,<<"count">>) end},
      {"whole guest crash does not affect another deployment",fun() ->
        ok=deploy(<<"bad">>,singleton,<<"normal">>),ok=deploy(<<"good">>,singleton,<<"normal">>),
        P=identity(<<"/user/bad">>),rejected_call(<<"/user/bad">>,<<"halt">>),dead(P),
        until(fun()->parrot_gw:admin_status(<<"/user/bad">>)=:={status_reply,[{<<"/user/bad">>,<<"failed">>,<<"1">>}]} end,2000),
        1=count(<<"/user/good">>,<<"inc">>),stopped=parrot_gw:admin_stop(<<"/user/bad">>) end},
      {"gateway owner death retires the VM and its snapshot",fun() ->
        {P,T}=busy(),#{dir:=Dir}=parrot_beam_host:describe(host(<<"pool">>)),
        exit(get(root),kill),stopped_task(T),dead(P),until(fun()->not filelib:is_dir(Dir) end,3000) end},
      {"coordinator death retires a blocked guest",fun() ->
        {P,T}=busy(),exit(whereis(parrot_component_runtime),kill),stopped_task(T),dead(P) end},
      {"blocked initialization times out and releases source and host quotas",fun() ->
        T=async(fun()->deploy_result(<<"blocked">>,singleton,<<"block-init">>) end),
        until(fun()->filelib:is_file(marker(<<"/user/blocked">>,".created")) end,3000),P=created(<<"/user/blocked">>),
        {ok,{failed,16#0A06,_}}=result(T),dead(P),0=ets:info(parrot_beam_hosts,size) end},
      {"owner death interrupts guest initialization",fun() ->
        T=async(fun()->deploy_result(<<"blocked">>,singleton,<<"block-init">>) end),
        until(fun()->filelib:is_file(marker(<<"/user/blocked">>,".created")) end,3000),P=created(<<"/user/blocked">>),
        exit(get(root),kill),{ok,{failed,16#0A06,_}}=result(T),dead(P) end},
      {"64 accepted proxy operations retain quota after caller cancellation",fun() ->
        ok=deploy(<<"work">>,singleton,<<"normal">>),First=async(fun()->count(<<"/user/work">>,<<"block">>) end),entered(<<"/user/work">>),
        Gate=gate(<<"work">>),Jobs=[async(fun()->count(<<"/user/work">>,<<"inc">>) end)||_<-lists:seq(1,63)],
        until(fun()->maps:get(count,sys:get_state(Gate))=:=64 end,2000),[exit(P,kill)||{_,P}<-Jobs],
        overloaded=try call(<<"/user/work">>,<<"inc">>),accepted catch error:component_overloaded -> overloaded end,
        resume(<<"/user/work">>),{ok,1}=result(First),
        until(fun()->maps:get(count,sys:get_state(Gate))=:=0 end,3000),64=count(<<"/user/work">>,<<"count">>) end},
      {"invalid reply does not commit the returned next state",fun reply_validation/0},
      {"reply byte budget is enforced inside the guest",fun() ->
        ok=deploy(<<"work">>,singleton,<<"normal">>),
        rejected_call(<<"/user/work">>,<<"large_reply">>),1=count(<<"/user/work">>,<<"inc">>) end},
      {"preexisting snapshot roots are never claimed or deleted",fun root_collision/0},
      {"snapshot rejects symlink resources and returns its reservation",fun() ->
        ok=file:make_symlink("resource.txt",filename:join(get(source),"link.txt")),
        {failed,16#0A06,_}=deploy_result(<<"bad">>,singleton,<<"normal">>),0=ets:info(parrot_beam_hosts,size),
        []=ets:tab2list(parrot_admin_components) end},
      {"unconfirmed cleanup retains quota and a second Stop completes retirement",fun() ->
        ok=deploy(<<"work">>,singleton,<<"normal">>),P=identity(<<"/user/work">>),H=host(<<"work">>),
        #{guard:=Guard,dir:=Dir}=sys:get_state(H),true=erlang:suspend_process(Guard),
        try
            {failed,16#0A06,_}=parrot_gw:admin_stop(<<"/user/work">>),
            1=ets:info(parrot_beam_hosts,size),true=filelib:is_dir(Dir),
            {status_reply,[{<<"/user/work">>,<<"failed">>,<<"1">>}]}=parrot_gw:admin_status(<<"/user/work">>)
        after case is_process_alive(Guard) of true -> erlang:resume_process(Guard); false -> ok end end,
        stopped=parrot_gw:admin_stop(<<"/user/work">>),dead(P),false=filelib:is_dir(Dir),
        0=ets:info(parrot_beam_hosts,size) end},
      {"64 real hosts enforce the count budget and release it on Stop",fun() ->
        [ok=deploy(<<"host",(integer_to_binary(I))/binary>>,singleton,<<"normal">>) || I<-lists:seq(1,64)],
        64=ets:info(parrot_beam_hosts,size),
        {failed,10,_}=deploy_result(<<"extra">>,singleton,<<"normal">>),
        1=count(<<"/user/host1">>,<<"inc">>),stopped=parrot_gw:admin_stop(<<"/user/host1">>),
        ok=deploy(<<"extra">>,singleton,<<"normal">>),64=ets:info(parrot_beam_hosts,size) end},
      {"real snapshots enforce aggregate bytes and release them on Stop",fun() ->
        File=filename:join(get(source),"large-resource.bin"),{ok,F}=file:open(File,[write,binary]),
        {ok,_}=file:position(F,50*1024*1024-1),ok=file:write(F,<<0>>),ok=file:close(F),
        [ok=deploy(<<"bytes",(integer_to_binary(I))/binary>>,singleton,<<"normal">>) || I<-lists:seq(1,4)],
        Total=lists:sum([B || {_,_,_,B,_,_}<-ets:tab2list(parrot_beam_hosts)]),
        true=Total>=200*1024*1024,true=Total<256*1024*1024,
        {failed,10,_}=deploy_result(<<"extra">>,singleton,<<"normal">>),
        stopped=parrot_gw:admin_stop(<<"/user/bytes1">>),
        ok=deploy(<<"extra">>,singleton,<<"normal">>),4=ets:info(parrot_beam_hosts,size) end},
      {"gateway accepts boundary and Unicode names with isolated state",fun() ->
        Max=binary:copy(<<"x">>,256),ok=deploy(Max,singleton,<<"normal">>),
        1=count(<<"/user/",Max/binary>>,<<"inc">>),
        Unicode=unicode:characters_to_binary("实例"),ok=deploy(Unicode,singleton,<<"normal">>),
        0=count(<<"/user/",Unicode/binary>>,<<"count">>) end},
      {"real Wire ASKs retain both module versions",fun() ->
        ok=deploy(<<"old">>,singleton,<<"normal">>),compile(2),ok=deploy(<<"new">>,singleton,<<"normal">>),
        {ok,Port}=parrot_gw:start(0),{ok,S}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}]),
        try
            H=iolist_to_binary(parrot_gw:handshake_body("host-contract")),
            ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,H)),{2,1,_,_}=recv(S),
            [begin ok=gen_tcp:send(S,parrot_gw:build_frame(16#10,I,Path,<<"magic">>,<<0:32/little>>)),
                {16#11,I,<<"count">>,<<Expected:64/little>>}=recv(S)
             end || {I,Path,Expected} <- [{2,<<"/user/old">>,1},{3,<<"/user/new">>,2},{4,<<"/user/old">>,1}]]
        after gen_tcp:close(S) end end},
      {"context retains deployment name version path and original config",fun() ->
        ok=deploy(<<"context">>,{pool,2},<<"normal">>),
        {<<"context">>,B}=call(<<"/user/context-1">>,<<"context">>),
        #{name:=<<"context">>,version:=<<"1">>,path:=<<"/user/context-1">>,config:=C}=binary_to_term(B,[safe]),
        C=config(<<"normal">>) end}],
    Fail=lists:foldl(fun({Name,F},N)->
        try fixture(F),io:format("BEAM_HOST_OK ~s~n",[Name]),N
        catch C:R:Stack -> io:format("BEAM_HOST_FAIL ~s ~p:~p ~p~n",[Name,C,R,Stack]),N+1 end
    end,0,Tests),
    case Fail of 0 -> ok; _ -> error({beam_host_failures,Fail}) end.
fixture(F) ->
    Dir="/tmp/parrot-host-test-"++os:getpid()++"-"++integer_to_list(erlang:unique_integer([positive])),
    ok=file:make_dir(Dir),Source=filename:join(Dir,"source"),Control=filename:join(Dir,"control"),
    ok=file:make_dir(Source),ok=file:make_dir(Control),put(source,Source),put(control,Control),compile(1),
    Self=self(),{Root,Mon}=spawn_monitor(fun()->parrot_gw:admin_init(),Self!{root,self()},receive stop->ok end end),
    receive {root,Root}->ok end,put(root,Root),
    try F()
    after
        exit(Root,kill),receive {'DOWN',Mon,process,Root,_}->ok after 1000->error(root_survived) end,
        until(fun()->whereis(parrot_component_runtime)=:=undefined end,3000),
        {ok,Files}=file:list_dir(Control),
        [begin {ok,P}=file:read_file(filename:join(Control,File)),dead(P) end || File<-Files,filename:extension(File)=:=".created"],
        file:del_dir_r(Dir)
    end.
compile(Magic) ->
    {ok,B}=file:read_file("test_beam_host_fixture.erl"),
    Text=binary:replace(B,<<"-define(MAGIC, 1).">>,iolist_to_binary(io_lib:format("-define(MAGIC, ~p).",[Magic]))),
    Source=get(source),File=filename:join(Source,"test_beam_host_fixture.erl"),ok=file:write_file(File,Text),
    {ok,test_beam_host_fixture}=compile:file(File,[{outdir,Source}]),
    Dep=filename:join(Source,"test_beam_host_dependency.erl"),
    ok=file:write_file(Dep,io_lib:format("-module(test_beam_host_dependency).~n-export([value/0]).~nvalue()->~p.~n",[Magic])),
    {ok,test_beam_host_dependency}=compile:file(Dep,[{outdir,Source}]),
    ok=file:write_file(filename:join(Source,"resource.txt"),io_lib:format("resource-~p",[Magic])).
config(Mode) -> C=list_to_binary(get(control)),<<(byte_size(C)):32/big,C/binary,Mode/binary>>.
deploy_result(N,P,M) -> parrot_gw:admin_deploy({N,<<"1">>,{beam,<<"test_beam_host_fixture">>,list_to_binary("file://"++get(source))},P,config(M)}).
deploy(N,P,M) -> {deployed,_}=deploy_result(N,P,M),ok.
call(P,K) -> parrot_gw:service(P,K,<<>>).
count(P,K) -> {<<"count">>,<<N:64/little>>}=call(P,K),N.
identity(P) -> {<<"pid">>,N}=call(P,<<"identity">>),N.
marker(Path,Suffix) -> filename:join(get(control),binary_to_list(binary:encode_hex(erlang:md5(Path)))++Suffix).
created(P) -> {ok,Pid}=file:read_file(marker(P,".created")),Pid.
entered(P) -> until(fun()->filelib:is_file(marker(P,".entered")) end,2000).
resume(P) -> ok=file:write_file(marker(P,".resume"),<<1>>).
async(F) ->
    Parent=self(),Ref=make_ref(),Dict=get(),P=spawn(fun()->[put(K,V)||{K,V}<-Dict],
        R=try {ok,F()} catch C:E->{error,C,E} end,Parent!{Ref,R} end),{Ref,P}.
result({Ref,_}) -> receive {Ref,R}->R after 20000->error(task_timeout) end.
stopped_task(T) -> {error,error,component_stopped}=result(T).
rejected(P) -> stopped=try call(P,<<"inc">>),accepted catch error:component_stopped -> stopped end.
rejected_call(P,K) -> rejected=try call(P,K),accepted catch error:_ -> rejected end.
until(F,Timeout) -> until_deadline(F,erlang:monotonic_time(millisecond)+Timeout).
until_deadline(F,D) -> case F() of true->ok; false->true=erlang:monotonic_time(millisecond)<D,timer:sleep(5),until_deadline(F,D) end.
alive(Pid) -> []=/=string:trim(os:cmd("ps -p "++binary_to_list(Pid)++" -o pid=" )).
dead(Pid) -> until(fun()->not alive(Pid) end,10000).
host(Name) -> [{Name,_,_,{owned,{isolated,H},_}}]=ets:lookup(parrot_admin_components,Name),H.
gate(Name) -> [{Name,_,[P],{owned,_,E}}]=ets:lookup(parrot_admin_components,Name),maps:get(pid,maps:get(P,E)).
busy() ->
    ok=deploy(<<"pool">>,{pool,2},<<"normal">>),P=identity(<<"/user/pool-0">>),0=count(<<"/user/pool-0">>,<<"trap">>),
    T=async(fun()->count(<<"/user/pool-0">>,<<"block">>) end),entered(<<"/user/pool-0">>),{P,T}.

recv(S) ->
    {ok,<<N:32/little>>}=gen_tcp:recv(S,4,5000),{ok,B}=gen_tcp:recv(S,N,5000),
    {ok,Ft,_,Cid,_,K,P,<<>>}=parrot_gw:parse_frame(<<N:32/little,B/binary>>),{Ft,Cid,K,P}.

run_reply_validation() -> fixture(fun reply_validation/0).
reply_validation() ->
    ok=deploy(<<"reply">>,singleton,<<"normal">>),
    rejected_call(<<"/user/reply">>,<<"invalid_reply">>),
    0=count(<<"/user/reply">>,<<"reply_marker">>).

run_root_collision() -> fixture(fun root_collision/0).
root_collision() ->
    Next=erlang:unique_integer([positive,monotonic])+1,
    Existing="/tmp/parrot-beam-host-"++os:getpid()++"-"++integer_to_list(Next),
    ok=file:make_dir(Existing),Sentinel=filename:join(Existing,"previous-owner-data"),
    ok=file:write_file(Sentinel,<<"must survive">>),
    try
        Reply=deploy_result(<<"collision">>,singleton,<<"normal">>),
        Preserved=filelib:is_file(Sentinel),
        io:format("ROOT_OWNERSHIP preserved=~p reply=~p~n",[Preserved,Reply]),
        true=Preserved,{deployed,_}=Reply,
        {unowned,{error,eexist}}=parrot_beam_snapshot:claim(Existing),
        {ok,<<"must survive">>}=file:read_file(Sentinel)
    after file:del_dir_r(Existing) end.
