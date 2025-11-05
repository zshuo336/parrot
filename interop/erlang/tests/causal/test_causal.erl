-module(test_causal).
-export([run/0,run_disabled/0]).

parent(N) -> <<1,N:128,(N+1):64>>.
reject(Fun) ->
    Outcome=try Fun(),returned catch _:_ -> rejected end,
    rejected=Outcome.
check(true,_) -> ok;
check(false,Reason) -> erlang:error(Reason).

run_disabled() ->
    false=code:is_loaded(otel_ctx),P=parent(1),
    P=parrot_trace:scope(P,ask,fun() ->
        undefined=parrot_trace:scope(undefined,tell,fun parrot_trace:current/0),
        P=parrot_trace:current(),P
    end),
    undefined=parrot_trace:current(),
    Big= <<P/binary,(binary:copy(<<7>>,1048576))/binary>>,
    %% Some OTP27 runtimes report bits for heap binaries. Compare against an
    %% independent fixed-size allocation, not a version-specific numeric unit.
    {P,_}=parrot_trace:split(Big),
    Independent=binary:referenced_byte_size(binary:copy(<<0>>,25)),
    true=Independent=:=25 orelse Independent=:=200,
    true=binary:referenced_byte_size(Big)>=1048576,
    Independent=binary:referenced_byte_size(element(1,parrot_trace:split(Big))),
    Independent=binary:referenced_byte_size(parrot_trace:validate(P)),
    io:format("CAUSAL_PARENT_STORAGE logical_bytes=25 independent_measure=~p~n",[Independent]),
    [reject(fun() -> parrot_trace:validate(B) end) || B <- [<<>>,<<0,1:128,2:64>>,<<1,0:128,2:64>>,<<1,1:128,0:64>>,<<P/binary,0>>]],
    io:format("CAUSAL-DISABLED-PASS~n"),ok.

run() ->
    ok=causal_fixture:start_sdk(),
    Tests=[
        {"actual SDK handler records exact parent and child",fun sdk_child/0},
        {"explicit empty context restores SDK and logger state",fun empty_scope/0},
        {"deferred callback captures registration and restores invoking context",fun deferred/0},
        {"handler error ends the SDK span without private exception data",fun panic/0},
        {"target attributes preserve UTF8 and the byte budget",fun target/0},
        {"disabled tracer preserves identity without recording",fun disabled/0},
        {"real TCP ASK TELL and plain requests have isolated handler spans",fun network/0},
        {"real TCP rejects unnegotiated malformed and control trace envelopes",fun network_rejection/0},
        {"real serial component workers retain parents and forward children",fun native_workers/0},
        {"isolated guest records its actual worker callback and retires",fun isolated/0}],
    [begin
        ets:delete_all_objects(parrot_causal_spans),
        try Fun(),io:format("ok ~s~n",[Name])
        catch C:R:S -> io:format("FAIL ~s ~p:~p ~p~n",[Name,C,R,S]),erlang:raise(C,R,S) end
     end || {Name,Fun} <- Tests],
    io:format("CAUSAL-ALL-PASS tests=~p~n",[length(Tests)]),ok.

