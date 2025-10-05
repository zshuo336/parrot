%% Parrot Erlang Gateway 模块本体（DEV_03 §7 / E6——POC 转正为 Wire 1.0）。
%%
%% Rust 节点 <--Wire 1.0(TLV 握手)--> 本网关 <--OTP 原生--> Erlang 进程
%%
%% 帧布局（07 §2.1，与 Rust/JVM/Python 逐字节一致）：
%%   外层: [u32 body_len LE]
%%   body: [ver u8][ft u8][flags u16 LE][cid u64 LE]
%%         [hop_count u8][hop_limit u8][rsv u48]
%%         [path_len u32 LE][path][key_len u32 LE][key][payload]
%%
%% 握手 TLV: tag u8 + len u16 LE + value（NODE_ID=1 CAPS=4 MAX_FRAME_LEN=5
%% TOPOLOGY_ROLE=6 HOP_LIMIT=7；ACK 加 CHOSEN_CODEC=8）
-module(parrot_gw).
-export([main/1, start/1, start/2, service/2, build_frame/5, build_frame/6,
         parse_frame/1, handshake_body/1, handshake_ack_body/1, err_payload/2]).

-define(VER, 1).
-define(ASK, 16#10).
-define(REPLY, 16#11).
-define(REPLY_ERR, 16#12).
-define(TELL, 16#13).
-define(HANDSHAKE, 16#01).
-define(HANDSHAKE_ACK, 16#02).
-define(HEARTBEAT, 16#03).
-define(HEARTBEAT_ACK, 16#04).

%% ============ 帧编解码（Wire 1.0 正式布局） ============

build_frame(Ft, Cid, Path, Key, Payload) ->
    build_frame(Ft, 0, Cid, Path, Key, Payload).

build_frame(Ft, Flags, Cid, Path, Key, Payload) ->
    PathB = to_bin(Path),
    KeyB = to_bin(Key),
    %% body = 定长 24 + path_len(4) + path + key_len(4) + key + payload（07 §2.1）
    BodyLen = 28 + byte_size(PathB) + byte_size(KeyB) + byte_size(Payload),
    <<BodyLen:32/little,
      ?VER:8, Ft:8, Flags:16/little, Cid:64/little,
      0:8, 8:8, 0:48,                     %% hop_count / hop_limit / rsv48
      (byte_size(PathB)):32/little, PathB/binary,
      (byte_size(KeyB)):32/little, KeyB/binary,
      Payload/binary>>.

to_bin(B) when is_binary(B) -> B;
to_bin(L) when is_list(L) -> unicode:characters_to_binary(L).

parse_frame(Buf) ->
    case Buf of
        <<BodyLen:32/little, Rest/binary>> when byte_size(Rest) >= BodyLen ->
            Body = binary:part(Rest, 0, BodyLen),
            Tail = binary:part(Rest, BodyLen, byte_size(Rest) - BodyLen),
            <<?VER:8, Ft:8, Flags:16/little, Cid:64/little,
              _HopC:8, _HopL:8, _Rsv:48,
              PathLen:32/little, PathB:PathLen/binary,
              KeyLen:32/little, KeyB:KeyLen/binary,
              Payload/binary>> = Body,
            {ok, Ft, Flags, Cid, PathB, KeyB, Payload, Tail};
        _ ->
            {more, Buf}
    end.

%% ============ 握手 TLV ============

tlv(Tag, V) -> <<Tag:8, (byte_size(V)):16/little, V/binary>>.

caps_pb_only() -> 16#02.  %% erlang 网关 pb 栈（07 §8.1）

handshake_body(NodeId) ->
    Id = to_bin(NodeId),
    [tlv(1, Id),
     tlv(4, <<(caps_pb_only()):32/little>>),
     tlv(5, <<1048576:32/little>>),
     tlv(6, <<0:8>>),
     tlv(7, <<8:8>>)].

handshake_ack_body(NodeId) ->
    handshake_body(NodeId) ++ [tlv(8, <<"pb">>)].

%% ============ Erlang actor 服务（方言可辨识） ============

service(<<"bin:u:Ping">>, <<N:64/little>>) ->
    {<<"bin:u:Pong">>, <<(N + 3):64/little>>};   %% erlang 方言 +3
service(<<"bin:u:Add">>, <<A:64/little, B:64/little>>) ->
    {<<"bin:u:AddR">>, <<(A + B + 10000):64/little>>};  %% erlang 方言 +10000
service(Key, _Payload) ->
    erlang:error({unknown_service, Key}).

%% ============ 错误体（[u16 code][u16 rsv][detail utf8]） ============

err_payload(Code, Detail) ->
    D = to_bin(Detail),
    <<Code:16/little, 0:16/little, D/binary>>.

%% ============ main（网关进程入口） ============

main(Args) ->
    Port = case Args of
               [P] when is_list(P) -> list_to_integer(P);
               [P] when is_integer(P) -> P;
               _ -> 0
           end,
    {ok, RealPort} = start(Port),
    io:format("PARROT_ERL_PORT=~p~n", [RealPort]),
    receive stop_gateway -> halt(0) end.   %% 驻留（被 kill 或 halt）

%% DEV_08 测试口：启动网关并直接返回端口（不打印——测试免 stdout 捕获）。
%% ServiceFun 可注入（默认 ?MODULE:service/2——测试注入慢实现验证并发结构）。
start(Port) -> start(Port, fun ?MODULE:service/2).
start(Port, ServiceFun) ->
    Self = self(),
    _Gw = spawn(fun() ->
                        {ok, LSock} = gen_tcp:listen(Port, [binary, {packet, raw},
                                                            {active, false},
                                                            {nodelay, true},
                                                            {reuseaddr, true},
                                                            {send_timeout, 5000},
                                                            {send_timeout_close, true}]),
                        {ok, RealPort} = inet:port(LSock),
                        Self ! {gw_port, RealPort},
                        {ok, Sock} = gen_tcp:accept(LSock),
                        gen_tcp:close(LSock),
                        put(service_fun, ServiceFun),
                        log("rust node connected: ~p~n", [inet:peername(Sock)]),
                        loop(Sock, <<>>)
                end),
    receive {gw_port, P} -> {ok, P} after 5000 -> {error, gw_start_timeout} end.

log(Fmt, Args) ->
    %% 网关日志走 stderr（stdout 契约只留端口行）
    io:format(standard_error, Fmt, Args).

%% DEV_08 修复：先榨干缓冲区内的完整帧再 recv（原实现每次 recv 只解析
%% 一帧，同批到达的第二帧滞留缓冲直到新数据到达——流水线/合发场景的
%% 解析级队头阻塞；单帧逐发的旧客户端形态掩盖了此 bug）。
loop(Sock, Buf0) ->
    case parse_frame(Buf0) of
        {ok, Ft, Flags, Cid, _Path, Key, Payload, Tail} ->
            handle(Sock, Ft, Flags, Cid, Key, Payload),
            loop(Sock, Tail);
        {more, Rest} ->
            case gen_tcp:recv(Sock, 0, infinity) of
                {ok, Data} ->
                    loop(Sock, <<Rest/binary, Data/binary>>);
                {error, closed} ->
                    log("connection closed~n", []);
                {error, Reason} ->
                    log("recv error ~p~n", [Reason])
            end
    end.

handle(Sock, Ft, _Flags, Cid, Key, Payload) ->
    case Ft of
        ?HANDSHAKE ->
            log("handshake received~n", []),
            AckBody = iolist_to_binary(handshake_ack_body("erl-gw-1")),
            gen_tcp:send(Sock, build_frame(?HANDSHAKE_ACK, Cid, <<"">>,
                                            <<"__handshake__">>,
                                            AckBody));
        ?HEARTBEAT ->
            gen_tcp:send(Sock, build_frame(?HEARTBEAT_ACK, Cid, <<"">>, <<"">>, <<>>));
        ?ASK ->
            %% 剥 reply_to 前缀（4B len + path；回程经 cid 配对）
            Real = split_reply_to(Payload),
            %% DEV_08：worker 进程执行（慢 service 不阻塞后续帧——OTP 每
            %% 进程一调度单元，正是 erlang 并发原生的形态）。Svc 闭包捕获
            %% （spawn 不继承 process dictionary——直接变量捕获）。
            Svc = case get(service_fun) of
                      F when is_function(F, 2) -> F;
                      _ -> fun ?MODULE:service/2
                  end,
            spawn(fun() ->
                          Res = try {ok, Svc(Key, Real)}
                                catch _:R -> {error, R} end,
                          case Res of
                              {ok, {RK, RP}} ->
                                  gen_tcp:send(Sock, build_frame(?REPLY, Cid, <<"">>, RK, RP));
                              {error, Reason} ->
                                  Err = unicode:characters_to_binary(
                                          io_lib:format("~p", [Reason])),
                                  gen_tcp:send(Sock, build_frame(?REPLY_ERR, Cid, <<"">>, <<"">>,
                                                                 err_payload(6, Err)))
                          end
                  end);
        ?TELL ->
            Svc2 = case get(service_fun) of
                       F2 when is_function(F2, 2) -> F2;
                       _ -> fun ?MODULE:service/2
                   end,
            spawn(fun() -> try Svc2(Key, Payload) catch _:_ -> ok end end);
        _ ->
            ok
    end.

split_reply_to(<<Len:32/little, _Skip:Len/binary, Rest/binary>>) when Len < 4096 ->
    Rest;
split_reply_to(B) -> B.
