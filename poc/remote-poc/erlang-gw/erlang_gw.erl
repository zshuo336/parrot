%% Local POC wire gateway: all business calls execute in one OTP gen_server.
%% The 28-byte body fields and 1 MiB budget match the independent Rust POC.
-module(erlang_gw).
-behaviour(gen_server).
-export([main/1, handle_frame/7]).
-export([init/1, handle_call/3, handle_cast/2, handle_info/2, terminate/2]).
-define(ASK, 16#10).
-define(REPLY, 16#11).
-define(REPLY_ERR, 16#12).
-define(TELL, 16#13).
-define(MAX_BODY, 1048576).

build_frame(Ft, Cid, Path, Key, Payload) ->
    BodyLen = 28 + byte_size(Path) + byte_size(Key) + byte_size(Payload),
    true = BodyLen =< ?MAX_BODY,
    <<BodyLen:32/little, 1:8, Ft:8, 0:16, Cid:64/little, 0:64/little,
      (byte_size(Path)):32/little, Path/binary,
      (byte_size(Key)):32/little, Key/binary, Payload/binary>>.

parse_frame(<<BodyLen:32/little, _/binary>>) when BodyLen < 28; BodyLen > ?MAX_BODY ->
    {error, invalid_length};
parse_frame(<<BodyLen:32/little, Rest/binary>>) when byte_size(Rest) >= BodyLen ->
    <<Body:BodyLen/binary, Tail/binary>> = Rest,
    case Body of
        <<1:8, Ft:8, 0:16, Cid:64/little, 0:64/little,
          PathLen:32/little, Path:PathLen/binary,
          KeyLen:32/little, Key:KeyLen/binary, Payload/binary>>
          when Ft >= ?ASK, Ft =< 16#14 ->
            case {unicode:characters_to_binary(Path, utf8, utf8),
                  unicode:characters_to_binary(Key, utf8, utf8)} of
                {Path, Key} -> {ok, Ft, Cid, Path, Key, Payload, Tail};
                _ -> {error, invalid_utf8}
            end;
        _ -> {error, invalid_frame}
    end;
parse_frame(_) -> more.

service(<<"bin:u:Ping">>, <<N:64/little>>) ->
    {<<"bin:u:Pong">>, <<(N + 3):64/little>>};
service(<<"bin:u:Add">>, <<A:64/little, B:64/little>>) ->
    {<<"bin:u:AddR">>, <<(A + B + 10000):64/little>>};
service(Key, _) -> erlang:error({unknown_service, Key}).

main([Port]) when is_integer(Port) ->
    {ok, LSock} = gen_tcp:listen(Port, [binary, {packet, raw}, {active, false},
        {ip, {127,0,0,1}}, {nodelay, true}, {reuseaddr, true}, {send_timeout, 5000}]),
    {ok, {_, Bound}} = inet:sockname(LSock),
    io:format("POC_PORT ~p~n", [Bound]),
    {ok, Sock} = gen_tcp:accept(LSock),
    gen_tcp:close(LSock),
    {ok, Pid} = gen_server:start_link(?MODULE, Sock, []),
    loop(Sock, Pid, <<>>),
    gen_tcp:close(Sock),
    gen_server:stop(Pid),
    halt(0).

%% Drain buffered frames before receiving again; TCP does not preserve writes.
loop(Sock, Pid, Buf) ->
    case parse_frame(Buf) of
        {ok, Ft, Cid, Path, Key, Payload, Tail} ->
            handle_frame(Sock, Pid, Ft, Cid, Path, Key, Payload),
            loop(Sock, Pid, Tail);
        more ->
            case gen_tcp:recv(Sock, 0, 30000) of
                {ok, Data} -> loop(Sock, Pid, <<Buf/binary, Data/binary>>);
                {error, _} -> ok
            end;
        {error, _} -> ok
    end.

handle_frame(_Sock, Pid, Ft, Cid, Path, Key, Payload) ->
    gen_server:call(Pid, {frame, Ft, Cid, Path, Key, Payload}, 10000).

init(Sock) -> {ok, #{sock => Sock, pending => #{}, next => 1, tells => 0}}.

send(State, Ft, Cid, Key, Payload) ->
    ok = gen_tcp:send(maps:get(sock, State), build_frame(Ft, Cid, <<>>, Key, Payload)).

handle_call({frame, ?ASK, Cid, _Path, <<"bin:u:Probe">>, <<>>}, _,
            #{sock := Sock, pending := Pending, next := Next} = State) when map_size(Pending) < 128 ->
    ok = gen_tcp:send(Sock, build_frame(?ASK, Next, <<"/user/rust_service">>,
                                      <<"bin:u:Ping">>, <<100:64/little>>)),
    Timer = erlang:send_after(5000, self(), {expired, Next}),
    {reply, ok, State#{pending := Pending#{Next => {Cid, Timer}}, next := Next + 1}};
handle_call({frame, ?ASK, Cid, _Path, <<"bin:u:TellCount">>, <<>>}, _, State) ->
    send(State, ?REPLY, Cid, <<"bin:u:AddR">>, <<(maps:get(tells, State)):64/little>>),
    {reply, ok, State};
handle_call({frame, ?ASK, Cid, _Path, Key, Payload}, _, State) ->
    try service(Key, Payload) of
        {ReplyKey, ReplyPayload} -> send(State, ?REPLY, Cid, ReplyKey, ReplyPayload)
    catch _:Reason ->
        send(State, ?REPLY_ERR, Cid, <<>>, iolist_to_binary(io_lib:format("~p", [Reason])))
    end,
    {reply, ok, State};
handle_call({frame, ?TELL, _Cid, _Path, Key, Payload}, _, State) ->
    Updated = try service(Key, Payload) of
        _ -> State#{tells := maps:get(tells, State) + 1}
    catch _:_ -> State end,
    {reply, ok, Updated};
handle_call({frame, Ft, Cid, _Path, Key, Payload}, _, #{pending := Pending} = State)
  when Ft == ?REPLY; Ft == ?REPLY_ERR ->
    case maps:take(Cid, Pending) of
        error -> {reply, ok, State};
        {{Original, Timer}, Rest} ->
            erlang:cancel_timer(Timer),
            case {Ft, Key, byte_size(Payload)} of
                {?REPLY, <<"bin:u:Pong">>, 8} ->
                    send(State, ?REPLY, Original, <<"bin:u:AddR">>, Payload);
                _ -> send(State, ?REPLY_ERR, Original, <<>>, <<"invalid reverse reply">>)
            end,
            {reply, ok, State#{pending := Rest}}
    end;
handle_call(_, _, State) -> {reply, ok, State}.
handle_cast(_, State) -> {noreply, State}.
handle_info({expired, Cid}, #{pending := Pending} = State) ->
    case maps:take(Cid, Pending) of
        error -> {noreply, State};
        {{Original, _}, Rest} ->
            send(State, ?REPLY_ERR, Original, <<>>, <<"reverse ask timeout">>),
            {noreply, State#{pending := Rest}}
    end;
handle_info(_, State) -> {noreply, State}.
terminate(_, _) -> ok.
