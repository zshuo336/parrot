%% Inspect real retained binaries and race actual legacy producers at capacity.
-module(test_frontier_capacity_storage).
-export([run/1]).
run(Kind) ->
    App=case Kind of crawler -> "crawler-lab"; websearch -> "websearch" end,
    Source="../../apps/"++App++"/erlang/frontier.erl",
    {ok,frontier,Beam}=compile:file(Source,[binary,report_errors,report_warnings]),
    {module,frontier}=code:load_binary(frontier,Source,Beam),
    Failed=lists:foldl(fun({Name,F},Count)->
        try F(),io:format("FRONTIER_STORAGE_OK ~p ~s~n",[Kind,Name]),Count
        catch C:R -> io:format("FRONTIER_STORAGE_FAIL ~p ~s: ~p:~p~n",[Kind,Name,C,R]),Count+1 end
    end,0,[{"retained short URLs do not pin a large duplicate request",fun()->retention(Kind) end},
           {"32 competing legacy producers cannot overbook the final slot",fun()->concurrent(Kind) end},
           {"individual entries must fit the largest encodable reply",fun()->maximum_entry(Kind) end}]),
    case Failed of 0 -> ok; _ -> erlang:error({frontier_storage_cases_failed,Failed}) end.
retention(Kind) ->
    {ok,{Q,S}=State}=frontier:parrot_init(#{}),
    try
        Big=binary:copy(<<"b">>,500000),Short=binary:copy(<<"s">>,128),
        {reply,_,State}=frontier:parrot_service(key(Kind,push),batch(Kind,[{1,Big}]),State),
        {reply,_,State}=frontier:parrot_service(key(Kind,next),<<1:32/little>>,State),
        {reply,_,State}=frontier:parrot_service(key(Kind,push),batch(Kind,[{2,Short},{1,Big}]),State),
        [Row]=ets:tab2list(Q),Url=case Kind of crawler -> element(3,Row); websearch -> element(2,Row) end,
        Size=byte_size(Url),Ref=binary:referenced_byte_size(Url),
        io:format("FRONTIER_RETAINED_BINARY ~p logical=~p referenced=~p~n",[Kind,Size,Ref]),
        true=Ref=<Size,
        {reply,_,State}=frontier:parrot_service(key(Kind,next),<<1:32/little>>,State),
        case Kind of
            websearch -> [{Stored}]=ets:lookup(S,Short),true=binary:referenced_byte_size(Stored)=<byte_size(Stored);
            crawler -> ok
        end
    after ets:delete(Q),ets:delete(S) end.
concurrent(Kind) ->
    [Tab,Seen]=case Kind of crawler -> [crawl_frontier,crawl_seen]; websearch -> [ws_frontier,ws_seen] end,
    [case ets:info(T) of undefined -> ok; _ -> ets:delete(T) end||T<-[Tab,Seen]],
    ok=frontier:parrot_init(),
    try
        frontier:parrot_service(key(Kind,push),batch(Kind,[{I,integer_to_binary(I)}||I<-lists:seq(1,65535)])),
        Parent=self(),Ref=make_ref(),
        Workers=[spawn_monitor(fun()->receive {go,Ref}->
            Result=try frontier:parrot_service(key(Kind,push),batch(Kind,[{I,integer_to_binary(I)}])),accepted
                   catch error:component_overloaded->overloaded end,
            Parent!{Ref,self(),Result}
        end end)||I<-lists:seq(65536,65567)],
        [P!{go,Ref}||{P,_}<-Workers],
        try
            Results=[receive {Ref,P,R}->R after 10000->erlang:error(producer_timeout) end||{P,_}<-Workers],
            [receive {'DOWN',M,process,P,normal}->ok after 1000->erlang:error(producer_not_retired) end||{P,M}<-Workers],
            Accepted=length([ok||accepted<-Results]),Queued=ets:info(Tab,size),
            io:format("FRONTIER_COMPETING_PRODUCERS ~p accepted=~p queued=~p~n",[Kind,Accepted,Queued]),
            1=Accepted,65536=Queued,
            frontier:parrot_service(key(Kind,next),<<1:32/little>>),
            frontier:parrot_service(key(Kind,push),batch(Kind,[{70000,<<"new">>}])),65536=ets:info(Tab,size)
        after [begin exit(P,kill),demonitor(M,[flush]) end||{P,M}<-Workers] end
    after ets:delete(Tab),ets:delete(Seen) end.
key(crawler,push)-><<"bin:crawl/FrontierPush">>;
key(crawler,next)-><<"bin:crawl/FrontierNext">>;
key(websearch,push)-><<"bin:ws/Push">>;
key(websearch,next)-><<"bin:ws/Next">>.
batch(Kind,Entries) ->
    iolist_to_binary([<<(length(Entries)):32/little>>|[begin
        Common= <<(byte_size(U)):32/little,U/binary,0:16/little>>,
        case Kind of crawler-><<I:64/little,Common/binary>>; websearch->Common end
    end||{I,U}<-Entries]]).

maximum_entry(Kind) ->
    {ok,{Q,S}=State}=frontier:parrot_init(#{}),
    try
        Key=case Kind of crawler-><<"bin:crawl/FrontierBatch">>; websearch-><<"bin:ws/Batch">> end,
        EntryOverhead=case Kind of crawler->14;websearch->6 end,
        UrlSize=16777216-28-byte_size(Key)-4-EntryOverhead,
        TooLarge=binary:copy(<<"x">>,UrlSize+1),
        overloaded=try frontier:parrot_service(key(Kind,push),batch(Kind,[{1,TooLarge}]),State),accepted
                   catch error:component_overloaded->overloaded end,
        {reply,{Key,<<0:32/little>>},State}=frontier:parrot_service(key(Kind,next),<<1:32/little>>,State),
        Exact=binary:copy(<<"e">>,UrlSize),
        {reply,_,State}=frontier:parrot_service(key(Kind,push),batch(Kind,[{2,Exact}]),State),
        {reply,{Key,Body},State}=frontier:parrot_service(key(Kind,next),<<1:32/little>>,State),
        16777216=28+byte_size(Key)+byte_size(Body)
    after ets:delete(Q),ets:delete(S) end.
