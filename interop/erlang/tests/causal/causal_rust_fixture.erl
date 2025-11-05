%% A separate gateway VM and component-owned SDK for the production Rust sender.
-module(causal_rust_fixture).
-export([run/0,service/1]).

run() ->
    Dir=os:getenv("PARROT_ERLANG_CAUSAL_RECEIPT"),
    ok=causal_fixture:start_sdk(),Self=self(),
    {Root,Mon}=spawn_monitor(fun() ->
        {ok,Port}=parrot_gw:start(0),Self!{ready,self(),Port},receive stop -> ok end
    end),
    Port=receive {ready,Root,N} -> N after 5000 -> erlang:error(gateway_start_timeout) end,
    ok=parrot_component_runtime:ensure(Root),
    {deployed,[<<"/user/rust-native">>]}=parrot_component_runtime:deploy(<<"rust-native">>,<<"1">>,
        [<<"/user/rust-native">>],fun() -> causal_fixture end,<<>>),
    Artifact=list_to_binary(os:getenv("PARROT_CAUSAL_ARTIFACT")),
    {deployed,[<<"/user/rust-guest">>]}=parrot_gw:admin_deploy({<<"rust-guest">>,<<"1">>,
        {beam,<<"causal_fixture">>,<<"file://",Artifact/binary>>},singleton,<<>>}),
    [{_,_,_,{owned,{isolated,Host},_}}]=ets:lookup(parrot_admin_components,<<"rust-guest">>),
    #{os_pid:=Guest,dir:=Snapshot}=parrot_beam_host:describe(Host),
    true=alive(Guest),
    publish(Dir,"ready.json",io_lib:format("{\"port\":~p,\"pid\":~s,\"guest_pid\":~s}",[Port,os:getpid(),Guest])),
    until(fun() -> filelib:is_file(filename:join(Dir,"stop")) end,30000),
    stopped=parrot_gw:admin_stop(<<"/user/rust-guest">>),
    stopped=parrot_gw:admin_stop(<<"/user/rust-native">>),
    until(fun() -> not alive(Guest) end,5000),
    false=filelib:is_dir(Snapshot),0=ets:info(parrot_beam_hosts,size),
    Root!stop,receive {'DOWN',Mon,process,Root,normal} -> ok after 5000 -> erlang:error(root_did_not_exit) end,
    until(fun() -> whereis(parrot_component_runtime)=:=undefined andalso whereis(parrot_gw_lease_owner)=:=undefined end,5000),
    {error,econnrefused}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}],1000),
    publish(Dir,"retired.json",<<"{\"guest_exited_before_cleanup\":true,\"snapshot_removed\":true,\"port_closed\":true}">>),
    case os:getenv("PARROT_ERLANG_FIXTURE_EXIT") of false -> halt(0); Exit -> halt(list_to_integer(Exit)) end.

service(<<1>>) ->
    {Trace,Span}=case parrot_trace:current() of
        undefined -> {<<"null">>,<<"null">>};
        <<1,T:128,S:64>> -> {hex(T,32),hex(S,16)}
    end,
    iolist_to_binary(io_lib:format("{\"trace\":~s,\"span\":~s,\"operation\":\"~s\",\"pid\":~s}",
        [Trace,Span,atom_to_list(parrot_trace:operation()),os:getpid()]));
service(<<99>>) -> erlang:error(<<"private-exception-text">>);
service(<<254>>) ->
    Spans=[io_lib:format("{\"trace\":~s,\"span\":~s,\"parent\":~s,\"status\":\"~s\",\"outcome\":\"~s\"}",
        [hex(T,32),hex(S,16),hex(P,16),atom_to_list(Status),maps:get(<<"parrot.outcome">>,Attrs)])
        || #{trace:=T,span:=S,parent:=P,status:=Status,attributes:=Attrs,ended:=true} <- causal_fixture:snapshot()],
    iolist_to_binary(["{\"spans\":[",lists:join(",",Spans),"]}"]).

hex(Number,Width) ->
    Text=string:lowercase(integer_to_list(Number,16)),
    [$",lists:duplicate(Width-length(Text),$0),Text,$"].
publish(Dir,Name,Content) ->
    Temp=filename:join(Dir,Name++".tmp"),
    ok=file:write_file(Temp,Content,[exclusive]),ok=file:rename(Temp,filename:join(Dir,Name)).
alive(Pid) ->
    Port=open_port({spawn_executable,"/bin/kill"},[binary,exit_status,stderr_to_stdout,{args,["-0",Pid]}]),
    probe(Port).
probe(Port) ->
    receive {Port,{data,_}} -> probe(Port); {Port,{exit_status,0}} -> true; {Port,{exit_status,1}} -> false
    after 1000 -> erlang:error(os_probe_timeout) end.
until(Fun,Millis) -> wait(Fun,erlang:monotonic_time(millisecond)+Millis).
wait(Fun,Deadline) ->
    case Fun() of true -> ok; false ->
        case erlang:monotonic_time(millisecond)<Deadline of
            true -> receive after 10 -> ok end,wait(Fun,Deadline);
            false -> erlang:error(condition_timeout)
        end
    end.
