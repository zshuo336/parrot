%% websearch URL Frontier 组件（erlang 方言——app 内 source of truth）。
%%
%% 真实搜索引擎版（与 crawler-lab frontier 同族但面向真实 URL）：
%%   - URL 为任意长度字符串（真实网页——无合成 id 压缩）
%%   - depth/sequence 最浅已排队层批次派发 + ets 去重（进程重启即失——
%%     去重持久化由 Rust 驱动器侧 data/dedupe.tsv 承担，本组件纯内存）
%%
%% 协议（bin:ws/* 键空间——与 crawler-lab bin:crawl/* 分离）：
%%   bin:ws/Push  [n u32][{len u32|url|depth u16}...]           → Ack [本次新增 n u32]
%%   bin:ws/Next  [n u32]                                       → Batch 同 push 布局
%%   bin:ws/Size  []                                             → [n u32]
-module(frontier).
-export([parrot_init/0, parrot_service/2, parrot_init/1, parrot_service/3, parrot_service/4]).

-define(TAB, ws_frontier).
-define(SEEN, ws_seen).

%% Production instances own private ETS tables in their serial actor process.
%% The zero-argument API is retained for explicit standalone legacy use/tests.
parrot_init(Context) ->
    Limits=frontier_limits(maps:get(config,Context,undefined)),
    Queue=ets:new(frontier_queue,[ordered_set,private]),
    Seen=ets:new(frontier_seen,[set,private]),
    ok=new_budget(Seen,Limits),
    {ok,{Queue,Seen}}.
parrot_service(Key,Payload,State) -> parrot_service(Key,Payload,State,#{reply_bytes=>16777216}).
parrot_service(Key,Payload,{Queue,Seen}=State,#{reply_bytes:=ReplyBytes}) ->
    {reply,with_frontier(Key,Payload,Queue,Seen,ReplyBytes),State}.

parrot_init() ->
    global:trans({{frontier_legacy,ws_frontier,ws_seen},self()},fun() ->
        case {ets:info(ws_frontier),ets:info(ws_seen)} of
            {undefined,undefined} ->
                ets:new(ws_frontier,[named_table,public,ordered_set]),
                ets:new(ws_seen,[named_table,public,set]),
                new_budget(ws_seen,default_limits());
            {undefined,_} -> erlang:error(frontier_queue_requires_restart);
            {_,undefined} -> erlang:error(frontier_queue_requires_restart);
            _ -> ensure_bfs_queue(),ensure_budget(ws_frontier,ws_seen)
        end
    end,[node()]).

parrot_service(Key,Payload) -> with_frontier(Key,Payload,?TAB,?SEEN,16777216).
frontier_service(<<"bin:ws/Push">>,Payload,Tab,Seen,ReplyBytes) ->
    case ReplyBytes>=byte_size(<<"bin:ws/PushAck">>)+4 of true -> ok; false -> erlang:error(component_overloaded) end,
    N=ws_push(Payload,Tab,Seen),{<<"bin:ws/PushAck">>,<<N:32/little>>};
frontier_service(<<"bin:ws/Next">>,Payload,Tab,Seen,ReplyBytes) -> ws_next(Payload,Tab,Seen,ReplyBytes);
frontier_service(<<"bin:ws/Size">>,<<>>,Tab,Seen,_ReplyBytes) ->
    N=ets:info(Tab,size),S=ets:info(Seen,size)-1,{<<"bin:ws/SizeR">>,<<N:32/little,S:32/little>>};
frontier_service(<<"bin:ws/Size">>,_,_,_,_) -> erlang:error(malformed_frontier_size);
frontier_service(Key,_,_,_,_) -> erlang:error({unknown_service,Key}).

%% ---- Push：[{len u32 | url | depth u16}...]（去重后入队）----
%% Full validation precedes every queue/dedup mutation; malformed tails fail.
ws_push(<<Count:32/little, Entries/binary>>,Tab,Seen) when Count=<65536,byte_size(Entries)=<16777212 ->
    Batch=ws_decode(Count,Entries,[]),
    Fresh=ws_fresh(Batch,Seen,#{},[]),
    MaxEntry=16777216-28-byte_size(<<"bin:ws/Batch">>)-4,
    case lists:all(fun({U,_})->6+byte_size(U)=<MaxEntry end,Fresh) of
        true -> ok; false -> erlang:error(component_overloaded)
    end,
    QAdded=lists:sum([6+byte_size(U)||{U,_}<-Fresh]),
    SAdded=lists:sum([4+byte_size(U)||{U,_}<-Fresh]),
    NextBudget=preflight(Tab,Seen,length(Fresh),QAdded,SAdded),
    %% Do not pin the entire input frame through a tiny retained sub-binary.
    Copies=[{binary:copy(U),D}||{U,D}<-Fresh],
    ok=reserve(Seen,NextBudget),
    [begin
        ets:insert(Seen,{Url}),Sequence=erlang:unique_integer([monotonic,positive]),
        ets:insert(Tab,{{Depth,Sequence},Url,Depth})
     end||{Url,Depth}<-Copies],
    length(Copies);
ws_push(<<_:32/little,_/binary>>,_,_) -> erlang:error(component_overloaded);
ws_push(_,_,_) -> erlang:error(malformed_frontier_push).
ws_fresh([],_,_,Acc) -> lists:reverse(Acc);
ws_fresh([{Url,_}=Entry|Rest],Seen,Local,Acc) ->
    case maps:is_key(Url,Local) orelse ets:member(Seen,Url) of
        true -> ws_fresh(Rest,Seen,Local,Acc);
        false -> ws_fresh(Rest,Seen,Local#{Url=>true},[Entry|Acc])
    end.

ws_decode(0,<<>>,Acc) -> lists:reverse(Acc);
ws_decode(N,<<UrlLen:32/little,Url:UrlLen/binary,Depth:16/little,Rest/binary>>,Acc) when N > 0 ->
    case unicode:characters_to_binary(Url,utf8,utf8) of
        B when is_binary(B) -> ws_decode(N-1,Rest,[{Url,Depth}|Acc]);
        _ -> erlang:error(invalid_frontier_url_utf8)
    end;
ws_decode(_,_,_) -> erlang:error(malformed_frontier_push).


%% Depth is the primary queue key; a monotonic sequence preserves FIFO ties.
%% Only the shallowest available layer is transferred by each Next request.
ws_next(<<N:32/little>>,Tab,Seen,ReplyBytes) ->
    Remaining=ReplyBytes-byte_size(<<"bin:ws/Batch">>)-4,
    case Remaining>=0 of true -> ok; false -> erlang:error(component_overloaded) end,
    Batch = ws_take(N, undefined, [],Tab,Seen,Remaining),
    Enc = iolist_to_binary([<<(length(Batch)):32/little>> |
        [[<<(byte_size(Url)):32/little>>,Url,<<Depth:16/little>>]
         || {Url,Depth} <- Batch]]),
    {<<"bin:ws/Batch">>, Enc};
ws_next(_,_,_,_) -> erlang:error(malformed_frontier_next).

%% Inspect the head before destructive transfer; a partial reply is a FIFO
%% prefix and never skips a larger head to expose later work or deeper layers.
ws_take(0, _Layer, Acc,_Tab,_Seen,_Remaining) -> lists:reverse(Acc);
ws_take(N, Layer, Acc,Tab,Seen,Remaining) ->
    case ets:first(Tab) of
        '$end_of_table' -> lists:reverse(Acc);
        {Depth,_}=Key when Layer=:=undefined; Layer=:=Depth ->
            case ets:lookup(Tab,Key) of
                [] -> ws_take(N,Layer,Acc,Tab,Seen,Remaining);
                [{Key,Url,Depth}] ->
                    Size=6+byte_size(Url),
                    case Size=<Remaining of
                        true ->
                            case ets:take(Tab,Key) of
                                [] -> ws_take(N,Layer,Acc,Tab,Seen,Remaining);
                                [{Key,Url,Depth}] ->
                                    ok=release_queue(Seen,Size),
                                    ws_take(N-1,Depth,[{Url,Depth}|Acc],Tab,Seen,Remaining-Size)
                            end;
                        false when Acc=:=[] -> erlang:error(component_overloaded);
                        false -> lists:reverse(Acc)
                    end
            end;
        {_,_} -> lists:reverse(Acc);
        _ -> erlang:error(frontier_queue_requires_restart)
    end.

ensure_bfs_queue() ->
    Count=ets:select_count(?TAB,[{{{'$1','$2'},'_','$1'},[],[true]}]),
    case Count=:=ets:info(?TAB,size) of
        true -> ok;
        false -> erlang:error(frontier_queue_requires_restart)
    end.

%% Limits are logical retained bytes plus independent item counts, not an RSS cap.
%% Config is a deliberately small flat integer TOML subset; unknown syntax is
%% rejected, never silently ignored. The app manifest serializer emits this form.
frontier_limits(undefined) -> default_limits();
frontier_limits(Config) when is_binary(Config),byte_size(Config)=<4096 ->
    true=is_binary(unicode:characters_to_binary(Config,utf8,utf8)),
    Parsed=lists:foldl(fun(Line,Acc) ->
        [Text|_]=binary:split(Line,<<"#">>),
        case string:trim(Text) of
            <<>> -> Acc;
            Value ->
                {match,[Key,Digits]}=re:run(Value,
                    <<"^([a-z_]+)[ \\t]*=[ \\t]*(\\+?(?:0|[1-9][0-9]*(?:_[0-9]+)*))[ \\t]*$">>,
                    [{capture,[1,2],binary}]),
                Field=limit_key(Key),false=maps:is_key(Field,Acc),
                Number=binary_to_integer(binary:replace(Digits,<<"_">>,<<>>,[global])),
                true=Number>0 andalso Number=<maps:get(Field,maximum_limits()),
                Acc#{Field=>Number}
        end
    end,#{},binary:split(Config,<<"\n">>,[global])),
    maps:merge(default_limits(),Parsed);
frontier_limits(_) -> erlang:error(invalid_frontier_limits).
limit_key(<<"max_queued_items">>) -> max_queued_items;
limit_key(<<"max_queued_bytes">>) -> max_queued_bytes;
limit_key(<<"max_seen_items">>) -> max_seen_items;
limit_key(<<"max_seen_bytes">>) -> max_seen_bytes;
limit_key(_) -> erlang:error(unknown_frontier_limit).
default_limits() -> #{max_queued_items=>65536,max_queued_bytes=>16777216,
                      max_seen_items=>262144,max_seen_bytes=>16777216}.
maximum_limits() -> #{max_queued_items=>65536,max_queued_bytes=>67108864,
                      max_seen_items=>262144,max_seen_bytes=>67108864}.
new_budget(Seen,Limits) -> ets:insert(Seen,{'$frontier_budget_v1',Limits,0,0}),ok.
ensure_budget(Tab,Seen) ->
    case ets:lookup(Seen,'$frontier_budget_v1') of
        [{'$frontier_budget_v1',L,Q,S}] when is_map(L),is_integer(Q),Q>=0,is_integer(S),S>=0 -> ok;
        [] -> case ets:info(Tab,size)=:=0 andalso ets:info(Seen,size)=:=0 of
            true -> new_budget(Seen,default_limits());
            false -> erlang:error(frontier_queue_requires_restart)
        end;
        _ -> erlang:error(frontier_queue_requires_restart)
    end.

%% Production tables are private to a serial instance. Explicit legacy tables
%% are public, so all helper operations share a local monitored OTP lock.
%% node() is explicit: this lock never coordinates a distributed queue.
with_frontier(Key,Payload,Tab,Seen,ReplyBytes) ->
    Run=fun()->frontier_service(Key,Payload,Tab,Seen,ReplyBytes) end,
    case ets:info(Seen,protection) of
        private -> Run();
        public -> global:trans({{frontier_legacy,Tab,Seen},self()},Run,[node()]);
        _ -> erlang:error(frontier_queue_requires_restart)
    end.
preflight(Tab,Seen,Count,QAdded,SAdded) ->
    [{'$frontier_budget_v1',Limits,QBytes,SBytes}]=ets:lookup(Seen,'$frontier_budget_v1'),
    Fits=ets:info(Tab,size)+Count=<maps:get(max_queued_items,Limits) andalso
         ets:info(Seen,size)-1+Count=<maps:get(max_seen_items,Limits) andalso
         QBytes+QAdded=<maps:get(max_queued_bytes,Limits) andalso
         SBytes+SAdded=<maps:get(max_seen_bytes,Limits),
    case Fits of true -> {Limits,QBytes+QAdded,SBytes+SAdded}; false -> erlang:error(component_overloaded) end.
reserve(Seen,{Limits,QBytes,SBytes}) ->
    ets:insert(Seen,{'$frontier_budget_v1',Limits,QBytes,SBytes}),ok.
release_queue(Seen,Bytes) ->
    N=ets:update_counter(Seen,'$frontier_budget_v1',{3,-Bytes}),true=N>=0,ok.
