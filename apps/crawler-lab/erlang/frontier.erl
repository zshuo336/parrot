%% crawler-lab URL Frontier 组件（erlang 方言——app 内 source of truth）。
%%
%% R1（应用体系架构纠正）：本模块是 apps/crawler-lab 的业务代码，
%% 经 parrot 标准包分发：crawler.app.toml 声明 artifact = { Beam =
%% { app = "frontier" } }，erl 网关 deploy 时在独立子 VM 中载入本
%% beam 及所属目录的部署副本。
%%
%% 组件契约（erl 方言 deploy 载入点）：parrot_service/2 返回该组件的
%% 服务映射（网关 service 分发优先查已部署组件——网关内置探针之外）。
%%
%% 协议（与 Rust hub 逐字节对齐——run_regression golden 锚定）：
%%   bin:crawl/FrontierPush [n u32][{id u64|len u32|url|depth u16}...] → FrontierAck [1 u32]
%%   bin:crawl/FrontierNext [n u32]                                    → FrontierBatch 同构批次
-module(frontier).
-export([parrot_init/0, parrot_service/2, parrot_init/1, parrot_service/3, parrot_service/4]).

%% 组件初始化（deploy 后网关首问前调用——幂等）。
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
    global:trans({{frontier_legacy,crawl_frontier,crawl_seen},self()},fun() ->
        case {ets:info(crawl_frontier),ets:info(crawl_seen)} of
            {undefined,undefined} ->
                ets:new(crawl_frontier,[named_table,public,ordered_set]),
                ets:new(crawl_seen,[named_table,public,set]),
                new_budget(crawl_seen,default_limits());
            {undefined,_} -> erlang:error(frontier_queue_requires_restart);
            {_,undefined} -> erlang:error(frontier_queue_requires_restart);
            _ -> ensure_bfs_queue(),ensure_budget(crawl_frontier,crawl_seen)
        end
    end,[node()]).

%% 服务映射：type_key → {ReplyKey, ReplyBody}。
parrot_service(Key,Payload) -> with_frontier(Key,Payload,crawl_frontier,crawl_seen,16777216).
frontier_service(<<"bin:crawl/FrontierPush">>,Payload,Tab,Seen,ReplyBytes) ->
    case ReplyBytes>=byte_size(<<"bin:crawl/FrontierAck">>)+4 of true -> ok; false -> erlang:error(component_overloaded) end,
    crawl_push(Payload,Tab,Seen),{<<"bin:crawl/FrontierAck">>,<<1:32/little>>};
frontier_service(<<"bin:crawl/FrontierNext">>,Payload,Tab,Seen,ReplyBytes) -> crawl_next(Payload,Tab,Seen,ReplyBytes);
frontier_service(Key,_,_,_,_) -> erlang:error({unknown_service,Key}).

%% ── 内部实现：层级优先键 + 同层入队序号，Next 不跨层 ──

