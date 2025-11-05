%% Wire 1.0 对齐测试（DEV_03 §7：golden vectors eunit 断言）。
%%
%% 运行（repo 根）：
%%   make check-erl  % strict compilation followed by gateway contracts
-module(test_wire).
-export([run/0, all/0, parrot_service/2]).

-define(ASK, 16#10).
-define(REPLY, 16#11).
-define(REPLY_ERR, 16#12).

all() ->
    [{"rejection assertion distinguishes return and exception", fun t_rejection_assertion_contract/0},
     {"NUL identity encoders", fun t_identity_encoder/0},
     {"NUL identity decoders", fun t_identity_decoder/0},
     {"NUL identity real listener", fun t_identity_listener/0},
     {"identity exact UTF8 limit", fun t_identity_limit/0},
     {"all shared wire golden vectors", fun t_ask_golden/0},
     {"full header legacy defaults", fun t_header_defaults/0},
     {"full header independent bytes", fun t_header_bytes/0},
     {"full header hop boundaries", fun t_header_hops/0},
     {"full header sequence boundaries", fun t_header_sequence/0},
     {"full header shared split points", fun t_header_splits/0},
     {"full header shared pipeline", fun t_header_pipeline/0},
     {"full header malformed input parity", fun t_header_rejection/0},
     {"reply-err golden", fun t_reply_err_golden/0},
     {"partial frame", fun t_partial/0},
     {"pipeline two frames", fun t_pipeline/0},
     {"handshake tlv layout", fun t_handshake/0},
     {"service dialects", fun t_service/0},
     {"err payload layout", fun t_err/0},
     {"DEV_08 slow ask not blocking heartbeat", fun t_slow_ask_concurrency/0},
     {"DEV_08 slow ask not blocking fast ask", fun t_slow_fast_ask/0},
     {"outbound boundaries", fun t_outbound_boundaries/0},
     {"all split points and concatenated frames", fun t_all_splits/0},
     {"actual listener receive limit", fun t_listener_receive_limit/0},
     {"reject traffic before handshake", fun t_reject_before_handshake/0},
     {"reject malformed handshakes", fun t_invalid_handshake/0},
     {"reject repeated handshake", fun t_repeated_handshake/0},
     {"negotiate bin-only peer", fun t_bin_only_negotiation/0},
     {"strict handshake TLV contract", fun t_handshake_contract/0},
     {"killed driver retires its connection lease", fun t_killed_driver/0},
     {"minimum negotiated limit starts after successful ACK", fun t_minimum_ack_limit/0},
     {"wire counters saturate under concurrency", fun t_counter_saturation/0},
     {"handshake deadline includes partial input", fun t_handshake_deadline/0},
     {"ASK work retires with its connection", fun() -> t_work_retirement(?ASK) end},
     {"TELL work retires with its connection", fun() -> t_work_retirement(16#13) end},
     {"killed connection retires its OTP task supervisor", fun t_killed_work_tree/0},
     {"direct socket retires with its parent connection", fun t_direct_retirement/0},
     {"ASK capacity rejects overload and preserves heartbeat", fun() -> t_work_capacity(?ASK) end},
     {"TELL capacity closes and retires admitted work", fun() -> t_work_capacity(16#13) end},
     {"direct admission bounds and deduplicates pending dials", fun t_direct_capacity/0},
     {"nested direct supervisors retire transitively", fun t_nested_direct/0},
     {"gateway connection admission is bounded", fun t_connection_capacity/0},
     {"acceptor death retires an unfinished handoff", fun t_acceptor_handoff_death/0}] ++ delivery_cases() ++ boundary_cases() ++ error_payload_cases().

run() ->
    Fail = lists:foldl(
        fun ({Name, F}, Acc) ->
            try F(), io:format("ok   ~s~n", [Name]), Acc
            catch _:R -> io:format("FAIL ~s: ~p~n", [Name, R]), Acc + 1
            end
        end, 0, all()),
    case Fail of
        0 -> io:format("~nEUNIT-ALL-PASS~n");
        N -> io:format("~n~p FAILED~n", [N]), halt(1)
    end.

%% Consume the same JSON bytes and independent field expectations as other stacks.
t_ask_golden() ->
    Rows=parrot_test_vectors:wire(),
    [begin
        #{<<"bytes">>:=Bytes,<<"ft">>:=Ft,<<"flags">>:=Flags,<<"cid">>:=Cid,
          <<"path">>:=Path,<<"key">>:=Key,<<"payload">>:=Payload,
          <<"hop_count">>:=HopCount,<<"hop_limit">>:=HopLimit,<<"sequence">>:=Sequence}=Row,
        Bytes=parrot_gw:build_frame(Ft,Flags,Cid,HopCount,HopLimit,Sequence,Path,Key,Payload),
        {ok,Ft,Flags,Cid,Path,Key,Payload,<<>>}=parrot_gw:parse_frame(Bytes),
        Expected=wire_expected(Row),
        {ok,Expected,<<>>}=parrot_gw:parse_frame_full(Bytes),
        BodySize=byte_size(Bytes)-4,
        <<BodySize:32/little,1,Ft,Flags:16/little,Cid:64/little,
          HopCount,HopLimit,Sequence:32/little,0:16/little,_/binary>>=Bytes
     end || Row <- Rows],
    Names=[maps:get(<<"name">>,Row) || Row <- Rows],
    true=lists:all(fun(N) -> lists:member(N,Names) end,
        [<<"ask-basic">>,<<"tell-basic">>,<<"reply-err-stopped">>,<<"heartbeat">>]),
    io:format("SHARED_WIRE_JSON_OK vectors=~p~n",[length(Rows)]).

wire_expected(Row) ->
    maps:from_list([{K,maps:get(atom_to_binary(K,utf8),Row)} || K <-
        [ft,flags,cid,hop_count,hop_limit,sequence,path,key,payload]]).

t_header_defaults() ->
    [begin
        Bytes=parrot_gw:build_frame(Ft,Flags,Cid,Path,Key,Payload),
        Bytes=parrot_gw:build_frame(Ft,Flags,Cid,0,8,0,Path,Key,Payload),
        {ok,#{hop_count:=0,hop_limit:=8,sequence:=0},<<>>}=parrot_gw:parse_frame_full(Bytes),
        {ok,Ft,Flags,Cid,Path,Key,Payload,<<>>}=parrot_gw:parse_frame(Bytes),
        case Flags of
            0 -> Bytes=parrot_gw:build_frame(Ft,Cid,Path,Key,Payload);
            _ -> ok
        end
    end || {Ft,Flags,Cid,Path,Key,Payload} <-
        [{?ASK,0,1,<<"/x">>,<<"bin:t::M">>,<<0,0,0,0,171>>},
         {?REPLY,65535,16#FFFFFFFFFFFFFFFF,<<>>,<<>>,<<0,255>>}]],
    ok.

%% Independently specified bytes, with every nondefault header field and UTF8.
header_sample() ->
    {hex_to_bin("3800000001110a80ffffffffffffffff0109ffffffff000012000000"
                "efbbbfe99baa225c0a7b7061796c6f61647d0700000070623a7465737400ff80"),
     #{ft=>?REPLY,flags=>16#800A,cid=>16#FFFFFFFFFFFFFFFF,
       hop_count=>1,hop_limit=>9,sequence=>16#FFFFFFFF,
       path=><<239,187,191,233,155,170,34,92,10,"{payload}">>,
       key=><<"pb:test">>,payload=><<0,255,128>>}}.

t_header_bytes() ->
    {Bytes,Expected}=header_sample(),
    Bytes=parrot_gw:build_frame(?REPLY,16#800A,16#FFFFFFFFFFFFFFFF,
                              1,9,16#FFFFFFFF,maps:get(path,Expected),<<"pb:test">>,<<0,255,128>>),
    {ok,Expected,<<>>}=parrot_gw:parse_frame_full(Bytes),
    {ok,?REPLY,16#800A,16#FFFFFFFFFFFFFFFF,Path,<<"pb:test">>,<<0,255,128>>,<<>>}=
        parrot_gw:parse_frame(Bytes),
    Path=maps:get(path,Expected).

t_header_hops() ->
    [begin
        Bytes=parrot_gw:build_frame(?ASK,0,1,Count,Limit,0,<<>>,<<>>,<<>>),
        <<28:32/little,1,?ASK,0:16/little,1:64/little,Count,Limit,0:48,0:64>>=Bytes,
        {ok,#{hop_count:=Count,hop_limit:=Limit},<<>>}=parrot_gw:parse_frame_full(Bytes)
    end || Limit <- lists:seq(1,255), Count <- lists:usort([0,Limit-1])],
    [begin
        {exception,error,{invalid_wire_frame,invalid_hop}}=assertion_outcome(fun() ->
            parrot_gw:build_frame(?ASK,0,1,Count,Limit,0,<<>>,<<>>,<<>>) end)
    end || {Count,Limit} <- [{-1,8},{8,8},{9,8},{0,0},{0,-1},{0,256},
                            {255,255},{256,255},{1.0,8},{0,8.0},{true,8},{0,false}]],
    ok.

t_header_sequence() ->
    [begin
        Bytes=parrot_gw:build_frame(?ASK,0,1,0,8,Seq,<<>>,<<>>,<<>>),
        <<28:32/little,1,?ASK,0:16/little,1:64/little,0,8,Seq:32/little,0:16,0:64>>=Bytes,
        {ok,#{sequence:=Seq},<<>>}=parrot_gw:parse_frame_full(Bytes)
    end || Seq <- [0,1,16#7FFFFFFF,16#80000000,16#FFFFFFFF]],
    [begin
        {exception,error,{invalid_wire_frame,bad_sequence}}=assertion_outcome(fun() ->
            parrot_gw:build_frame(?ASK,0,1,0,8,Seq,<<>>,<<>>,<<>>) end)
    end || Seq <- [-1,16#100000000,1.0,true,<<0>>]],
    ok.

header_rows() ->
    [{maps:get(<<"bytes">>,Row),wire_expected(Row)} || Row <- parrot_test_vectors:wire()] ++
        [header_sample()].

t_header_splits() ->
    [begin
        <<Prefix:N/binary,Suffix/binary>>=Bytes,
        {more,Prefix}=parrot_gw:parse_frame_full(Prefix),
        {more,Prefix}=parrot_gw:parse_frame(Prefix),
        {ok,Expected,Bytes}=parrot_gw:parse_frame_full(<<Prefix/binary,Suffix/binary,Bytes/binary>>)
    end || {Bytes,Expected} <- header_rows(), N <- lists:seq(0,byte_size(Bytes)-1)],
    ok.

t_header_pipeline() ->
    Rows=header_rows(),
    <<>>=lists:foldl(fun({_,Expected},Remaining) ->
        {ok,Expected,Tail}=parrot_gw:parse_frame_full(Remaining),Tail
    end,iolist_to_binary([Bytes || {Bytes,_} <- Rows]),Rows),
    ok.

t_header_rejection() ->
    {ok,Corpus}=file:read_file("../../docs/vectors/wire1-boundaries.tsv"),
    [begin
        Bytes=hex_to_bin(Hex),
        {more,Bytes}=parrot_gw:parse_frame_full(Bytes)
    end || Line <- binary:split(Corpus,<<"\n">>,[global]),
        [_,<<"more">>,Hex] <- [binary:split(Line,<<"\t">>,[global])]],
    [begin
        Bytes=hex_to_bin(Hex),
        {exception,error,_}=assertion_outcome(fun() -> parrot_gw:parse_frame_full(Bytes) end),
        {Sample,_}=header_sample(),
        {exception,error,_}=assertion_outcome(fun() ->
            parrot_gw:parse_frame_full(<<Bytes/binary,Sample/binary>>) end)
    end || Line <- binary:split(Corpus,<<"\n">>,[global]),
        [_,<<"reject">>,Hex] <- [binary:split(Line,<<"\t">>,[global])]],
    ok.

%% golden 语义: reply-err stopped（code=3 结构体往返）
t_reply_err_golden() ->
    [#{<<"bytes">>:=Frame,<<"payload">>:=Payload}]=[Row || Row <- parrot_test_vectors:wire(),
        maps:get(<<"name">>,Row)=:= <<"reply-err-stopped">>],
    Frame=parrot_gw:build_frame(?REPLY_ERR,0,7,<<>>,<<>>,parrot_gw:err_payload(3,"actor stopped")),
    {ok,?REPLY_ERR,0,7,<<>>,<<>>,Payload,<<>>}=parrot_gw:parse_frame(Frame),
    <<3:16/little,0:16/little,"actor stopped">>=Payload.

t_partial() ->
    Full = parrot_gw:build_frame(?ASK, 0, 5, <<"/a">>, <<"k">>, <<1, 2, 3>>),
    {more, _} = parrot_gw:parse_frame(binary:part(Full, 0, byte_size(Full) - 1)),
    {ok, ?ASK, _, 5, <<"/a">>, <<"k">>, <<1, 2, 3>>, <<>>} = parrot_gw:parse_frame(Full).

t_pipeline() ->
    F1 = parrot_gw:build_frame(?ASK, 0, 1, <<>>, <<"a">>, <<>>),
    F2 = parrot_gw:build_frame(?ASK, 0, 2, <<>>, <<"b">>, <<>>),
    {ok, ?ASK, _, 1, <<>>, <<"a">>, <<>>, Rest} = parrot_gw:parse_frame(<<F1/binary, F2/binary>>),
    {ok, ?ASK, _, 2, <<>>, <<"b">>, <<>>, <<>>} = parrot_gw:parse_frame(Rest).

t_handshake() ->
    Body = iolist_to_binary(parrot_gw:handshake_body("erl-test")),
    <<1:8, 8:16/little, "erl-test", _/binary>> = Body,
    Ack = iolist_to_binary(parrot_gw:handshake_ack_body("erl-test")),
    TailLen = byte_size(Ack),
    Tail = binary:part(Ack, TailLen - 5, 5),
    %% chosen_codec 用 tag 9（RB1：tag 8 已被 DIRECT_ADDR 占用——
    %% tag 分配表见 parrot-remote/src/handshake.rs 头注）
    <<9:8, 2:16/little, "pb">> = Tail,
    %% caps=bin|pb（bit0|bit1——crawler-lab 起放宽为双栈）
    <<1:8, 8:16/little, "erl-test", 4:8, 4:16/little, 3:32/little, _/binary>> = Body.

t_service() ->
    {<<"bin:u:Pong">>, <<103:64/little>>} =
        parrot_gw:service(<<"bin:u:Ping">>, <<100:64/little>>),
    {<<"bin:u:AddR">>, <<10007:64/little>>} =
        parrot_gw:service(<<"bin:u:Add">>, <<3:64/little, 4:64/little>>),
    %% 未知 key 抛错（REPLY_ERR 路径）
    try parrot_gw:service(<<"nope">>, <<>>), throw(should_fail)
    catch error:{unknown_service, <<"nope">>} -> ok end.

t_err() ->
    <<13:16/little, 0:16/little, "forbidden">> = parrot_gw:err_payload(13, "forbidden").

%% ---------- DEV_08 并发模型（真实网关进程 + socket） ----------

t_slow_ask_concurrency() ->
    with_held_ping(100, fun(S, Worker, Ref) ->
        Hb = parrot_gw:build_frame(16#03, 101, <<>>, <<>>, <<>>),
        ok = gen_tcp:send(S, Hb),
        {16#04, 0, 101, <<>>, <<>>, <<>>} = gw_recv_frame(S),
        true = is_process_alive(Worker),
        Worker ! {release_ping, Ref},
        {?REPLY, 0, 100, <<>>, <<"bin:u:Pong">>, <<45:64/little>>} = gw_recv_frame(S)
    end).

t_slow_fast_ask() ->
    with_held_ping(200, fun(S, Worker, Ref) ->
        Fast = parrot_gw:build_frame(?ASK, 201, <<"/u">>, <<"bin:u:Add">>,
                                   <<3:32/little, "a/l", 3:64/little, 4:64/little>>),
        ok = gen_tcp:send(S, Fast),
        {?REPLY, 0, 201, <<>>, <<"bin:u:AddR">>, <<10007:64/little>>} = gw_recv_frame(S),
        true = is_process_alive(Worker),
        Worker ! {release_ping, Ref},
        {?REPLY, 0, 200, <<>>, <<"bin:u:Pong">>, <<45:64/little>>} = gw_recv_frame(S)
    end).

with_held_ping(Cid, Check) ->
    Observer = self(), Ref = make_ref(),
    Slow = fun(<<"bin:u:Ping">> = K, P) ->
                   Observer ! {ping_entered, Ref, self()},
                   receive {release_ping, Ref} -> parrot_gw:service(K, P)
                   after 10000 -> error(test_ping_gate_timeout) end;
              (K, P) -> parrot_gw:service(K, P)
           end,
    {ok, Port} = parrot_gw:start(0, Slow),
    {ok, S} = gw_connect(Port),
    try
        Ask = parrot_gw:build_frame(?ASK, Cid, <<"/user/x">>, <<"bin:u:Ping">>,
                                   <<3:32/little, "a/l", 42:64/little>>),
        ok = gen_tcp:send(S, Ask),
        Worker = receive {ping_entered, Ref, Pid} -> Pid
                 after 5000 -> error(test_ping_not_entered) end,
        try Check(S, Worker, Ref)
        after Worker ! {release_ping, Ref} end
    after gen_tcp:close(S) end.

gw_recv_frame(S) -> gw_recv_frame(S, saved_tail(S)).
gw_recv_frame(S, Buf) ->
    case parrot_gw:parse_frame(Buf) of
        {ok, Ft, Flags, Cid, Path, Key, Payload, Tail} ->
            put({wire_tail, S}, Tail), {Ft, Flags, Cid, Path, Key, Payload};
        {more, _} ->
            {ok, Data} = gen_tcp:recv(S, 0, 5000),
            gw_recv_frame(S, <<Buf/binary, Data/binary>>)
    end.

gw_connect(Port) ->
    {ok, S} = gen_tcp:connect("127.0.0.1", Port, [binary, {packet, raw},
                                                   {active, false}, {nodelay, true}]),
    Body = iolist_to_binary(parrot_gw:handshake_body("test-peer")),
    Hs = parrot_gw:build_frame(16#01, 1, <<>>, <<"__handshake__">>, Body),
    ok = gen_tcp:send(S, Hs),
    {16#02, _} = gw_recv_ft(S),
    {ok, S}.

gw_recv_ft(S) -> gw_recv_loop(S, fun(Ft, _Cid) -> {Ft, none} end, saved_tail(S)).
gw_recv_ft_cid(S) -> gw_recv_loop(S, fun(Ft, Cid) -> {Ft, Cid} end, saved_tail(S)).

saved_tail(S) -> case erase({wire_tail, S}) of undefined -> <<>>; B -> B end.

gw_recv_loop(S, Pick, Buf) ->
    case parrot_gw:parse_frame(Buf) of
        {ok, Ft, _Fl, Cid, _P, _K, _Pay, Tail} ->
            case Pick(Ft, Cid) of
                skip -> gw_recv_loop(S, Pick, Tail);
                R -> put({wire_tail, S}, Tail), R
            end;
        {more, _} ->
            {ok, D} = gen_tcp:recv(S, 0, 5000),
            gw_recv_loop(S, Pick, <<Buf/binary, D/binary>>)
    end.

%% ---------- util ----------

hex_to_bin(Hex) -> hex_to_bin(unicode:characters_to_list(Hex), []).

hex_to_bin([], Acc) -> list_to_binary(lists:reverse(Acc));
hex_to_bin([A, B | Rest], Acc) ->
    hex_to_bin(Rest, [list_to_integer([A, B], 16) | Acc]).

boundary_cases() ->
    {ok, Corpus} = file:read_file("../../docs/vectors/wire1-boundaries.tsv"),
    [{"shared boundary " ++ binary_to_list(Name), fun() ->
        Bytes = hex_to_bin(binary_to_list(Hex)),
        case Expected of
            <<"reject">> ->
                assert_rejected(fun() -> parrot_gw:parse_frame(Bytes) end),
                Valid = parrot_gw:build_frame(?ASK, 1, <<>>, <<>>, <<>>),
                assert_rejected(fun() -> parrot_gw:parse_frame(<<Bytes/binary, Valid/binary>>) end);
            <<"more">> -> {more, Bytes} = parrot_gw:parse_frame(Bytes);
            <<"frame">> -> {ok, _, _, _, _, _, _, <<>>} = parrot_gw:parse_frame(Bytes)
        end
    end} || Line <- binary:split(Corpus, <<"\n">>, [global]),
        Line =/= <<>>, binary:at(Line, 0) =/= $#,
        [Name, Expected, Hex] <- [binary:split(Line, <<"\t">>, [global])]].

error_payload_cases() ->
    {ok, Corpus} = file:read_file("../../docs/vectors/error-payloads.tsv"),
    Accept = [{"shared error encoder " ++ binary_to_list(Name), fun() ->
        Expected = hex_to_bin(PayloadHex),
        Expected = parrot_gw:err_payload(binary_to_integer(Code), hex_to_bin(DetailHex))
    end} || Line <- binary:split(Corpus, <<"\n">>, [global]),
        Line =/= <<>>, binary:at(Line, 0) =/= $#,
        [Name, <<"accept">>, PayloadHex, Code, DetailHex] <- [binary:split(Line, <<"\t">>, [global])]],
    Codes = [{"invalid error code " ++ lists:flatten(io_lib:format("~p", [Code])), fun() ->
        assert_rejected(fun() -> parrot_gw:err_payload(Code, <<>>) end)
    end} || Code <- [0, 14, -1, 65536, 1.5, true]],
    Details = [{"invalid error detail " ++ binary_to_list(Name), fun() ->
        <<_:4/binary, Detail/binary>> = hex_to_bin(PayloadHex),
        assert_rejected(fun() -> parrot_gw:err_payload(3, Detail) end)
    end} || Line <- binary:split(Corpus, <<"\n">>, [global]),
        Line =/= <<>>, binary:at(Line, 0) =/= $#,
        [Name, <<"reject">>, PayloadHex, _, _] <- [binary:split(Line, <<"\t">>, [global])],
        lists:member(Name, [<<"continuation">>, <<"overlong">>, <<"surrogate">>,
                            <<"above-unicode">>, <<"truncated">>])],
    Accept ++ Codes ++ Details.

assert_rejected(F) ->
    try F() of
        Value -> erlang:error({malformed_accepted, Value})
    catch
        error:_ -> ok;
        exit:_ -> ok
    end.

%% A returned or thrown EXIT-shaped term is not an error/exit exception.
t_rejection_assertion_contract() ->
    ok = assert_rejected(fun() -> erlang:error(badarg) end),
    ok = assert_rejected(fun() -> exit(rejected) end),
    [begin
        {exception, error, {malformed_accepted, Value}} = assertion_outcome(
            fun() -> assert_rejected(fun() -> Value end) end)
    end || Value <- [ok, false, {'EXIT', normal_return}, {error, badarg}]],
    [begin
        {exception, throw, Value} = assertion_outcome(
            fun() -> assert_rejected(fun() -> throw(Value) end) end)
    end || Value <- [rejected, {'EXIT', thrown_term}]],
    ok.

assertion_outcome(F) ->
    try F() of
        Value -> {returned, Value}
    catch
        Class:Reason -> {exception, Class, Reason}
    end.

t_outbound_boundaries() ->
    [assert_rejected(fun() -> parrot_gw:build_frame(Ft, Flags, Cid, Path, Key, Payload) end)
      || {Ft, Flags, Cid, Path, Key, Payload} <-
        [{0, 0, 1, <<>>, <<>>, <<>>},
         {?ASK, -1, 1, <<>>, <<>>, <<>>},
         {?ASK, 65536, 1, <<>>, <<>>, <<>>},
         {?ASK, 0, -1, <<>>, <<>>, <<>>},
         {?ASK, 0, 16#10000000000000000, <<>>, <<>>, <<>>},
         {?ASK, 0, 1, <<255>>, <<>>, <<>>},
         {?ASK, 0, 1, <<>>, <<255>>, <<>>},
         {?ASK, 0, 1, <<>>, <<>>, binary:copy(<<0>>, 16777216)}]],
    ok.

t_listener_receive_limit() ->
    {ok, Port} = parrot_gw:start(0),
    {ok, Pending} = gen_tcp:connect("127.0.0.1", Port, [binary, {active, false}], 5000),
    try
        %% Before negotiation the global bound still rejects from four bytes.
        ok = gen_tcp:send(Pending, <<16777217:32/little>>),
        {error, closed} = gen_tcp:recv(Pending, 0, 1000)
    after gen_tcp:close(Pending) end,
    {ok, Sock} = gw_connect(Port),
    try
        %% The advertised 1 MiB bound applies after completing handshake.
        ok = gen_tcp:send(Sock, <<1048577:32/little>>),
        {error, closed} = gen_tcp:recv(Sock, 0, 1000)
    after gen_tcp:close(Sock) end.

t_all_splits() ->
    Path = unicode:characters_to_binary([47, 16#76ee, 16#1f642]),
    Full = parrot_gw:build_frame(?ASK, 3, Path, <<"bin:key">>, <<0, 255, 42>>),
    Next = parrot_gw:build_frame(?REPLY, 4, <<>>, <<>>, <<1>>),
    [begin
        <<Prefix:N/binary, Suffix/binary>> = Full,
        {more, Prefix} = parrot_gw:parse_frame(Prefix),
        {ok, ?ASK, _, 3, Path, <<"bin:key">>, <<0, 255, 42>>, Next} =
            parrot_gw:parse_frame(<<Prefix/binary, Suffix/binary, Next/binary>>)
    end || N <- lists:seq(0, byte_size(Full) - 1)],
    ok.

t_reject_before_handshake() ->
    {ok, Port} = parrot_gw:start(0),
    {ok, S} = gen_tcp:connect("127.0.0.1", Port, [binary, {active, false}], 5000),
    try
        ok = gen_tcp:send(S, parrot_gw:build_frame(?ASK, 123, <<>>, <<"bin:u:Ping">>, <<0:32/little, 1:64/little>>)),
        {error, closed} = gen_tcp:recv(S, 0, 1000)
    after gen_tcp:close(S) end.

t_invalid_handshake() ->
    {ok, Port} = parrot_gw:start(0),
    Valid = iolist_to_binary(parrot_gw:handshake_body("peer")),
    Bodies = [<<>>, binary:part(Valid, 0, byte_size(Valid) - 1),
              <<Valid/binary, 1, 1:16/little, "x">>],
    [begin
        {ok, S} = gen_tcp:connect("127.0.0.1", Port, [binary, {active, false}], 5000),
        try
            ok = gen_tcp:send(S, parrot_gw:build_frame(1, 1, <<>>, <<>>, Body)),
            {error, closed} = gen_tcp:recv(S, 0, 1000)
        after gen_tcp:close(S) end
    end || Body <- Bodies].

t_repeated_handshake() ->
    {ok, Port} = parrot_gw:start(0),
    {ok, S} = gw_connect(Port),
    try
        Body = iolist_to_binary(parrot_gw:handshake_body("peer")),
        ok = gen_tcp:send(S, parrot_gw:build_frame(1, 2, <<>>, <<>>, Body)),
        {error, closed} = gen_tcp:recv(S, 0, 1000)
    after gen_tcp:close(S) end.

t_bin_only_negotiation() ->
    {ok, Port} = parrot_gw:start(0),
    {ok, S} = gen_tcp:connect("127.0.0.1", Port, [binary, {active, false}], 5000),
    try
        Body = <<1, 4:16/little, "peer", 4, 4:16/little, 1:32/little,
                 5, 4:16/little, 1048576:32/little, 6, 1:16/little, 0>>,
        ok = gen_tcp:send(S, parrot_gw:build_frame(1, 1, <<>>, <<>>, Body)),
        {ok, Bytes} = gen_tcp:recv(S, 0, 1000),
        {ok, 2, _, _, _, _, Ack, <<>>} = parrot_gw:parse_frame(Bytes),
        true = binary:match(Ack, <<9, 3:16/little, "bin">>) =/= nomatch,
        nomatch = binary:match(Ack, <<9, 2:16/little, "pb">>)
    after gen_tcp:close(S) end.

hs_fields() ->
    [{1, <<"peer">>}, {4, <<3:32/little>>}, {5, <<1048576:32/little>>},
     {6, <<0>>}, {7, <<8>>}].

hs_pack(Fields) ->
    iolist_to_binary([<<Tag, (byte_size(V)):16/little, V/binary>> || {Tag, V} <- Fields]).

t_handshake_contract() ->
    Fields = hs_fields(),
    Valid = hs_pack(Fields),
    #{node := <<"peer">>, codec := <<"bin">>} = parrot_gw:decode_handshake(Valid, false),
    %% Optional hop and unknown extension tags remain forward compatible.
    #{codec := <<"bin">>} = parrot_gw:decode_handshake(hs_pack(lists:keydelete(7, 1, Fields)), true),
    #{node := <<"peer">>} = parrot_gw:decode_handshake(<<Valid/binary, 99, 2:16/little, 0, 255>>, false),
    #{codec := <<"pb">>} = parrot_gw:decode_handshake(hs_pack(lists:keyreplace(4, 1, Fields, {4, <<2:32/little>>})), false),
    [assert_rejected(fun() -> parrot_gw:decode_handshake(hs_pack(lists:keydelete(Tag, 1, Fields)), false) end)
        || Tag <- [1, 4, 5, 6]],
    [assert_rejected(fun() -> parrot_gw:decode_handshake(hs_pack(lists:keystore(Tag, 1, Fields, {Tag, Value})), false) end)
        || {Tag, Value} <- [{1, <<>>}, {1, <<255>>}, {2, <<255>>}, {3, <<255>>}, {8, <<255>>},
            {4, <<>>}, {4, <<0:32/little>>}, {5, <<27:32/little>>}, {6, <<4>>},
            {6, <<0, 0>>}, {7, <<0>>}, {7, <<8, 8>>}, {9, <<"bin">>}]],
    [assert_rejected(fun() -> parrot_gw:decode_handshake(hs_pack(Fields ++ [{Tag, <<>>}]), false) end)
        || Tag <- [1, 4, 5, 6, 7]],
    [assert_rejected(fun() -> parrot_gw:decode_handshake(<<Valid/binary, Extra/binary>>, true) end)
        || Extra <- [<<9, 3:16/little, "wat">>, <<9, 0:16/little>>, <<99>>, <<99, 2:16/little, 0>>]],
    PbOnly = hs_pack(lists:keyreplace(4, 1, Fields, {4, <<2:32/little>>})),
    assert_rejected(fun() -> parrot_gw:decode_handshake(PbOnly, true) end),
    #{codec := <<"pb">>} = parrot_gw:decode_handshake(<<PbOnly/binary, 9, 2:16/little, "pb">>, true).

metric(Key) -> case ets:lookup(parrot_gw_metrics, Key) of [] -> 0; [{_, N}] -> N end.

eventually(F) -> eventually(F, erlang:monotonic_time(millisecond) + 3000).
eventually(F, Until) ->
    case F() of
        true -> ok;
        false ->
            case erlang:monotonic_time(millisecond) < Until of
                true -> receive after 5 -> ok end, eventually(F, Until);
                false -> erlang:error({condition_deadline,
                    ets:tab2list(parrot_gw_metrics),
                    process_info(whereis(parrot_gw_lease_owner), [monitors, messages, current_function])})
            end
    end.

quiet_leases() ->
    eventually(fun() ->
        {monitors, Ms} = process_info(whereis(parrot_gw_lease_owner), monitors),
        length(Ms) =:= 1 andalso metric(connections) =:= 0
    end).

peername_or_closed(P) ->
    try inet:peername(P)
    catch
        error:_ -> {error, closed};
        exit:_ -> {error, closed}
    end.

accepted_driver(S) ->
    {ok, Endpoint} = inet:sockname(S),
    {ok, Peer} = inet:peername(S),
    Matches = [P || P <- erlang:ports(),
        erlang:port_info(P,name)=:={name,"tcp_inet"}, peername_or_closed(P) =:= {ok, Endpoint},
        (try inet:sockname(P) catch _:_ -> error end) =:= {ok, Peer}],
    case Matches of
        [P] -> {connected, Pid} = erlang:port_info(P, connected), Pid;
        _ -> undefined
    end.

t_killed_driver() ->
    {ok, Port} = parrot_gw:start(0),
    quiet_leases(),
    Failed = metric(hs_failed),
    {ok, Pending} = gen_tcp:connect("127.0.0.1", Port, [binary, {active, false}], 1000),
    try
        eventually(fun() ->
            case accepted_driver(Pending) of
                undefined -> false;
                Pid -> case process_info(Pid, monitored_by) of
                    {monitored_by, Ms} -> lists:member(whereis(parrot_gw_lease_owner), Ms);
                    _ -> false
                end
            end
        end),
        0 = metric(connections),
        exit(accepted_driver(Pending), kill),
        {error, closed} = gen_tcp:recv(Pending, 0, 1000),
        quiet_leases(),
        true = metric(hs_failed) =:= Failed + 1
    after gen_tcp:close(Pending) end,
    {ok, Active} = gw_connect(Port),
    try
        eventually(fun() -> metric(connections) =:= 1 end),
        Success = metric(hs_ok),
        exit(accepted_driver(Active), kill),
        {error, closed} = gen_tcp:recv(Active, 0, 1000),
        quiet_leases(),
        Success = metric(hs_ok),
        true = metric(hs_failed) =:= Failed + 1
    after gen_tcp:close(Active) end.

t_minimum_ack_limit() ->
    {ok, Port} = parrot_gw:start(0),
    quiet_leases(),
    Tx = metric(bytes_tx), Ok = metric(hs_ok), Failed = metric(hs_failed),
    Body = hs_pack(lists:keyreplace(5, 1, hs_fields(), {5, <<28:32/little>>})),
    {ok, S} = gen_tcp:connect("127.0.0.1", Port, [binary, {active, false}], 1000),
    try
        ok = gen_tcp:send(S, parrot_gw:build_frame(1, 1, <<>>, <<>>, Body)),
        {2, 1} = gw_recv_ft_cid(S),
        eventually(fun() -> metric(hs_ok) =:= Ok + 1 end),
        true = metric(bytes_tx) > Tx,
        Failed = metric(hs_failed),
        ok = gen_tcp:send(S, parrot_gw:build_frame(3, 2, <<>>, <<>>, <<>>)),
        {4, 2} = gw_recv_ft_cid(S),
        ok = gen_tcp:send(S, <<29:32/little>>),
        {error, closed} = gen_tcp:recv(S, 0, 1000),
        quiet_leases(),
        true = metric(hs_ok) =:= Ok + 1,
        Failed = metric(hs_failed)
    after gen_tcp:close(S) end.

t_counter_saturation() ->
    {ok, Port} = parrot_gw:start(0),
    quiet_leases(),
    Keys = [bytes_tx, bytes_rx, hs_ok, hb],
    Saved = [{K, metric(K)} || K <- Keys],
    Max = 16#FFFFFFFFFFFFFFFF,
    try
        ets:insert(parrot_gw_metrics, [{K, Max - 2} || K <- Keys]),
        Peers = [spawn_monitor(fun() ->
            {ok, S} = gw_connect(Port),
            Driver = accepted_driver(S),
            true = is_pid(Driver),
            Retired = erlang:monitor(process, Driver),
            try
                ok = gen_tcp:send(S, parrot_gw:build_frame(3, 5, <<>>, <<>>, <<>>)),
                {4, 5} = gw_recv_ft_cid(S)
            after gen_tcp:close(S) end,
            %% Client completion is not a server retirement receipt. Await the
            %% actual driver, whose normal exit follows lease retirement.
            receive {'DOWN', Retired, process, Driver, normal} -> ok;
                    {'DOWN', Retired, process, Driver, Reason} -> erlang:error({driver_failed, Reason})
            after 3000 -> erlang:error({driver_not_retired, process_info(Driver, [current_stacktrace, messages])}) end
        end) || _ <- lists:seq(1, 16)],
        [receive {'DOWN', Ref, process, Pid, normal} -> ok;
                 {'DOWN', Ref, process, Pid, Reason} -> erlang:error({peer_failed, Reason})
         after 5000 -> erlang:error(peer_timeout) end || {Pid, Ref} <- Peers],
        0 = metric(connections),
        [Max = metric(K) || K <- Keys]
    after ets:insert(parrot_gw_metrics, Saved) end.

t_handshake_deadline() ->
    {ok, Port} = parrot_gw:start(0),
    quiet_leases(),
    Failed = metric(hs_failed),
    {ok, S} = gen_tcp:connect("127.0.0.1", Port, [binary, {active, false}], 1000),
    Start = erlang:monotonic_time(millisecond),
    try
        %% A partial header late in the handshake must not reset the total deadline.
        receive after 6000 -> ok end,
        ok = gen_tcp:send(S, <<100:32/little, 1>>),
        {error, closed} = gen_tcp:recv(S, 0, 6500),
        Elapsed = erlang:monotonic_time(millisecond) - Start,
        true = Elapsed >= 9000 andalso Elapsed < 12500,
        quiet_leases(),
        true = metric(hs_failed) =:= Failed + 1
    after gen_tcp:close(S) end.

t_work_retirement(Ft) ->
    Parent = self(),
    Blocked = fun(_, _) -> Parent ! {blocked_worker, self()}, receive release -> {<<"done">>,<<>>} end end,
    {ok, Port} = parrot_gw:start(0, Blocked),
    {ok, S} = gw_connect(Port),
    Driver = accepted_driver(S),
    DriverRef = erlang:monitor(process, Driver),
    ok = gen_tcp:send(S, parrot_gw:build_frame(Ft, 80, <<>>, <<"test">>, <<0:32/little>>)),
    Worker = receive {blocked_worker, Pid} -> Pid after 1000 -> erlang:error(worker_not_started) end,
    WorkerRef = erlang:monitor(process, Worker),
    try
        gen_tcp:close(S),
        receive {'DOWN', DriverRef, process, Driver, _} -> ok after 3000 -> erlang:error(driver_not_retired) end,
        receive {'DOWN', WorkerRef, process, Worker, _} -> ok
        after 1000 -> erlang:error({orphan_work_after_driver_retirement, Ft}) end
    after gen_tcp:close(S), exit(Worker, kill), erlang:demonitor(WorkerRef,[flush]), erlang:demonitor(DriverRef,[flush]) end.

t_killed_work_tree() ->
    Parent = self(),
    Blocked = fun(_, _) -> Parent ! {kill_worker,self()}, receive release -> {<<>>,<<>>} end end,
    {ok, Port} = parrot_gw:start(0,Blocked), {ok,S} = gw_connect(Port),
    Driver = accepted_driver(S),
    {dictionary, Dictionary} = process_info(Driver,dictionary),
    Sup = proplists:get_value(task_sup,Dictionary),
    true = is_pid(Sup),
    Mon = erlang:monitor(process,Sup),
    ok = gen_tcp:send(S,parrot_gw:build_frame(?ASK,1,<<>>,<<"test">>,<<0:32/little>>)),
    Worker = receive {kill_worker,Pid} -> Pid after 1000 -> erlang:error(worker_not_started) end,
    try
        exit(Driver,kill),
        receive {'DOWN',Mon,process,Sup,_} -> ok after 3000 -> erlang:error(supervisor_not_retired) end,
        false = is_process_alive(Worker),
        {error,closed} = gen_tcp:recv(S,0,1000)
    after gen_tcp:close(S),exit(Worker,kill),erlang:demonitor(Mon,[flush]) end.

t_direct_retirement() ->
    {ok, Listen} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true}]),
    {ok, DirectPort} = inet:port(Listen),
    {ok, Port} = parrot_gw:start(0),
    {ok, Parent} = gw_connect(Port),
    Addr = list_to_binary("127.0.0.1:" ++ integer_to_list(DirectPort)),
    Hint = <<11:16/little,"direct-peer",(byte_size(Addr)):16/little,Addr/binary>>,
    try
        ok = gen_tcp:send(Parent,parrot_gw:build_frame(16#24,1,<<>>,<<>>,Hint)),
        {ok, Direct} = gen_tcp:accept(Listen,3000),
        try
            {1,_} = gw_recv_ft(Direct),
            Ack = iolist_to_binary(parrot_gw:handshake_ack_body("direct-peer")),
            Hs = parrot_gw:build_frame(2,1,<<>>,<<>>,Ack),
            Hb = parrot_gw:build_frame(3,2,<<>>,<<>>,<<>>),
            ok = gen_tcp:send(Direct,<<Hs/binary,Hb/binary>>),
            {4,2} = gw_recv_ft_cid(Direct),
            gen_tcp:close(Parent),
            {error,closed} = gen_tcp:recv(Direct,0,3000)
        after gen_tcp:close(Direct) end
    after gen_tcp:close(Parent), gen_tcp:close(Listen) end.

t_work_capacity(Ft) ->
    Parent = self(), Tag = make_ref(),
    Blocked = fun(_, _) -> Parent ! {Tag, self()}, receive release -> {<<"done">>,<<>>} end end,
    {ok, Port} = parrot_gw:start(0, Blocked),
    {ok, S} = gw_connect(Port),
    Driver = accepted_driver(S), Mon = erlang:monitor(process, Driver),
    try
        First = [parrot_gw:build_frame(Ft,N,<<>>,<<"test">>,<<0:32/little>>) || N <- lists:seq(1,64)],
        ok = gen_tcp:send(S, First),
        Workers = [receive {Tag, Pid} -> Pid after 3000 -> erlang:error(work_not_admitted) end || _ <- lists:seq(1,64)],
        try
            ok = gen_tcp:send(S,parrot_gw:build_frame(Ft,65,<<>>,<<"test">>,<<0:32/little>>)),
            case Ft of
                ?ASK ->
                    {?REPLY_ERR,65} = gw_recv_ft_cid(S),
                    ok = gen_tcp:send(S,parrot_gw:build_frame(3,66,<<>>,<<>>,<<>>)),
                    {4,66} = gw_recv_ft_cid(S),
                    gen_tcp:close(S);
                16#13 -> {error,closed} = gen_tcp:recv(S,0,3000)
            end,
            receive {'DOWN',Mon,process,Driver,normal} -> ok after 3000 -> erlang:error(driver_not_retired) end,
            true = lists:all(fun(P) -> not is_process_alive(P) end,Workers),
            receive {Tag,_} -> erlang:error(overload_executed) after 0 -> ok end
        after [exit(P,kill) || P <- Workers] end
    after gen_tcp:close(S), erlang:demonitor(Mon,[flush]) end.

hint(S, Node, Port) ->
    Addr = list_to_binary("127.0.0.1:" ++ integer_to_list(Port)),
    Payload = <<(byte_size(Node)):16/little,Node/binary,(byte_size(Addr)):16/little,Addr/binary>>,
    gen_tcp:send(S,parrot_gw:build_frame(16#24,1,<<>>,<<>>,Payload)).

accept_direct(Listen, Node) ->
    {ok, S} = gen_tcp:accept(Listen,3000),
    {1,_} = gw_recv_ft(S),
    Ack = iolist_to_binary(parrot_gw:handshake_ack_body(Node)),
    Hs = parrot_gw:build_frame(2,1,<<>>,<<>>,Ack),
    Hb = parrot_gw:build_frame(3,2,<<>>,<<>>,<<>>),
    ok = gen_tcp:send(S,<<Hs/binary,Hb/binary>>),
    {4,2} = gw_recv_ft_cid(S),
    S.

t_direct_capacity() ->
    {ok, Listen} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{backlog,32}]),
    {ok, DirectPort} = inet:port(Listen),
    {ok, Port} = parrot_gw:start(0), {ok, S} = gw_connect(Port),
    try
        [ok = hint(S,<<"pending-first">>,DirectPort) || _ <- lists:seq(1,3)],
        {ok, First} = gen_tcp:accept(Listen,3000),
        try
            {1,_} = gw_recv_ft(First),
            {error,timeout} = gen_tcp:accept(Listen,200),
            Others = [begin
                ok = hint(S,integer_to_binary(N),DirectPort),
                {ok, Child} = gen_tcp:accept(Listen,3000),
                {1,_} = gw_recv_ft(Child), Child
            end || N <- lists:seq(1,7)],
            try
                ok = hint(S,<<"excess">>,DirectPort),
                {error,timeout} = gen_tcp:accept(Listen,200),
                gen_tcp:close(S),
                [{error,closed} = gen_tcp:recv(Child,0,3000) || Child <- [First|Others]]
            after [gen_tcp:close(Child) || Child <- Others] end
        after gen_tcp:close(First) end
    after gen_tcp:close(S),gen_tcp:close(Listen) end.

t_nested_direct() ->
    {ok, Listen} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true}]),
    {ok, DirectPort} = inet:port(Listen),
    {ok, Port} = parrot_gw:start(0), {ok, S} = gw_connect(Port),
    Driver = accepted_driver(S), Mon = erlang:monitor(process,Driver),
    try
        ok = hint(S,<<"first-peer">>,DirectPort),
        First = accept_direct(Listen,<<"first-peer">>),
        try
            ok = hint(First,<<"second-peer">>,DirectPort),
            Second = accept_direct(Listen,<<"second-peer">>),
            try
                gen_tcp:close(S),
                receive {'DOWN',Mon,process,Driver,normal} -> ok after 3000 -> erlang:error(driver_not_retired) end,
                {error,closed} = gen_tcp:recv(First,0,1000),
                {error,closed} = gen_tcp:recv(Second,0,1000),
                [] = ets:lookup(parrot_direct_links,<<"first-peer">>),
                [] = ets:lookup(parrot_direct_links,<<"second-peer">>)
            after gen_tcp:close(Second) end
        after gen_tcp:close(First) end
    after gen_tcp:close(S),gen_tcp:close(Listen),erlang:demonitor(Mon,[flush]) end.

t_connection_capacity() ->
    {ok, Port} = parrot_gw:start(0), quiet_leases(),
    try
        with_capacity_peers(Port,256,fun([Vacated|_]) ->
            256 = metric(connections),
            {ok, Extra} = gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}],1000),
            try {error,closed} = gen_tcp:recv(Extra,0,1000)
            after gen_tcp:close(Extra) end,
            256 = metric(connections),
            ok = gen_tcp:close(Vacated),
            eventually(fun() -> metric(connections) =:= 255 end),
            Replacement = capacity_peer(Port),
            try 256 = metric(connections)
            after gen_tcp:close(Replacement) end
        end)
    after quiet_leases() end.

with_capacity_peers(_Port,0,F) -> F([]);
with_capacity_peers(Port,N,F) ->
    S = capacity_peer(Port),
    try with_capacity_peers(Port,N-1,fun(Peers) -> F([S|Peers]) end)
    after gen_tcp:close(S) end.

capacity_peer(Port) ->
    {ok,S} = gw_connect(Port),
    try
        %% ACK precedes lease activation. A response on the same driver proves
        %% that activation finished before an exact metric assertion.
        ok = gen_tcp:send(S,parrot_gw:build_frame(16#03,573,<<>>,<<>>,<<>>)),
        {16#04,0,573,<<>>,<<>>,<<>>} = gw_recv_frame(S),
        S
    catch Class:Reason:Stack ->
        gen_tcp:close(S), erlang:raise(Class,Reason,Stack)
    end.

t_acceptor_handoff_death() ->
    {ok, Port} = parrot_gw:start(0), quiet_leases(),
    Owner = whereis(parrot_gw_lease_owner), Failed = metric(hs_failed),
    true = erlang:suspend_process(Owner),
    try
        {ok,S} = gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}],1000),
        try
            Pending = fun() ->
                {messages, Messages} = process_info(Owner,messages),
                [{Caller,Child} || {lease,Caller,_,{reserve,Child}} <- Messages]
            end,
            eventually(fun() -> length(Pending()) =:= 1 end),
            [{Acceptor,Child}] = Pending(),
            Acceptor = accepted_driver(S),
            Ref = erlang:monitor(process,Child),
            exit(Acceptor,kill),
            receive {'DOWN',Ref,process,Child,_} -> ok after 1000 -> erlang:error(orphan_handoff) end,
            {error,closed} = gen_tcp:recv(S,0,1000)
        after gen_tcp:close(S) end
    after erlang:resume_process(Owner) end,
    quiet_leases(),
    Failed = metric(hs_failed).

%% Receipts prove handler completion, not admission. Barriers are controlled by
%% the test, so a heartbeat alone cannot accidentally make a slow test pass.
delivery_cases() ->
    [{"completion capability on actual gateway only", fun t_completion_capability/0},
     {"completion waits for success and preserves plain TELL", fun t_completion_success/0},
     {"completion reports handler exceptions and explicit errors", fun t_completion_errors/0},
     {"completion overload replies without closing admitted work", fun t_completion_capacity/0},
     {"fitting completion request rejects an oversized receipt before dispatch", fun t_completion_reply_budget/0},
     {"completion worker retires on connection close", fun t_completion_retirement/0},
     {"invalid payload flags never execute business", fun t_payload_flags/0},
     {"completion requires nonzero cid and zero sequence", fun t_completion_identity/0},
     {"malformed ASK prefix cannot become business payload", fun t_reply_to_invalid/0},
     {"valid long Unicode ASK reply_to is fully stripped", fun t_reply_to_long/0},
     {"invalid ASK handler result returns a terminal error", fun t_ask_bad_result/0},
     {"component handler failure cannot fall through to builtin success", fun t_component_error/0}].

delivery_recv(S) -> delivery_recv(S, saved_tail(S)).
delivery_recv(S, Buf) ->
    case parrot_gw:parse_frame(Buf) of
        {ok,Ft,Flags,Cid,Path,Key,Payload,Tail} ->
            put({wire_tail,S},Tail), {Ft,Flags,Cid,Path,Key,Payload};
        {more,_} ->
            {ok,More} = gen_tcp:recv(S,0,3000),
            delivery_recv(S,<<Buf/binary,More/binary>>)
    end.

expect_error(S,Cid,Code) ->
    {?REPLY_ERR,0,Cid,_,_,<<Code:16/little,0:16/little,Detail/binary>>} = delivery_recv(S),
    true = byte_size(Detail) > 0.

completion_frame(Cid,Payload) -> parrot_gw:build_frame(16#13,16#10,Cid,<<"/u">>,<<"test">>,Payload).

t_completion_capability() ->
    Helper = parrot_gw:decode_handshake(iolist_to_binary(parrot_gw:handshake_body("helper")),false),
    0 = maps:get(capabilities,Helper) band 256,
    {ok,Port} = parrot_gw:start(0),
    {ok,S} = gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}]),
    try
        Body = iolist_to_binary(parrot_gw:handshake_body("peer")),
        ok = gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,Body)),
        {2,0,1,_,_,Ack} = delivery_recv(S),
        Peer = parrot_gw:decode_handshake(Ack,true),
        256 = maps:get(capabilities,Peer) band 256,
        0 = maps:get(capabilities,Peer) band 512
    after gen_tcp:close(S) end.

t_completion_success() ->
    Parent = self(), Tag = make_ref(),
    Svc = fun(_, Payload) -> Parent ! {Tag,self(),Payload}, receive finish -> ok end end,
    {ok,Port} = parrot_gw:start(0,Svc), {ok,S} = gw_connect(Port),
    try
        ok = gen_tcp:send(S,completion_frame(31,<<1,2,3>>)),
        Worker = receive {Tag,P,<<1,2,3>>} -> P after 3000 -> erlang:error(missing_worker) end,
        ok = gen_tcp:send(S,parrot_gw:build_frame(3,32,<<>>,<<>>,<<>>)),
        {4,32} = gw_recv_ft_cid(S),
        Worker ! finish,
        {?REPLY,0,31,<<>>,<<"parrot:completion#v1">>,<<>>} = delivery_recv(S),
        %% Normal-return values are not serialized by completion delivery.
        ok = gen_tcp:send(S,parrot_gw:build_frame(16#13,0,<<>>,<<"test">>,<<4>>)),
        Plain = receive {Tag,P2,<<4>>} -> P2 after 3000 -> erlang:error(missing_plain_worker) end,
        Mon = erlang:monitor(process,Plain), Plain ! finish,
        receive {'DOWN',Mon,process,Plain,normal} -> ok after 3000 -> erlang:error(plain_not_done) end,
        ok = gen_tcp:send(S,parrot_gw:build_frame(3,33,<<>>,<<>>,<<>>)),
        {4,33} = gw_recv_ft_cid(S)
    after gen_tcp:close(S) end.

t_completion_errors() ->
    Svc = fun(_,<<0>>) -> erlang:error({unknown_service,<<"test">>});
             (_,<<1>>) -> erlang:error(handler_failed);
             (_,<<2>>) -> throw(handler_threw);
             (_,<<3>>) -> exit(handler_exited);
             (_,<<4>>) -> {error,handler_rejected}
          end,
    {ok,Port} = parrot_gw:start(0,Svc), {ok,S} = gw_connect(Port),
    try
        [begin
            ok = gen_tcp:send(S,completion_frame(N+40,<<N>>)),
            expect_error(S,N+40,case N of 0 -> 6; _ -> 12 end)
         end || N <- lists:seq(0,4)],
        ok = gen_tcp:send(S,parrot_gw:build_frame(3,49,<<>>,<<>>,<<>>)),
        {4,49} = gw_recv_ft_cid(S)
    after gen_tcp:close(S) end.

t_completion_capacity() ->
    Parent = self(), Tag = make_ref(),
    Svc = fun(_, _) -> Parent ! {Tag,self()}, receive finish -> ok end end,
    {ok,Port} = parrot_gw:start(0,Svc), {ok,S} = gw_connect(Port),
    try
        ok = gen_tcp:send(S,[completion_frame(N,<<>>) || N <- lists:seq(1,64)]),
        Workers = [receive {Tag,P} -> P after 3000 -> erlang:error(missing_worker) end || _ <- lists:seq(1,64)],
        ok = gen_tcp:send(S,completion_frame(65,<<>>)),
        expect_error(S,65,10),
        ok = gen_tcp:send(S,parrot_gw:build_frame(3,66,<<>>,<<>>,<<>>)),
        {4,66} = gw_recv_ft_cid(S),
        true = lists:all(fun erlang:is_process_alive/1,Workers),
        [P ! finish || P <- Workers],
        Cids = [begin {?REPLY,0,Cid,<<>>,<<"parrot:completion#v1">>,<<>>} = delivery_recv(S), Cid end || _ <- Workers],
        CidsSorted = lists:seq(1,64), CidsSorted = lists:sort(Cids),
        receive {Tag,_} -> erlang:error(overload_executed) after 0 -> ok end
    after gen_tcp:close(S) end.

t_completion_retirement() ->
    Parent = self(),Tag = make_ref(),
    {ok,Port} = parrot_gw:start(0,fun(_,_) -> Parent ! {Tag,self()}, receive never -> ok end end),
    {ok,S} = gw_connect(Port), Driver = accepted_driver(S), Mon = erlang:monitor(process,Driver),
    try
        ok = gen_tcp:send(S,completion_frame(77,<<>>)),
        Worker = receive {Tag,P} -> P after 3000 -> erlang:error(missing_worker) end,
        gen_tcp:close(S),
        receive {'DOWN',Mon,process,Driver,normal} -> ok after 3000 -> erlang:error(driver_not_retired) end,
        false = is_process_alive(Worker)
    after gen_tcp:close(S),erlang:demonitor(Mon,[flush]) end.

%% Invalid frames are built structurally; the gateway must reject them before
%% applying any service or admin side effects, on either receive-loop path.
reject_delivery_frame(Frame) ->
    Parent = self(),Tag = make_ref(),
    {ok,Port} = parrot_gw:start(0,fun(_,_) -> Parent ! Tag, {<<"reply">>,<<>>} end),
    {ok,S} = gw_connect(Port), Driver = accepted_driver(S),Mon = erlang:monitor(process,Driver),
    try
        ok = gen_tcp:send(S,Frame),
        {error,closed} = gen_tcp:recv(S,0,3000),
        receive {'DOWN',Mon,process,Driver,normal} -> ok after 3000 -> erlang:error(driver_not_retired) end,
        receive Tag -> erlang:error(invalid_frame_executed) after 0 -> ok end
    after gen_tcp:close(S),erlang:demonitor(Mon,[flush]) end.

t_payload_flags() ->
    [reject_delivery_frame(parrot_gw:build_frame(Ft,Flags,9,<<>>,<<"test">>,<<0:32/little>>))
     || Ft <- [?ASK,16#13], Flags <- [1,4,32,16#8000]],
    reject_delivery_frame(parrot_gw:build_frame(?ASK,16#10,9,<<>>,<<"test">>,<<0:32/little>>)),
    reject_delivery_frame(parrot_gw:build_frame(16#13,16#10,9,<<>>,<<"parrot:durable#v1">>,<<>>)).

t_completion_identity() ->
    reject_delivery_frame(completion_frame(0,<<>>)),
    <<Head:18/binary,_:32/little,Tail/binary>> = completion_frame(7,<<>>),
    reject_delivery_frame(<<Head/binary,1:32/little,Tail/binary>>).

t_reply_to_invalid() ->
    Parent = self(),Tag = make_ref(),
    {ok,Port} = parrot_gw:start(0,fun(_,_) -> Parent ! Tag, {<<"reply">>,<<>>} end),
    {ok,S} = gw_connect(Port),
    try
        [begin
            ok = gen_tcp:send(S,parrot_gw:build_frame(?ASK,90,<<>>,<<"test">>,Payload)),
            expect_error(S,90,12)
         end || Payload <- [<<>>,<<0>>,<<0,0,0>>,<<8:32/little,"short">>,<<1:32/little,255>>]],
        ok = gen_tcp:send(S,parrot_gw:build_frame(3,91,<<>>,<<>>,<<>>)),
        {4,91} = gw_recv_ft_cid(S),
        receive Tag -> erlang:error(malformed_ask_executed) after 0 -> ok end
    after gen_tcp:close(S) end.

t_reply_to_long() ->
    {ok,Port} = parrot_gw:start(0,fun(_,Payload) -> {<<"echo">>,Payload} end),
    {ok,S} = gw_connect(Port),
    try
        ReplyTo = binary:copy(unicode:characters_to_binary("路径"),1000),
        Payload = <<(byte_size(ReplyTo)):32/little,ReplyTo/binary,"business">>,
        ok = gen_tcp:send(S,parrot_gw:build_frame(?ASK,92,<<>>,<<"test">>,Payload)),
        {?REPLY,0,92,<<>>,<<"echo">>,<<"business">>} = delivery_recv(S)
    after gen_tcp:close(S) end.

t_ask_bad_result() ->
    {ok,Port} = parrot_gw:start(0,fun(_,_) -> wrong_return end),
    {ok,S} = gw_connect(Port),
    try
        ok = gen_tcp:send(S,parrot_gw:build_frame(?ASK,93,<<>>,<<"test">>,<<0:32/little>>)),
        expect_error(S,93,12)
    after gen_tcp:close(S) end.

parrot_service(<<"bin:u:Ping">>,_) -> erlang:error(component_failed);
parrot_service(Key,_) -> erlang:error({unknown_service,Key}).

t_component_error() ->
    parrot_gw:admin_init(),
    ets:insert(parrot_admin_components,{<<"delivery-test">>,<<"v1">>,[<<"/test">>],?MODULE}),
    try
        Result = try parrot_gw:service(<<"bin:u:Ping">>,<<1:64/little>>) of
                     Value -> {unexpected_success,Value}
                 catch error:component_failed -> failed end,
        failed = Result,
        %% Only an explicit unknown_service may fall through to another handler.
        {<<"bin:u:AddR">>,<<10003:64/little>>} = parrot_gw:service(<<"bin:u:Add">>,<<1:64/little,2:64/little>>)
    after ets:delete(parrot_admin_components,<<"delivery-test">>) end.


t_identity_encoder() ->
    [begin
        assert_rejected(fun() -> parrot_gw:handshake_body(Id) end),
        assert_rejected(fun() -> parrot_gw:handshake_ack_body(Id) end)
    end || Id <- [<<>>, <<0>>, <<"left",0,"right">>, <<255>>, binary:copy(<<231,149,140>>,21846)]].

t_identity_decoder() ->
    [assert_rejected(fun() -> parrot_gw:decode_handshake(hs_pack(lists:keyreplace(1,1,hs_fields(),{1,Id})),Ack) end)
     || Id <- [<<0>>, <<"left",0,"right">>], Ack <- [false,true]].

t_identity_listener() ->
    {ok,Port}=parrot_gw:start(0),
    {ok,S}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}],5000),
    try
        Body=hs_pack(lists:keyreplace(1,1,hs_fields(),{1,<<"left",0,"right">>})),
        H=parrot_gw:build_frame(1,1,<<>>,<<>>,Body),
        B=parrot_gw:build_frame(3,2,<<>>,<<>>,<<>>),
        ok=gen_tcp:send(S,<<H/binary,B/binary>>),
        {error,closed}=gen_tcp:recv(S,0,1000)
    after gen_tcp:close(S) end,
    {ok,Good}=gw_connect(Port),
    try
        ok=gen_tcp:send(Good,parrot_gw:build_frame(3,3,<<>>,<<>>,<<>>)),
        {4,3}=gw_recv_ft_cid(Good)
    after gen_tcp:close(Good) end.

t_identity_limit() ->
    Id=binary:copy(<<231,149,140>>,21845),
    [begin
        Body=iolist_to_binary(case Ack of true->parrot_gw:handshake_ack_body(Id);false->parrot_gw:handshake_body(Id) end),
        <<1,65535:16/little,_/binary>>=Body,
        #{node:=Id}=parrot_gw:decode_handshake(<<Body/binary,99,1:16/little,255,99,1:16/little,0>>,Ack)
    end || Ack <- [true,false]].


t_completion_reply_budget() ->
    Parent=self(), Tag=make_ref(),
    Svc=fun(_,_) -> Parent ! {Tag,self()}, receive finish -> ok end end,
    {ok,Port}=parrot_gw:start(0,Svc),
    {ok,S}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}],1000),
    try
        Body=hs_pack(lists:keyreplace(5,1,hs_fields(),{5,<<32:32/little>>})),
        ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,Body)),
        {2,1}=gw_recv_ft_cid(S),
        Driver=accepted_driver(S), {dictionary,Dictionary}=process_info(Driver,dictionary),
        Sup=proplists:get_value(task_sup,Dictionary),
        %% This 28-byte request really fits; its reserved success receipt does not.
        Request=parrot_gw:build_frame(16#13,16#10,2,<<>>,<<>>,<<>>),
        32=byte_size(Request),
        ok=gen_tcp:send(S,Request),
        {?REPLY_ERR,0,2,<<>>,<<>>,<<10:16/little,0:16/little>>}=delivery_recv(S),
        []=supervisor:which_children(Sup),
        %% Same-size ordinary TELL can enter the actual controlled service.
        ok=gen_tcp:send(S,parrot_gw:build_frame(16#13,0,<<>>,<<>>,<<>>)),
        Worker=receive {Tag,P}->P after 3000->erlang:error(missing_plain_worker) end,
        Mon=erlang:monitor(process,Worker), Worker ! finish,
        receive {'DOWN',Mon,process,Worker,normal}->ok after 3000->erlang:error(worker_not_retired) end,
        ok=gen_tcp:send(S,parrot_gw:build_frame(3,3,<<>>,<<>>,<<>>)),
        {4,3}=gw_recv_ft_cid(S)
    after gen_tcp:close(S) end.
