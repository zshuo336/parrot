%% Wire 1.0 对齐测试（DEV_03 §7：golden vectors eunit 断言）。
%%
%% 运行（repo 根）：
%%   erl -noshell -pa interop/erlang -eval 'c:c(parrot_gw), test_wire:run(), halt().'
%%   （或 make：见 interop/erlang/Makefile）
-module(test_wire).
-export([run/0, all/0]).

-define(ASK, 16#10).
-define(REPLY, 16#11).
-define(REPLY_ERR, 16#12).

all() ->
    [{"ask-basic golden", fun t_ask_golden/0},
     {"reply-err golden", fun t_reply_err_golden/0},
     {"partial frame", fun t_partial/0},
     {"pipeline two frames", fun t_pipeline/0},
     {"handshake tlv layout", fun t_handshake/0},
     {"service dialects", fun t_service/0},
     {"err payload layout", fun t_err/0},
     {"DEV_08 slow ask not blocking heartbeat", fun t_slow_ask_concurrency/0},
     {"DEV_08 slow ask not blocking fast ask", fun t_slow_fast_ask/0}].

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

%% golden: docs/vectors/wire1.json ask-basic（hex 逐字节）
t_ask_golden() ->
    Hex = "2b0000000110000001000000000000000008000000000000020000002f780800000062696e3a743a3a4d00000000ab",
    Expect = hex_to_bin(Hex),
    Frame = parrot_gw:build_frame(?ASK, 0, 1, <<"/x">>, <<"bin:t::M">>,
                                  hex_to_bin("00000000ab")),
    Expect = Frame.

%% golden 语义: reply-err stopped（code=3 结构体往返）
t_reply_err_golden() ->
    Frame = parrot_gw:build_frame(?REPLY_ERR, 0, 9, <<>>, <<>>,
                                  parrot_gw:err_payload(3, "stopped")),
    {ok, ?REPLY_ERR, _F, 9, <<>>, <<>>, Payload, <<>>} = parrot_gw:parse_frame(Frame),
    <<3:16/little, 0:16/little, "stopped">> = Payload.

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
    %% 注入慢 service：bin:u:Ping sleep 300ms（其余原逻辑）
    Slow = fun(<<"bin:u:Ping">> = K, P) ->
                   receive after 300 -> ok end,
                   parrot_gw:service(K, P);
              (K, P) -> parrot_gw:service(K, P)
           end,
    {ok, Port} = parrot_gw:start(0, Slow),
    {ok, S} = gw_connect(Port),
    try
        %% 慢 ASK + 立即心跳：心跳 ACK 必须在慢 REPLY 之前到达
        %% reply_to 前缀：u32 len=3 + "a/l"（与载荷长度声明严格一致）
        Ask = parrot_gw:build_frame(?ASK, 100, <<"/user/x">>, <<"bin:u:Ping">>,
                                    <<3:32/little, "a/l", 42:64/little>>),
        Hb = parrot_gw:build_frame(16#03, 101, <<>>, <<>>, <<>>),
        ok = gen_tcp:send(S, <<Ask/binary, Hb/binary>>),
        T0 = os:timestamp(),
        {16#04, _} = gw_recv_ft(S),               %% HEARTBEAT_ACK 先到
        HbMs = timer:now_diff(os:timestamp(), T0) div 1000,
        {?REPLY, 100} = gw_recv_ft_cid(S),        %% 慢 REPLY 随后
        true = HbMs < 250                          %% 300ms 慢 service 期间已应答
    after
        gen_tcp:close(S)
    end.

t_slow_fast_ask() ->
    Slow = fun(<<"bin:u:Ping">> = K, P) ->
                   receive after 300 -> ok end,
                   parrot_gw:service(K, P);
              (K, P) -> parrot_gw:service(K, P)
           end,
    {ok, Port} = parrot_gw:start(0, Slow),
    {ok, S} = gw_connect(Port),
    try
        SlowF = parrot_gw:build_frame(?ASK, 200, <<"/u">>, <<"bin:u:Ping">>,
                                      <<3:32/little, "a/l", 1:64/little>>),
        FastF = parrot_gw:build_frame(?ASK, 201, <<"/u">>, <<"bin:u:Add">>,
                                      <<3:32/little, "a/l", 3:64/little, 4:64/little>>),
        ok = gen_tcp:send(S, <<SlowF/binary, FastF/binary>>),
        %% 快 ASK（Add，未注入）的 REPLY 必须先到
        {?REPLY, 201} = gw_recv_ft_cid(S),
        {?REPLY, 200} = gw_recv_ft_cid(S)
    after
        gen_tcp:close(S)
    end.

gw_connect(Port) ->
    {ok, S} = gen_tcp:connect("127.0.0.1", Port, [binary, {packet, raw},
                                                   {active, false}, {nodelay, true}]),
    Hs = parrot_gw:build_frame(16#01, 1, <<>>, <<"__handshake__">>, <<>>),
    ok = gen_tcp:send(S, Hs),
    {16#02, _} = gw_recv_ft(S),
    {ok, S}.

gw_recv_ft(S) -> gw_recv_loop(S, fun(Ft, _Cid) -> {Ft, none} end, <<>>).
gw_recv_ft_cid(S) -> gw_recv_loop(S, fun(Ft, Cid) -> {Ft, Cid} end, <<>>).

gw_recv_loop(S, Pick, Buf) ->
    case parrot_gw:parse_frame(Buf) of
        {ok, Ft, _Fl, Cid, _P, _K, _Pay, Tail} ->
            case Pick(Ft, Cid) of
                skip -> gw_recv_loop(S, Pick, Tail);
                R -> R
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