%% Validate the entire request before changing either queue or dedup state.
%% A well-formed prefix of a truncated batch must not become partial success.
crawl_push(<<Count:32/little, Entries/binary>>,Tab,Seen) when Count=<65536,byte_size(Entries)=<16777212 ->
    Batch=crawl_decode(Count,Entries,[]),
    Fresh=crawl_fresh(Batch,Seen,#{},[]),
    MaxEntry=16777216-28-byte_size(<<"bin:crawl/FrontierBatch">>)-4,
    case lists:all(fun({_,U,_})->14+byte_size(U)=<MaxEntry end,Fresh) of
        true -> ok; false -> erlang:error(component_overloaded)
    end,
    QAdded=lists:sum([14+byte_size(U)||{_,U,_}<-Fresh]),
    NextBudget=preflight(Tab,Seen,length(Fresh),QAdded,8*length(Fresh)),
    Copies=[{I,binary:copy(U),D}||{I,U,D}<-Fresh],
    ok=reserve(Seen,NextBudget),
    [begin
        ets:insert(Seen,{Id}),Sequence=erlang:unique_integer([monotonic,positive]),
        ets:insert(Tab,{{Depth,Sequence},Id,Url,Depth})
     end||{Id,Url,Depth}<-Copies],
    ok;
crawl_push(<<_:32/little,_/binary>>,_,_) -> erlang:error(component_overloaded);
crawl_push(_,_,_) -> erlang:error(malformed_frontier_push).
crawl_fresh([],_,_,Acc) -> lists:reverse(Acc);
crawl_fresh([{Id,_,_}=Entry|Rest],Seen,Local,Acc) ->
    case maps:is_key(Id,Local) orelse ets:member(Seen,Id) of
        true -> crawl_fresh(Rest,Seen,Local,Acc);
        false -> crawl_fresh(Rest,Seen,Local#{Id=>true},[Entry|Acc])
    end.

crawl_decode(0, <<>>, Acc) -> lists:reverse(Acc);
crawl_decode(N, <<Id:64/little, UrlLen:32/little, Url:UrlLen/binary,
                  Depth:16/little, Rest/binary>>, Acc) when N > 0 ->
    case unicode:characters_to_binary(Url,utf8,utf8) of
        B when is_binary(B) -> crawl_decode(N-1,Rest,[{Id,Url,Depth}|Acc]);
        _ -> erlang:error(invalid_frontier_url_utf8)
    end;
crawl_decode(_, _, _) -> erlang:error(malformed_frontier_push).

crawl_next(<<N:32/little>>,Tab,Seen,ReplyBytes) ->
    Remaining=ReplyBytes-byte_size(<<"bin:crawl/FrontierBatch">>)-4,
    case Remaining>=0 of true -> ok; false -> erlang:error(component_overloaded) end,
    Batch = crawl_take(N, undefined, [],Tab,Seen,Remaining),
    Enc = iolist_to_binary([<<(length(Batch)):32/little>> |
        [[<<Id:64/little,(byte_size(Url)):32/little>>,Url,<<Depth:16/little>>]
         || {Id,Url,Depth} <- Batch]]),
    {<<"bin:crawl/FrontierBatch">>, Enc};
crawl_next(_,_,_,_) -> erlang:error(malformed_frontier_next).

%% A batch contains only the shallowest available layer. Queue ties follow
%% acceptance sequence, never URL or numeric document identifier ordering.
%% Inspect the head before destructive transfer; a partial reply is a FIFO
%% prefix and never skips a larger head to expose later work or deeper layers.
crawl_take(0, _Layer, Acc,_Tab,_Seen,_Remaining) -> lists:reverse(Acc);
crawl_take(N, Layer, Acc,Tab,Seen,Remaining) ->
    case ets:first(Tab) of
        '$end_of_table' -> lists:reverse(Acc);
        {Depth,_}=Key when Layer=:=undefined; Layer=:=Depth ->
            case ets:lookup(Tab,Key) of
                [] -> crawl_take(N,Layer,Acc,Tab,Seen,Remaining);
                [{Key,Id,Url,Depth}] ->
                    Size=14+byte_size(Url),
                    case Size=<Remaining of
                        true ->
                            case ets:take(Tab,Key) of
                                [] -> crawl_take(N,Layer,Acc,Tab,Seen,Remaining);
                                [{Key,Id,Url,Depth}] ->
                                    ok=release_queue(Seen,Size),
                                    crawl_take(N-1,Depth,[{Id,Url,Depth}|Acc],Tab,Seen,Remaining-Size)
                            end;
                        false when Acc=:=[] -> erlang:error(component_overloaded);
                        false -> lists:reverse(Acc)
                    end
            end;
        {_,_} -> lists:reverse(Acc);
        _ -> erlang:error(frontier_queue_requires_restart)
    end.

ensure_bfs_queue() ->
    Count=ets:select_count(crawl_frontier,[{{{'$1','$2'},'_','_','$1'},[],[true]}]),
    case Count=:=ets:info(crawl_frontier,size) of
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