sdk_child() ->
    P=parent(3),
    <<1,3:128,Child:64>>=parrot_trace:handler(<<"/user/one">>,P,ask,fun parrot_trace:current/0),
    true=Child=/=0 andalso Child=/=4,
    Records=causal_fixture:snapshot(),check(length(Records)=:=1,handler_span_not_exported),
    [#{trace:=3,span:=Child,parent:=4,ended:=Ended,attributes:=Attrs}]=Records,
    check(Ended,handler_span_not_ended),
    <<"Returned">>=maps:get(<<"parrot.outcome">>,Attrs),undefined=parrot_trace:current().

empty_scope() ->
    Old=otel_ctx:get_current(),Metadata=logger:get_process_metadata(),P=parent(5),
    parrot_trace:scope(P,ask,fun() ->
        5=otel_span:trace_id(otel_tracer:current_span_ctx()),
        parrot_trace:scope(undefined,tell,fun() ->
            undefined=parrot_trace:current(),undefined=otel_tracer:current_span_ctx(),
            check(not maps:is_key(otel_trace_id,logger:get_process_metadata()),empty_scope_retained_logger_parent)
        end),
        5=otel_span:trace_id(otel_tracer:current_span_ctx())
    end),
    Old=otel_ctx:get_current(),Metadata=logger:get_process_metadata().

deferred() ->
    P=parent(7),Q=parent(9),
    Fun=parrot_trace:scope(P,ask,fun() -> parrot_trace:capture(fun() ->
        P=parrot_trace:current(),7=otel_span:trace_id(otel_tracer:current_span_ctx()),
        erlang:error(deferred_failure)
    end) end),
    parrot_trace:scope(Q,tell,fun() ->
        reject(Fun),Q=parrot_trace:current(),tell=parrot_trace:operation(),
        9=otel_span:trace_id(otel_tracer:current_span_ctx())
    end),undefined=parrot_trace:current().

panic() ->
    reject(fun() -> parrot_trace:handler(<<"/x">>,parent(11),tell,
        fun() -> erlang:error(<<"private-exception-text">>) end) end),
    [#{ended:=true,status:=Status,attributes:=Attrs}=Span]=causal_fixture:snapshot(),
    <<"Panicked">>=maps:get(<<"parrot.outcome">>,Attrs),check(Status=:=error,panic_status_not_error),
    nomatch=binary:match(term_to_binary(Span),<<"private-exception-text">>),
    undefined=parrot_trace:current(),
    ok=parrot_trace:handler(<<"/x">>,undefined,ask,fun() -> undefined=otel_tracer:current_span_ctx(),ok end),
    1=length(causal_fixture:snapshot()).

target() ->
    Text= <<(binary:copy(<<"a">>,254))/binary,16#1F600/utf8>>,
    ok=parrot_trace:handler(Text,parent(13),ask,fun() -> ok end),
    [#{attributes:=Attrs}]=causal_fixture:snapshot(),
    254=byte_size(maps:get(<<"parrot.target">>,Attrs)),true=maps:get(<<"parrot.target_truncated">>,Attrs),
    ets:delete_all_objects(parrot_causal_spans),
    ok=parrot_trace:handler(<<"prefix",255,"suffix">>,parent(14),ask,fun() -> ok end),
    [#{attributes:=InvalidAttrs}]=causal_fixture:snapshot(),
    <<"prefix">>=maps:get(<<"parrot.target">>,InvalidAttrs),
    true=maps:get(<<"parrot.target_truncated">>,InvalidAttrs).

disabled() ->
    ok=parrot_trace:clear_tracer(),P=parent(15),
    try P=parrot_trace:handler(<<"/x">>,P,tell,fun parrot_trace:current/0),[]=causal_fixture:snapshot()
    after causal_fixture:start_sdk() end.

gateway(Fun,Test) ->
    Parent=self(),{Owner,Mon}=spawn_monitor(fun() ->
        {ok,Port}=parrot_gw:start(0,{127,0,0,1},Fun),Parent!{ready,self(),Port},receive stop -> ok end
    end),
    Port=receive {ready,Owner,P} -> P; {'DOWN',Mon,process,Owner,R} -> erlang:error(R)
         after 5000 -> erlang:error(gateway_start_timeout) end,
    try Test(Port,Owner)
    after
        exit(Owner,kill),receive {'DOWN',Mon,process,Owner,_} -> ok after 5000 -> erlang:error(owner_not_retired) end,
        until(fun() -> whereis(parrot_gw_lease_owner)=:=undefined andalso whereis(parrot_component_runtime)=:=undefined end),
        {error,econnrefused}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}],1000)
    end.

connect(Port,Trace) ->
    {ok,S}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}],1000),
    Caps=case Trace of true -> 8195; false -> 3 end,
    Body= <<1,4:16/little,"peer",4,4:16/little,Caps:32/little,
            5,4:16/little,1048576:32/little,6,1:16/little,0,7,1:16/little,8>>,
    ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,Body)),
    {2,1,_,Ack}=read(S),#{capabilities:=Accepted}=parrot_gw:decode_handshake(Ack,true),
    check(Accepted band 8192=/=0,trace_capability_not_advertised),S.

