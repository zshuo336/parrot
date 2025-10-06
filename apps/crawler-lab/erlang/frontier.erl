%% crawler-lab URL Frontier 组件（erlang 方言——app 内 source of truth）。
%%
%% R1（应用体系架构纠正）：本模块是 apps/crawler-lab 的业务代码，
%% 经 parrot 标准包分发：crawler.app.toml 声明 artifact = { Beam =
%% { app = "frontier" } }，erl 网关 deploy 时 code:load_abs 载入本
%% beam（artifact dir 已 add_patha 首）。
%%
%% 组件契约（erl 方言 deploy 载入点）：parrot_service/2 返回该组件的
%% 服务映射（网关 service 分发优先查已部署组件——网关内置探针之外）。
%%
%% 协议（与 Rust hub 逐字节对齐——run_regression golden 锚定）：
%%   bin:crawl/FrontierPush [n u32][{id u64|len u32|url|depth u16}...] → FrontierAck [1 u32]
%%   bin:crawl/FrontierNext [n u32]                                    → FrontierBatch 同构批次
-module(frontier).
-export([parrot_init/0, parrot_service/2]).

%% 组件初始化（deploy 后网关首问前调用——幂等）。
parrot_init() ->
    case ets:info(crawl_frontier) of
        undefined ->
            ets:new(crawl_frontier, [named_table, public, ordered_set,
                                     {write_concurrency, true},
                                     {read_concurrency, true}]),
            ets:new(crawl_seen, [named_table, public,
                                 {write_concurrency, true}]),
            ok;
        _ -> ok
    end.

%% 服务映射：type_key → {ReplyKey, ReplyBody}。
parrot_service(<<"bin:crawl/FrontierPush">>, Payload) ->
    crawl_push(Payload),
    {<<"bin:crawl/FrontierAck">>, <<1:32/little>>};
parrot_service(<<"bin:crawl/FrontierNext">>, Payload) ->
    crawl_next(Payload);
parrot_service(Key, _Payload) ->
    erlang:error({unknown_service, Key}).

%% ── 内部实现（自 parrot_gw.erl 原样搬迁——语义不变）──

crawl_push(<<Count:32/little, Entries/binary>>) ->
    crawl_push_entries(Count, Entries),
    ok.

crawl_push_entries(0, <<>>) -> ok;
crawl_push_entries(N, <<Id:64/little, UrlLen:32/little,
                        Url:UrlLen/binary, Depth:16/little, Rest/binary>>) ->
    case ets:insert_new(crawl_seen, {Id}) of
        true  -> ets:insert(crawl_frontier, {Id, Url, Depth});
        false -> ok
    end,
    crawl_push_entries(N - 1, Rest);
crawl_push_entries(_, _) -> ok.  %% 截断容错

crawl_next(<<N:32/little>>) ->
    Batch = crawl_take(N, [], ets:first(crawl_frontier)),
    Enc = crawl_encode_batch(Batch, <<(length(Batch)):32/little>>),
    {<<"bin:crawl/FrontierBatch">>, Enc}.

crawl_take(0, Acc, _) -> lists:reverse(Acc);
crawl_take(_, Acc, '$end_of_table') -> lists:reverse(Acc);
crawl_take(N, Acc, Id) ->
    [{Id, Url, Depth}] = ets:lookup(crawl_frontier, Id),
    ets:delete(crawl_frontier, Id),
    crawl_take(N - 1, [{Id, Url, Depth} | Acc], ets:next(crawl_frontier, Id)).

crawl_encode_batch([], Acc) -> Acc;
crawl_encode_batch([{Id, Url, Depth} | T], Acc) ->
    U = byte_size(Url),
    crawl_encode_batch(T, <<Acc/binary, Id:64/little, U:32/little, Url:U/binary,
                            Depth:16/little>>).
