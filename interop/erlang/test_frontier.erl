%% Each application has its own VM: both intentionally export module frontier.
%% Independent wire construction/decoding tests the real application module.
-module(test_frontier).
-export([run/1, all/1]).

run(Kind) ->
    App = case Kind of crawler -> "crawler-lab"; websearch -> "websearch" end,
    Source = "../../apps/" ++ App ++ "/erlang/frontier.erl",
    {ok,frontier,Beam} = compile:file(Source,[binary,report_errors,report_warnings]),
    {module,frontier} = code:load_binary(frontier,Source,Beam),
    lists:foreach(fun({Name,F}) ->
        reset(Kind),
        try F(), io:format("FRONTIER_OK ~p ~s~n",[Kind,Name])
        after reset(Kind) end
    end, all(Kind)).

all(Kind) ->
    [{"frozen valid request and reply",fun() -> valid(Kind) end},
     {"zero Push and Unicode bytes preserve the contract",fun() -> unicode_push(Kind) end},
     {"all truncated Push frames reject before mutation",fun() -> truncated(Kind) end},
     {"invalid counts tails and UTF-8 reject before mutation",fun() -> malformed(Kind) end},
     {"malformed Next leaves queue intact",fun() -> bad_next(Kind) end},
     {"concurrent consumers never duplicate or lose entries",fun() -> concurrent(Kind) end},
     {"duplicate pushes remain deduplicated after removal",fun() -> duplicates(Kind) end},
     {"actual gateway reports malformed Push without partial writes",fun() -> gateway(Kind) end},
     {"direct callback reply budget rejects before queue or dedup mutation",fun() -> callback_budget(Kind) end}].

tables(crawler) -> [crawl_frontier,crawl_seen];
tables(websearch) -> [ws_frontier,ws_seen].
reset(Kind) ->
    [case ets:info(T) of undefined -> ok; _ -> ets:delete(T) end || T <- tables(Kind)],
    ok = frontier:parrot_init().

key(crawler,push) -> <<"bin:crawl/FrontierPush">>;
key(crawler,next) -> <<"bin:crawl/FrontierNext">>;
key(websearch,push) -> <<"bin:ws/Push">>;
key(websearch,next) -> <<"bin:ws/Next">>.

entry(Kind,Id) ->
    Url = <<"https://parrot.dev/",(integer_to_binary(Id))/binary>>,
    Common = <<(byte_size(Url)):32/little,Url/binary,1:16/little>>,
    case Kind of crawler -> <<Id:64/little,Common/binary>>; websearch -> Common end.
push(Kind,Ids) ->
    Data = iolist_to_binary([<<(length(Ids)):32/little>>|[entry(Kind,I) || I <- Ids]]),
    frontier:parrot_service(key(Kind,push),Data).
next(Kind,N) ->
    {_,Bytes} = frontier:parrot_service(key(Kind,next),<<N:32/little>>),
    decode(Kind,Bytes).
decode(Kind,<<N:32/little,Rest/binary>>) -> decode(Kind,N,Rest,[]).
decode(_,0,<<>>,Acc) -> lists:reverse(Acc);
decode(crawler,N,<<Id:64/little,L:32/little,Url:L/binary,D:16/little,Rest/binary>>,Acc) when N > 0 ->
    decode(crawler,N-1,Rest,[{Id,Url,D}|Acc]);
decode(websearch,N,<<L:32/little,Url:L/binary,D:16/little,Rest/binary>>,Acc) when N > 0 ->
    decode(websearch,N-1,Rest,[{Url,D}|Acc]).
expected(Kind,Ids) -> decode(Kind,iolist_to_binary([<<(length(Ids)):32/little>>|[entry(Kind,I) || I <- Ids]])).

state(Kind) -> [lists:sort(ets:tab2list(T)) || T <- tables(Kind)].
reject(F) ->
    Result = try F() of _ -> accepted catch _:_ -> rejected end,
    rejected = Result.

valid(Kind) ->
    Reply = push(Kind,[7,8]),
    case Kind of
        crawler -> {<<"bin:crawl/FrontierAck">>,<<1:32/little>>} = Reply;
        websearch -> {<<"bin:ws/PushAck">>,<<2:32/little>>} = Reply
    end,
    Expected = expected(Kind,[7,8]), Expected = next(Kind,10), [] = next(Kind,10).

truncated(Kind) ->
    Full = iolist_to_binary([<<2:32/little>>,entry(Kind,7),entry(Kind,8)]),
    [begin
        reset(Kind), push(Kind,[99]), Before=state(Kind),
        Prefix = binary:part(Full,0,N),
        reject(fun() -> frontier:parrot_service(key(Kind,push),Prefix) end),
        Before=state(Kind)
     end || N <- lists:seq(0,byte_size(Full)-1)].

