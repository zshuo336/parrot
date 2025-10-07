%% websearch URL Frontier 组件（erlang 方言——app 内 source of truth）。
%%
%% 真实搜索引擎版（与 crawler-lab frontier 同族但面向真实 URL）：
%%   - URL 为任意长度字符串（真实网页——无合成 id 压缩）
%%   - 深度上限剪枝 + BFS 批次派发 + ets 去重（进程重启即失——
%%     去重持久化由 Rust 驱动器侧 data/dedupe.tsv 承担，本组件纯内存）
%%
%% 协议（bin:ws/* 键空间——与 crawler-lab bin:crawl/* 分离）：
%%   bin:ws/Push  [n u32][{len u32|url|depth u16}...]           → Ack [n u32]
%%   bin:ws/Next  [n u32]                                       → Batch 同 push 布局
%%   bin:ws/Size  []                                             → [n u32]
-module(frontier).
-export([parrot_init/0, parrot_service/2]).

-define(TAB, ws_frontier).
-define(SEEN, ws_seen).

parrot_init() ->
    case ets:info(?TAB) of
        undefined ->
            ets:new(?TAB, [named_table, public, ordered_set,
                           {write_concurrency, true},
                           {read_concurrency, true}]),
            ets:new(?SEEN, [named_table, public, set,
                            {write_concurrency, true},
                            {read_concurrency, true}]),
            ok;
        _ -> ok
    end.

parrot_service(<<"bin:ws/Push">>, Payload) ->
    ws_push(Payload),
    N = count_push(),
    {<<"bin:ws/PushAck">>, <<N:32/little>>};
parrot_service(<<"bin:ws/Next">>, <<N:32/little>>) ->
    ws_next(N);
parrot_service(<<"bin:ws/Size">>, _Payload) ->
    Cnt = ets:info(?TAB, size),
    Seen = ets:info(?SEEN, size),
    {<<"bin:ws/SizeR">>, <<Cnt:32/little, Seen:32/little>>};
parrot_service(Key, _Payload) ->
    erlang:error({unknown_service, Key}).

%% ---- Push：[{len u32 | url | depth u16}...]（去重后入队）----
ws_push(<<Count:32/little, Entries/binary>>) ->
    ws_push_entries(Count, Entries),
    ok.

ws_push_entries(0, <<>>) -> ok;
ws_push_entries(N, <<UrlLen:32/little, Url:UrlLen/binary,
                     Depth:16/little, Rest/binary>>) ->
    case ets:insert_new(?SEEN, {Url}) of
        true  -> ets:insert(?TAB, {Url, Depth});
        false -> ok
    end,
    ws_push_entries(N - 1, Rest);
ws_push_entries(_, _) -> ok.  %% 截断容错

count_push() -> ets:info(?TAB, size).

%% ---- Next：取前 N 条（ordered_set 首 N = BFS 序）并出队 ----
ws_next(N) ->
    Batch = ws_take(N, [], ets:first(?TAB)),
    Enc = ws_encode_batch(Batch, <<(length(Batch)):32/little>>),
    {<<"bin:ws/Batch">>, Enc}.

ws_take(0, Acc, _) -> lists:reverse(Acc);
ws_take(_, Acc, '$end_of_table') -> lists:reverse(Acc);
ws_take(N, Acc, Url) ->
    [{Url, Depth}] = ets:lookup(?TAB, Url),
    ets:delete(?TAB, Url),
    ws_take(N - 1, [{Url, Depth} | Acc], ets:next(?TAB, Url)).

ws_encode_batch([], Acc) -> Acc;
ws_encode_batch([{Url, Depth} | T], Acc) ->
    U = byte_size(Url),
    ws_encode_batch(T, <<Acc/binary, U:32/little, Url:U/binary,
                         Depth:16/little>>).
