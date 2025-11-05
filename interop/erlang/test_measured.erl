-module(test_measured).
-export([run/0]).

run() ->
    Cases = [{"measured Rust frozen vectors", fun vectors/0},
             {"measured validation and full consumption", fun boundaries/0},
             {"measured RSS source bounds and missing data", fun rss/0},
             {"measured epoch survives listeners and connections", fun identity/0},
             {"measured components and states are distinct", fun components/0}],
    Failed = lists:foldl(fun({Name, F}, N) ->
        try F(), io:format("ok   ~s~n", [Name]), N
        catch C:R:S -> io:format("FAIL ~s: ~p:~p ~p~n", [Name, C, R, S]), N + 1 end
    end, 0, Cases),
    case Failed of 0 -> ok; _ -> halt(1) end.

reject(F) ->
    Result = try {accepted, F()} catch error:_ -> rejected; exit:_ -> rejected end,
    case Result of rejected -> ok; _ -> erlang:error(Result) end.

frozen() ->
    {ok, Json} = file:read_file("../../docs/vectors/measured_metrics.json"),
    {match, Matches} = re:run(Json, <<"\"payload_hex\":\\s*\"([0-9a-f]+)\"">>, [global, {capture, [1], binary}]),
    [Cmd, Reply] = [binary:decode_hex(string:uppercase(Hex)) || [Hex] <- Matches],
    {Cmd, Reply}.

vectors() ->
    {Cmd, Reply} = frozen(),
    Cmd = parrot_gw:encode_admin_cmd_v2({measured_metrics_report, 42}),
    {measured_metrics_report, 42} = parrot_gw:decode_admin_cmd_v2(Cmd),
    {measured_metrics, 42, #{snapshot := Snap, available := 32, epoch := <<"owner-a">>, process_kind := <<>>} = M} =
        parrot_gw:decode_admin_reply_v2(Reply),
    0 = maps:get(asks_rx, Snap),
    0 = maps:get(components, Snap),
    [] = maps:get(component_states, Snap),
    Reply = parrot_gw:encode_admin_reply_v2({measured_metrics, 42, M}).

boundaries() ->
    {Cmd, Reply} = frozen(),
    [begin
        [reject(fun() -> F(binary:part(B, 0, N)) end) || N <- lists:seq(0, byte_size(B) - 1)],
        reject(fun() -> F(<<B/binary, 0>>) end)
    end || {B, F} <- [{Cmd, fun parrot_gw:decode_admin_cmd_v2/1}, {Reply, fun parrot_gw:decode_admin_reply_v2/1}]],
    {measured_metrics, 42, Base} = parrot_gw:decode_admin_reply_v2(Reply),
    [reject(fun() -> parrot_gw:encode_admin_reply_v2({measured_metrics, 42, Base#{K => V}}) end)
     || {K,V} <- [{available,-1}, {available,1 bsl 17}, {available,1 bsl 14},
                 {epoch,<<255>>}, {epoch,binary:copy(<<"x">>,129)},
                 {process_kind,<<255>>}, {process_kind,binary:copy(<<"x">>,129)}]],
    %% The frozen snapshot is 17 bytes. Independent raw suffixes exercise decode too.
    <<Prefix:20/binary, 32, 7, "owner-a", 0>> = Reply,
    [reject(fun() -> parrot_gw:decode_admin_reply_v2(<<Prefix/binary, Suffix/binary>>) end)
     || Suffix <- [<<16#FC,131072:32/little,0,0>>, <<16#FB,16384:16/little,0,0>>,
                   <<32,1,255,0>>, <<32,129,(binary:copy(<<"a">>,129))/binary,0>>]],
    Max = 16#FFFFFFFFFFFFFFFF,
    M = Base#{snapshot => (maps:get(snapshot, Base))#{bytes_rx => Max}},
    {measured_metrics, 42, M} = parrot_gw:decode_admin_reply_v2(parrot_gw:encode_admin_reply_v2({measured_metrics,42,M})).

rss() ->
    Name = filename:join("/tmp", "parrot-rss-" ++ integer_to_list(erlang:unique_integer([positive]))),
    try
        undefined = parrot_gw:read_rss(Name),
        [begin
            ok = file:write_file(Name, Bytes),
            Expected = parrot_gw:read_rss(Name)
        end || {Bytes, Expected} <- [{<<"Name: beam\nVmRSS: 0 kB\n">>,{ok,0}},
            {<<"VmRSS:\t123 kB\n">>,{ok,125952}}, {<<"VmRSS: 123 MB\n">>,undefined},
            {<<"VmRSS: -1 kB\n">>,undefined}, {<<"VmRSS: 1.5 kB\n">>,undefined},
            {<<"VmRSS: 18446744073709551615 kB\n">>,undefined},
            {<<"VmRSS: 2 kB\nVmRSS: 3 kB\n">>,undefined}, {<<"Name: beam\n">>,undefined},
            {<<"VmRSS: 1 kB\n", (binary:copy(<<"x">>,65536))/binary>>,undefined}]],
        ok = file:delete(Name),
        undefined = parrot_gw:read_rss(Name)
    after file:delete(Name) end.

identity() ->
    {ok, P1} = parrot_gw:start(0),
    First = parrot_gw:admin_measured_metrics(),
    #{epoch := Epoch, process_kind := <<"erlang_processes">>, available := Mask, snapshot := Snap} = First,
    true = byte_size(Epoch) > 0 andalso byte_size(Epoch) =< 128,
    true = maps:get(processes, Snap) > 0,
    true = Mask band (1 bsl 14) =/= 0,
    {ok, _P2} = parrot_gw:start(0),
    Parent = self(),
    spawn(fun() -> Parent ! {sample, parrot_gw:admin_measured_metrics()} end),
    receive {sample, #{epoch := Epoch, snapshot := Next}} ->
        true = maps:get(uptime_start_ms, Next) =:= maps:get(uptime_start_ms, Snap)
    after 1000 -> erlang:error(sample_timeout) end,
    Body = iolist_to_binary(parrot_gw:handshake_body("generic-client")),
    #{capabilities := HelperCaps} = parrot_gw:decode_handshake(Body, false),
    0 = HelperCaps band 1024,
    {ok, S} = gen_tcp:connect("127.0.0.1", P1, [binary,{active,false}],1000),
    try
        ok = gen_tcp:send(S, parrot_gw:build_frame(1,1,<<>>,<<>>,Body)),
        Ack = receive_frame(S, <<>>),
        #{capabilities := Caps} = parrot_gw:decode_handshake(Ack, true),
        1024 = Caps band 1024
    after gen_tcp:close(S) end.

receive_frame(S, Buf) ->
    case parrot_gw:parse_frame(Buf) of
        {ok,2,_,_,_,_,Body,<<>>} -> Body;
        {more,_} -> {ok,B} = gen_tcp:recv(S,0,1000), receive_frame(S,<<Buf/binary,B/binary>>)
    end.

components() ->
    parrot_gw:admin_init(),
    Before = parrot_gw:admin_measured_metrics(),
    ets:insert(parrot_admin_components, {<<"metrics-test">>,<<"v1">>,[<<"/one">>,<<"/two">>],test_measured}),
    try
        #{snapshot := After} = parrot_gw:admin_measured_metrics(),
        Snap = maps:get(snapshot, Before),
        true = maps:get(components, After) =:= maps:get(components, Snap) + 1,
        true = length(maps:get(component_states, After)) =:= length(maps:get(component_states, Snap)) + 2
    after ets:delete(parrot_admin_components, <<"metrics-test">>) end.