malformed(Kind) ->
    One=entry(Kind,7), BadUrl=case Kind of crawler -> <<7:64/little,1:32/little,255,1:16/little>>; websearch -> <<1:32/little,255,1:16/little>> end,
    [begin
        reset(Kind), push(Kind,[99]), Before=state(Kind),
        reject(fun() -> frontier:parrot_service(key(Kind,push),Bytes) end),
        Before=state(Kind)
     end || Bytes <- [<<0:32/little,One/binary>>,<<1:32/little,One/binary,0>>,<<16#FFFFFFFF:32/little,One/binary>>,<<1:32/little,BadUrl/binary>>]].

bad_next(Kind) ->
    push(Kind,[7]), Before=state(Kind),
    [begin
        reject(fun() -> frontier:parrot_service(key(Kind,next),Bytes) end), Before=state(Kind)
     end || Bytes <- [<<>>,<<1>>,<<1,0,0>>,<<1:32/little,0>>]],
    Before=state(Kind), []=next(Kind,0), Before=state(Kind),
    case Kind of
        crawler -> ok;
        websearch ->
            reject(fun() -> frontier:parrot_service(<<"bin:ws/Size">>,<<0>>) end),
            {<<"bin:ws/SizeR">>,<<1:32/little,1:32/little>>} = frontier:parrot_service(<<"bin:ws/Size">>,<<>>)
    end.

concurrent(Kind) ->
    [concurrent_round(Kind) || _ <- lists:seq(1,10)],ok.
concurrent_round(Kind) ->
    reset(Kind),Ids=lists:seq(1,1000),push(Kind,Ids),
    Parent=self(),Tag=make_ref(),
    Workers=[spawn_monitor(fun() ->
        receive {go,Tag} ->
            Value=try next(Kind,1000) catch C:R -> {failed,C,R} end,
            Parent ! {Tag,self(),Value}
        end
    end) || _ <- lists:seq(1,32)],
    [P ! {go,Tag} || {P,_} <- Workers],
    try
        Results=[receive {Tag,P,R} -> R after 5000 -> erlang:error(consumer_timeout) end || {P,_} <- Workers],
        [receive {'DOWN',M,process,P,normal} -> ok after 5000 -> erlang:error(consumer_not_retired) end || {P,M} <- Workers],
        true=lists:all(fun is_list/1,Results),
        Expected=lists:sort(expected(Kind,Ids)),Expected=lists:sort(lists:append(Results)),
        []=next(Kind,1)
    after [begin exit(P,kill),erlang:demonitor(M,[flush]) end || {P,M} <- Workers] end.

duplicates(Kind) ->
    push(Kind,[7,7,8]),push(Kind,[8,9]),
    Expected=lists:sort(expected(Kind,[7,8,9])),Expected=lists:sort(next(Kind,100)),
    push(Kind,[7,8,9]),[]=next(Kind,100).

%% No shared client encoder for application payloads; Wire framing is separate.
gateway(Kind) ->
    {ok,Port}=parrot_gw:start(0,fun frontier:parrot_service/2),
    {ok,S}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}]),
    try
        Body=iolist_to_binary(parrot_gw:handshake_body("frontier-contract")),
        ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,Body)),{2,1,_}=recv(S),
        Entry=entry(Kind,7),
        Bad= <<0:32/little,2:32/little,Entry/binary>>,
        ok=gen_tcp:send(S,parrot_gw:build_frame(16#10,2,<<"/frontier">>,key(Kind,push),Bad)),
        {16#12,2,<<12:16/little,0:16/little,_/binary>>}=recv(S),
        []=next(Kind,100),
        %% A valid request on the same socket still completes with actual state.
        Valid= <<0:32/little,1:32/little,Entry/binary>>,
        ok=gen_tcp:send(S,parrot_gw:build_frame(16#10,3,<<"/frontier">>,key(Kind,push),Valid)),
        {16#11,3,_}=recv(S), Expected=expected(Kind,[7]),Expected=next(Kind,100)
    after gen_tcp:close(S) end.
recv(S) ->
    {ok,<<N:32/little>>}=gen_tcp:recv(S,4,3000),{ok,Body}=gen_tcp:recv(S,N,3000),
    {ok,Ft,_,Cid,_,_,Payload,<<>>}=parrot_gw:parse_frame(<<N:32/little,Body/binary>>),
    {Ft,Cid,Payload}.

unicode_push(Kind) ->
    Before=state(Kind),frontier:parrot_service(key(Kind,push),<<0:32/little>>),Before=state(Kind),
    Url=unicode:characters_to_binary("https://parrot.dev/路径🙂"),
    Common= <<(byte_size(Url)):32/little,Url/binary,9:16/little>>,
    Entry=case Kind of crawler -> <<77:64/little,Common/binary>>; websearch -> Common end,
    Body= <<1:32/little,Entry/binary>>,
    frontier:parrot_service(key(Kind,push),Body),
    {_,Body}=frontier:parrot_service(key(Kind,next),<<1:32/little>>).


%% Reply budget is an independent component API input. A Wire request larger
%% than its negotiated limit cannot be used to reach this callback contract.
callback_budget(Kind) ->
    {ok,{Queue,Seen}=State}=frontier:parrot_init(#{}),
    Snapshot=fun() -> [lists:sort(ets:tab2list(T)) || T <- [Queue,Seen]] end,
    Refuse=fun(Key,Payload) ->
        rejected = try frontier:parrot_service(Key,Payload,State,#{reply_bytes=>4}) of
            _ -> accepted
        catch error:component_overloaded -> rejected end
    end,
    Full=#{reply_bytes=>1048576-28},
    Data=iolist_to_binary([<<1:32/little>>,entry(Kind,7)]),
    try
        Empty=Snapshot(),
        Refuse(key(Kind,push),Data), Empty=Snapshot(),
        Refuse(key(Kind,push),Data), Empty=Snapshot(),
        {reply,_,State}=frontier:parrot_service(key(Kind,push),Data,State,Full),
        Queued=Snapshot(),
        Refuse(key(Kind,next),<<1:32/little>>), Queued=Snapshot(),
        Refuse(key(Kind,next),<<0:32/little>>), Queued=Snapshot(),
        {reply,{_,Batch},State}=frontier:parrot_service(key(Kind,next),<<1:32/little>>,State,Full),
        Expected=expected(Kind,[7]), Expected=decode(Kind,Batch),
        {reply,{_,<<0:32/little>>},State}=frontier:parrot_service(key(Kind,next),<<1:32/little>>,State,Full)
    after ets:delete(Queue),ets:delete(Seen) end.
