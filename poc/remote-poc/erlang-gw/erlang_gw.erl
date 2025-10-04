%% Parrot Erlang/OTP Gateway POC
%%
%% Rust 节点 <--Parrot Wire(LE 帧)--> 本网关 <--OTP 原生--> Erlang actor 进程
%%
%% 帧格式（与 rust POC / Java AkkaGw / Python ray_gw 逐字节一致）：
%%   [u32 frame_len][u8 ver][u8 ft][u16 flags][u64 cid][u64 reserved]
%%   [u32 path_len][path][u32 key_len][key][payload]
%% ASK=0x10 REPLY=0x11 REPLY_ERR=0x12 TELL=0x13
%%
%% OTP 接入形态：services 表的 handler 为 {M,F,A} 或 fun，
%% 真实接入时把 handler 换成 gen_server:call/2 到目标进程即可。
-module(erlang_gw).
-behaviour(gen_server).
-export([main/1, handle_frame/7]).
-export([init/1, handle_call/3, handle_cast/2, handle_info/2, terminate/2]).

%% ============ 帧编解码（与三语言实现一致） ============

-define(ASK, 16).
-define(REPLY, 17).
-define(REPLY_ERR, 18).
-define(TELL, 13).
-define(HEADER, 28).   %% ver(1)+ft(1)+flags(2)+cid(8)+reserved(8)+path_len(4)+key_len(4)

build_frame(Ft, Cid, Path, Key, Payload) when is_binary(Path), is_binary(Key), is_binary(Payload) ->
    BodyLen = 28 + byte_size(Path) + byte_size(Key) + byte_size(Payload),
    <<BodyLen:32/little, 1:8, Ft:8, 0:16, Cid:64/little, 0:64/little,
      (byte_size(Path)):32/little, Path/binary,
      (byte_size(Key)):32/little, Key/binary,
      Payload/binary>>.

parse_frame(Buf) ->
    case Buf of
        <<BodyLen:32/little, Rest/binary>> when byte_size(Rest) >= BodyLen ->
            Body = binary:part(Rest, 0, BodyLen),
            Tail = binary:part(Rest, BodyLen, byte_size(Rest) - BodyLen),
            <<1:8, Ft:8, _Flags:16, Cid:64/little, _Reserved:64/little,
              PathLen:32/little, PathBin:PathLen/binary,
              KeyLen:32/little, KeyBin:KeyLen/binary,
              Payload/binary>> = Body,
            {ok, Ft, Cid, PathBin, KeyBin, Payload, Tail};
        _ ->
            {more, Buf}
    end.

%% ============ "Erlang actor 服务"（方言可辨识） ============

%% erlang 方言：Ping(n) -> Pong(n+3)；Add(a,b) -> a+b+10000
service(<<"bin:u:Ping">>, <<N:64/little>>) ->
    {<<"bin:u:Pong">>, <<(N + 3):64/little>>};
service(<<"bin:u:Add">>, <<A:64/little, B:64/little>>) ->
    {<<"bin:u:AddR">>, <<(A + B + 10000):64/little>>};
service(Key, _Payload) ->
    erlang:error({unknown_service, Key}).

%% ============ main：socket 循环 ============

main([Port]) when is_integer(Port) ->
    {ok, LSock} = gen_tcp:listen(Port, [binary, {packet, raw}, {active, false},
                                        {nodelay, true}, {reuseaddr, true}]),
    io:format("[erl-gw] listening on ~p~n", [Port]),
    {ok, Sock} = gen_tcp:accept(LSock),
    gen_tcp:close(LSock),
    io:format("[erl-gw] rust node connected: ~p~n", [inet:peername(Sock)]),
    %% 主动 ask rust 侧（双向验证）
    erlang:spawn(fun() ->
        timer:sleep(1000),
        Cid = erlang:unique_integer([positive]),
        Frame = build_frame(?ASK, Cid, <<"/user/rust_service">>,
                            <<"bin:u:Ping">>, <<100:64/little>>),
        gen_server:cast(?MODULE, {send_raw, Frame}),
        %% 结果由 handle_info({reply, ...}) 打印
        ?MODULE ! {waiting, Cid}
    end),
    %% 用 gen_server 管理状态（pending cid → from；演示 OTP 结构）
    {ok, Pid} = gen_server:start_link({local, ?MODULE}, ?MODULE, {Sock, #{}}, []),
    loop(Sock, Pid, <<>>),
    gen_server:stop(Pid),
    halt(0).

loop(Sock, Pid, Buf0) ->
    case gen_tcp:recv(Sock, 0, 3600000) of
        {ok, Data} ->
            Buf = <<Buf0/binary, Data/binary>>,
            case parse_frame(Buf) of
                {more, Rest} ->
                    loop(Sock, Pid, Rest);
                {ok, Ft, Cid, Path, Key, Payload, Tail} ->
                    handle_frame(Sock, Pid, Ft, Cid, Path, Key, Payload),
                    loop(Sock, Pid, Tail)
            end;
        {error, closed} ->
            io:format("[erl-gw] connection closed~n");
        {error, timeout} ->
            loop(Sock, Pid, Buf0)
    end.

%% 帧分发（ASK 同步执行；TELL 异步 spawn —— 与 akka/ray 网关行为一致）
handle_frame(Sock, _Pid, Ft, Cid, _Path, Key, Payload) ->
    case Ft of
        ?ASK ->
            Res = try {ok, service(Key, Payload)}
                  catch _:R -> {error, R} end,
            case Res of
                {ok, {ReplyKey, ReplyPayload}} ->
                    gen_tcp:send(Sock, build_frame(?REPLY, Cid, <<>>, ReplyKey, ReplyPayload));
                {error, Reason} ->
                    Err = io_lib:format("~p", [Reason]),
                    gen_tcp:send(Sock, build_frame(?REPLY_ERR, Cid, <<>>, <<>>,
                                                   unicode:characters_to_binary(Err)))
            end;
        ?TELL ->
            spawn(fun() -> try service(Key, Payload) catch _:_ -> ok end end);
        ?REPLY ->
            ?MODULE ! {reply, Cid, Payload};
        ?REPLY_ERR ->
            ?MODULE ! {reply_err, Cid, Payload};
        _ ->
            ok
    end.

%% ============ gen_server：演示 OTP 结构化形态 ============

init({Sock, Pending}) ->
    {ok, {Sock, Pending}}.

handle_call(_Req, _From, State) -> {reply, ok, State}.

handle_cast({send_raw, Frame}, {Sock, Pending}) ->
    gen_tcp:send(Sock, Frame),
    {noreply, {Sock, Pending}}.

handle_info({reply, Cid, Payload}, State) ->
    <<N:64/little>> = Payload,
    io:format("[erl-gw] ask rust /user/rust_service Ping(100) -> ~p~n", [N]),
    {noreply, State};
handle_info({reply_err, Cid, Payload}, State) ->
    io:format("[erl-gw] ask rust failed cid=~p: ~p~n", [Cid, Payload]),
    {noreply, State};
handle_info(_Other, State) ->
    {noreply, State}.

terminate(_Reason, _State) -> ok.