read(S) ->
    {ok,<<N:32/little>>}=gen_tcp:recv(S,4,3000),{ok,Body}=gen_tcp:recv(S,N,3000),
    {ok,Ft,_,Cid,_,Key,Payload,<<>>}=parrot_gw:parse_frame(<<N:32/little,Body/binary>>),
    {Ft,Cid,Key,Payload}.
send(S,Ft,Cid,Path,Key,Parent,Payload) ->
    {Flags,Body}=case Parent of undefined -> {0,Payload}; _ -> {64,<<Parent/binary,Payload/binary>>} end,
    ok=gen_tcp:send(S,parrot_gw:build_frame(Ft,Flags,Cid,Path,Key,Body)).

network() ->
    Fun=fun(_,B) -> {<<"ok">>,term_to_binary({B,parrot_trace:current(),parrot_trace:operation()})} end,
    gateway(Fun,fun(Port,_) ->
        S=connect(Port,true),
        try
            send(S,16#10,2,<<"/x">>,<<"bin:test">>,parent(17),<<0:32/little,42>>),
            {16#11,2,<<"ok">>,Reply}=read(S),{<<42>>,<<1,17:128,Child:64>>,ask}=binary_to_term(Reply,[safe]),
            true=Child=/=18,
            send(S,16#13,3,<<"/x">>,<<"bin:test">>,parent(19),<<43>>),
            send(S,16#10,4,<<"/x">>,<<"bin:test">>,undefined,<<0:32/little,44>>),
            {16#11,4,_,Plain}=read(S),{<<44>>,undefined,ask}=binary_to_term(Plain,[safe]),
            until(fun() -> length(causal_fixture:snapshot())=:=2 end),
            true=lists:any(fun(#{trace:=T,parent:=P}) -> T=:=19 andalso P=:=20 end,causal_fixture:snapshot())
        after gen_tcp:close(S) end
    end).

network_rejection() ->
    Observer=self(),Fun=fun(_,_) -> Observer!unexpected_dispatch,{<<"ok">>,<<>>} end,
    gateway(Fun,fun(Port,_) ->
        P=parent(21),
        [begin
            S=connect(Port,Negotiated),
            try ok=gen_tcp:send(S,parrot_gw:build_frame(Ft,64,2,<<"/x">>,<<"bin:test">>,Body)),
                {error,closed}=gen_tcp:recv(S,0,2000)
            after gen_tcp:close(S) end
         end || {Negotiated,Ft,Body} <- [
            {false,16#10,<<P/binary,0:32>>},{true,3,P},{true,16#11,P},{true,16#20,P},
            {true,16#10,<<1>>},{true,16#10,<<0,1:128,2:64,0:32>>},
            {true,16#10,<<1,0:128,2:64,0:32>>},{true,16#10,<<1,1:128,0:64,0:32>>}]],
        receive unexpected_dispatch -> erlang:error(rejected_frame_executed) after 0 -> ok end
    end).

native_workers() ->
    gateway(fun parrot_gw:service/3,fun(Port,Owner) ->
        ok=parrot_component_runtime:ensure(Owner),
        [begin {deployed,[Path]}=parrot_component_runtime:deploy(Name,<<"1">>,[Path],fun() -> causal_fixture end,<<>>) end
         || {Name,Path} <- [{<<"first">>,<<"/user/first">>},{<<"next">>,<<"/user/next">>}]],
        S=connect(Port,true),
        try
            send(S,16#10,2,<<"/user/first">>,<<"forward">>,parent(23),<<0:32/little,"/user/next">>),
            {16#11,2,_,B}=read(S),Value=binary_to_term(B,[safe]),
            check(case Value of {<<1,23:128,_:64>>,#{parent:= <<1,23:128,_:64>>}} -> true; _ -> false end,
                  component_parent_not_forwarded),
            {<<1,23:128,First:64>>,#{parent:= <<1,23:128,Next:64>>,worker:=Worker}}=Value,
            true=First=/=Next,true=Worker=/=self(),
            Spans=causal_fixture:snapshot(),2=length(Spans),
            true=lists:any(fun(#{span:=ID,parent:=P}) -> ID=:=Next andalso P=:=First end,Spans),
            send(S,16#10,3,<<"/user/first">>,<<"panic">>,parent(25),<<0:32>>),
            {16#12,3,_,_}=read(S),
            send(S,16#10,4,<<"/user/first">>,<<"inspect">>,undefined,<<0:32>>),
            {16#11,4,_,Plain}=read(S),#{parent:=undefined}=binary_to_term(Plain,[safe]),
            3=length(causal_fixture:snapshot())
        after gen_tcp:close(S) end
    end).

isolated() ->
    Artifact=list_to_binary(os:getenv("PARROT_CAUSAL_ARTIFACT")),
    gateway(fun parrot_gw:service/3,fun(Port,_) ->
        {deployed,[<<"/user/guest">>]}=parrot_gw:admin_deploy({<<"guest">>,<<"1">>,
            {beam,<<"causal_fixture">>,<<"file://",Artifact/binary>>},singleton,<<>>}),
        S=connect(Port,true),
        try
            send(S,16#10,2,<<"/user/guest">>,<<"inspect">>,parent(27),<<0:32>>),
            {16#11,2,_,B}=read(S),Reply=binary_to_term(B,[safe]),
            check(case Reply of #{parent:= <<1,27:128,_:64>>} -> true; _ -> false end,guest_parent_not_forwarded),
            #{parent:= <<1,27:128,Child:64>>,os_pid:=OS}=Reply,
            true=Child=/=28,true=OS=/=list_to_binary(os:getpid()),
            send(S,16#10,3,<<"/user/guest">>,<<"snapshot">>,undefined,<<0:32>>),
            {16#11,3,_,Records}=read(S),[#{trace:=27,span:=Child,parent:=28,ended:=true}]=binary_to_term(Records,[safe]),
            []=causal_fixture:snapshot(),
            [{_,_,_,{owned,{isolated,Host},_}}]=ets:lookup(parrot_admin_components,<<"guest">>),
            #{dir:=Dir,os_pid:=HostOS}=parrot_beam_host:describe(Host),
            OS=list_to_binary(HostOS),check(os_pid_alive(OS),guest_os_process_not_observed_alive),
            stopped=parrot_gw:admin_stop(<<"/user/guest">>),false=is_process_alive(Host),false=filelib:is_dir(Dir),
            0=ets:info(parrot_beam_hosts,size),
            until(fun() -> not os_pid_alive(OS) end),
            io:format("GUEST_OS_RETIRED pid=~s~n",[OS])
        after gen_tcp:close(S) end
    end).

os_pid_alive(Pid) ->
    true=binary_to_integer(Pid)>1,
    Port=open_port({spawn_executable,"/bin/kill"},[binary,exit_status,stderr_to_stdout,
                   {args,["-0",binary_to_list(Pid)]}]),
    probe_status(Port).
probe_status(Port) ->
    receive
        {Port,{data,_}} -> probe_status(Port);
        {Port,{exit_status,0}} -> true;
        {Port,{exit_status,1}} -> false;
        {Port,{exit_status,Code}} -> erlang:error({os_probe_failed,Code})
    after 1000 -> try port_close(Port) catch _:_ -> ok end,erlang:error(os_probe_timeout)
    end.

until(Fun) -> until(Fun,erlang:monotonic_time(millisecond)+5000).
until(Fun,Deadline) ->
    case Fun() of true -> ok; false ->
        case erlang:monotonic_time(millisecond)<Deadline of
            true -> receive after 5 -> ok end,until(Fun,Deadline);
            false -> erlang:error(condition_timeout)
        end
    end.
