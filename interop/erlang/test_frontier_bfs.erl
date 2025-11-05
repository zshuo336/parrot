%% Independent depth/FIFO vectors, not implementation-shaped ETS assertions.
-module(test_frontier_bfs).
-export([run/1, all/1]).

run(Kind) ->
    App=case Kind of crawler -> "crawler-lab"; websearch -> "websearch" end,
    Source="../../apps/"++App++"/erlang/frontier.erl",
    {ok,frontier,Beam}=compile:file(Source,[binary,report_errors,report_warnings]),
    {module,frontier}=code:load_binary(frontier,Source,Beam),
    Failed=lists:foldl(fun({Name,Test},N) ->
        reset(Kind),
        try Test(),io:format("FRONTIER_BFS_OK ~p ~s~n",[Kind,Name]),N
        catch C:R -> io:format("FRONTIER_BFS_FAIL ~p ~s: ~p:~p~n",[Kind,Name,C,R]),N+1
        after reset(Kind) end
    end,0,all(Kind)),
    case Failed of 0 -> ok; _ -> erlang:error({bfs_cases_failed,Failed}) end.

all(Kind) ->
    [{"depth before identifier with one layer per batch",fun() ->
        push(Kind,[{9,2},{8,0},{7,1},{6,0},{5,2}]),
        [{8,0},{6,0}]=next(Kind,100),
        [{7,1}]=next(Kind,100),
        [{9,2},{5,2}]=next(Kind,100),[]=next(Kind,1)
     end},
     {"same layer FIFO across pushes and partial reads",fun() ->
        push(Kind,[{9,1},{3,1}]),push(Kind,[{7,1},{1,1}]),
        [{9,1}]=next(Kind,1),[{3,1},{7,1}]=next(Kind,2),[{1,1}]=next(Kind,2)
     end},
     {"zero and maximum depth and first accepted duplicate",fun() ->
        push(Kind,[{9,65535},{9,0},{8,65534},{7,0}]),
        []=next(Kind,0),[{7,0}]=next(Kind,4),[{8,65534}]=next(Kind,4),[{9,65535}]=next(Kind,4),
        push(Kind,[{9,0}]),[]=next(Kind,10)
     end},
     {"Push acknowledgement counts this batch despite queued work",fun() ->
        push(Kind,[{7,0},{8,0},{9,0}]),
        Reply=push(Kind,[{8,0},{5,1}]),
        case Kind of
            websearch ->
                {<<"bin:ws/PushAck">>,<<1:32/little>>}=Reply,
                {<<"bin:ws/PushAck">>,<<0:32/little>>}=push(Kind,[]),
                {<<"bin:ws/PushAck">>,<<0:32/little>>}=push(Kind,[{7,0},{5,1}]);
            crawler -> {<<"bin:crawl/FrontierAck">>,<<1:32/little>>}=Reply
        end
     end},
     {"old nonempty queue is rejected without deleting work",fun() ->
        [Queue|_]=tables(Kind),
        Old=case Kind of crawler -> {42,<<"42">>,1}; websearch -> {<<"42">>,1} end,
        true=ets:insert(Queue,Old),
        rejected=try frontier:parrot_init(),accepted catch error:frontier_queue_requires_restart -> rejected end,
        [Old]=ets:tab2list(Queue)
     end}].

tables(crawler) -> [crawl_frontier,crawl_seen];
tables(websearch) -> [ws_frontier,ws_seen].
reset(Kind) ->
    [case ets:info(T) of undefined -> ok; _ -> ets:delete(T) end || T <- tables(Kind)],
    ok=frontier:parrot_init().
key(crawler,push) -> <<"bin:crawl/FrontierPush">>;
key(crawler,next) -> <<"bin:crawl/FrontierNext">>;
key(websearch,push) -> <<"bin:ws/Push">>;
key(websearch,next) -> <<"bin:ws/Next">>.
push(Kind,Items) ->
    Entries=[begin
        Url=integer_to_binary(Id),Common= <<(byte_size(Url)):32/little,Url/binary,Depth:16/little>>,
        case Kind of crawler -> <<Id:64/little,Common/binary>>; websearch -> Common end
    end || {Id,Depth} <- Items],
    frontier:parrot_service(key(Kind,push),iolist_to_binary([<<(length(Items)):32/little>>|Entries])).
next(Kind,N) ->
    {_,<<Count:32/little,Data/binary>>}=frontier:parrot_service(key(Kind,next),<<N:32/little>>),
    decode(Kind,Count,Data,[]).
decode(_,0,<<>>,Acc) -> lists:reverse(Acc);
decode(crawler,N,<<Id:64/little,L:32/little,_Url:L/binary,D:16/little,Rest/binary>>,Acc) ->
    decode(crawler,N-1,Rest,[{Id,D}|Acc]);
decode(websearch,N,<<L:32/little,Url:L/binary,D:16/little,Rest/binary>>,Acc) ->
    decode(websearch,N-1,Rest,[{binary_to_integer(Url),D}|Acc]).
