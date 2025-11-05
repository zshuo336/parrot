%% B4（DEV_09）：admin-v2 erlang 方言测试。
%%
%% 三段：
%%   1. wire 编解码：docs/vectors/admin_v2.json 冻结向量逐字节互锁
%%      （Rust bincode ↔ Erlang 重实现——四方言事实源）
%%   2. 四命令纯逻辑：deploy/stop/drain/status（登记表 + 前缀匹配）
%%   3. 热加载全链：真实网关进程 + socket——test_hot_v1/v2 两个 beam
%%      同名模块先后 Deploy → ASK 行为切换断言（code:load_abs 热替换）
%%
%% 运行（repo 根）：
%%   make check-erl  % strict compilation followed by gateway contracts

-module(test_admin_v2).
-export([run/0, all/0, t_hot_reload/0]).
-export([compile_version/3, gw_connect/1, deploy_frame/4, recv_admin_reply/2, status_frame/3, stop_frame/3, ask_hot/2]).

-define(ASK, 16#10).
-define(REPLY, 16#11).
-define(SYSTEM_EVENT, 16#20).

all() ->
    [{"rejection assertion distinguishes return and exception", fun t_rejection_assertion_contract/0},
     {"admin_v2 golden deploy cmd", fun t_golden_deploy_cmd/0},
     {"admin_v2 golden drain cmd", fun t_golden_drain_cmd/0},
     {"admin_v2 golden stop cmd", fun t_golden_stop_cmd/0},
     {"admin_v2 golden status cmd", fun t_golden_status_cmd/0},
     {"admin_v2 golden deployed reply", fun t_golden_deployed_reply/0},
     {"admin_v2 golden failed reply", fun t_golden_failed_reply/0},
     {"varint boundaries", fun t_varint/0},
     {"cmd roundtrip all kinds", fun t_cmd_roundtrip/0},
     {"reply roundtrip all kinds", fun t_reply_roundtrip/0},
     {"instance paths expansion", fun t_instance_paths/0},
     {"deploy beam registers component", fun t_deploy_logic/0},
     {"deploy dialect mismatch", fun t_deploy_mismatch/0},
     {"initializer failures cannot report Deployed", fun t_init_failure/0},
     {"stop prefix no false match", fun t_stop_prefix/0},
     {"status not found", fun t_status_not_found/0},
     {"hot reload v1 -> v2 behavior switch", fun t_hot_reload/0},
     {"admin correlation rejects before effects", fun() -> t_hot_reload(true) end},
     {"varint numeric range", fun t_varint_range/0},
     {"admin strict UTF-8 decode", fun t_admin_utf8_decode/0},
     {"admin strict UTF-8 encode", fun t_admin_utf8_encode/0},
     {"Failed.code encode u16", fun t_failed_code_encode/0},
     {"Failed.code decode u16", fun t_failed_code_decode/0},
     {"Dylib.abi encode u32", fun t_abi_encode/0},
     {"Dylib.abi decode u32", fun t_abi_decode/0},
     {"typed varint discriminants", fun t_varint_discriminants/0},
     {"all frozen prefixes", fun t_all_frozen_prefixes/0},
     {"vector bounds and order", fun t_vector_bounds/0},
     {"actual malformed admin reply", fun t_malformed_admin_reply/0}].

run() ->
    Fail = lists:foldl(
        fun ({Name, F}, Acc) ->
            try F(), io:format("ok   ~s~n", [Name]), Acc
            catch _:R -> io:format("FAIL ~s: ~p~n", [Name, R]), Acc + 1
            end
        end, 0, all()),
    case Fail of
        0 -> io:format("~nADMIN-V2-ALL-PASS~n");
        N -> io:format("~n~p FAILED~n", [N]), halt(1)
    end.

%% ============ 1. 冻结向量（docs/vectors/admin_v2.json） ============

golden(Name) ->
    [#{<<"payload">>:=Payload}]=[Row || Row <- parrot_test_vectors:admin(),
        maps:get(<<"name">>,Row)=:=Name],
    Payload.

t_golden_deploy_cmd() ->
    Raw = golden(<<"v2-deploy-props-singleton">>),
    {deploy_component, 1, {<<"echo">>, <<"1.0.0">>,
                           {props, <<"app.echo">>}, singleton, undefined}} =
        parrot_gw:decode_admin_cmd_v2(Raw),
    Raw = parrot_gw:encode_admin_cmd_v2(
            {deploy_component, 1, {<<"echo">>, <<"1.0.0">>,
                                   {props, <<"app.echo">>}, singleton, undefined}}).

t_golden_drain_cmd() ->
    Raw = golden(<<"v2-drain">>),
    {drain_component, 2, <<"/user/idx">>, 5000} = parrot_gw:decode_admin_cmd_v2(Raw),
    Raw = parrot_gw:encode_admin_cmd_v2({drain_component, 2, <<"/user/idx">>, 5000}).

t_golden_stop_cmd() ->
    Raw = golden(<<"v2-stop">>),
    {stop_component, 3, <<"/user/idx">>} = parrot_gw:decode_admin_cmd_v2(Raw),
    Raw = parrot_gw:encode_admin_cmd_v2({stop_component, 3, <<"/user/idx">>}).

t_golden_status_cmd() ->
    Raw = golden(<<"v2-status">>),
    {component_status, 4, <<"/user/">>} = parrot_gw:decode_admin_cmd_v2(Raw),
    Raw = parrot_gw:encode_admin_cmd_v2({component_status, 4, <<"/user/">>}).

t_golden_deployed_reply() ->
    Raw = golden(<<"v2-reply-deployed">>),
    {deployed, 1, [<<"/user/echo">>]} = parrot_gw:decode_admin_reply_v2(Raw),
    Raw = parrot_gw:encode_admin_reply_v2({deployed, 1, [<<"/user/echo">>]}).

t_golden_failed_reply() ->
    Raw = golden(<<"v2-reply-failed">>),
    {failed, 9, 16#0A02, <<"no executor">>} = parrot_gw:decode_admin_reply_v2(Raw),
    Raw = parrot_gw:encode_admin_reply_v2({failed, 9, 16#0A02, <<"no executor">>}).

%% ============ 2. 编解码性质 ============

t_varint() ->
    <<0>> = parrot_gw:bc_put_varint(0),
    <<250>> = parrot_gw:bc_put_varint(250),
    <<16#FB, 16#FB, 0>> = parrot_gw:bc_put_varint(251),
    <<16#FB, 16#88, 16#13>> = parrot_gw:bc_put_varint(5000),
    <<16#FC, 16#40, 16#42, 16#0F, 0>> = parrot_gw:bc_put_varint(1000000),
    lists:foreach(fun(V) ->
                      Bin = parrot_gw:bc_put_varint(V),
                      {V, <<>>} = parrot_gw:bc_get_varint(Bin)
                  end, [0, 1, 250, 251, 5000, 65535, 65536, 1 bsl 32]).

t_cmd_roundtrip() ->
    Cmds = [
        {deploy_component, 7, {<<"crawler">>, <<"1.2.0">>,
                               {beam, <<"frontier">>, undefined}, {sharded, 3}, <<"[w]\nc=4">>}},
        {deploy_component, 8, {<<"x">>, <<"1">>,
                               {pymodule, <<"m">>, undefined, undefined}, singleton, undefined}},
        {deploy_component, 9, {<<"x">>, <<"1">>,
                               {jvm, <<"parrot.C">>, <<"file:///a.jar">>, undefined}, {pool, 2}, undefined}},
        {deploy_component, 10, {<<"x">>, <<"1">>,
                                {wasm, <<"sha256:aa">>, <<"file:///a.wasm">>}, singleton, undefined}},
        {deploy_component, 11, {<<"x">>, <<"1">>,
                                {dylib, <<"sha256:bb">>, <<"file:///a.so">>, 1}, singleton, undefined}},
        {drain_component, 12, <<"/u/">>, 99999},
        {stop_component, 13, <<"/u/">>},
        {component_status, 14, <<"/u/">>}
    ],
    lists:foreach(fun(C) ->
                      Bin = parrot_gw:encode_admin_cmd_v2(C),
                      C = parrot_gw:decode_admin_cmd_v2(Bin)
                  end, Cmds).

t_reply_roundtrip() ->
    Replies = [
        {deployed, 1, [<<"/a">>, <<"/b">>, <<"/c">>]},
        {drained, 2, 3, 1},
        {stopped, 3},
        {status, 4, [{<<"/u/x">>, <<"running">>, <<"1.0">>}]},
        {failed, 5, 16#0A06, <<"boom">>}
    ],
    lists:foreach(fun(R) ->
                      Bin = parrot_gw:encode_admin_reply_v2(R),
                      R = parrot_gw:decode_admin_reply_v2(Bin)
                  end, Replies).

t_instance_paths() ->
    [<<"/user/echo">>] = parrot_gw:admin_instance_paths(<<"echo">>, singleton),
    [<<"/user/c-0">>, <<"/user/c-1">>, <<"/user/c-2">>] =
        parrot_gw:admin_instance_paths(<<"c">>, {pool, 3}),
    4 = parrot_gw:instance_count({sharded, 4}),
    1 = parrot_gw:instance_count(singleton).

%% ============ 3. 四命令纯逻辑（ETS 登记表） ============

t_deploy_logic() -> with_status_artifact(fun(Uri) ->
    parrot_gw:admin_init(),
    %% 独立最小组件制品；不把宿主代码或整个工作目录当组件。
    {deployed, [<<"/user/logt">>]} =
        parrot_gw:admin_deploy({<<"logt">>, <<"9.9">>, {beam, <<"test_admin_component">>, Uri}, singleton, undefined}),
    %% 登记表就位 → status 可见
    {status_reply, [{<<"/user/logt">>, <<"running">>, <<"9.9">>}]} =
        parrot_gw:admin_status(<<"/user/logt">>),
    %% 清理
    stopped = parrot_gw:admin_stop(<<"/user/logt">>) end).

t_deploy_mismatch() ->
    parrot_gw:admin_init(),
    {failed, 16#0A02, _} =
        parrot_gw:admin_deploy({<<"x">>, <<"1">>, {props, <<"f">>}, singleton, undefined}).

t_stop_prefix() -> with_status_artifact(fun(Uri) ->
    parrot_gw:admin_init(),
    {deployed, _} = parrot_gw:admin_deploy(
        {<<"n1">>, <<"1">>, {beam, <<"test_admin_component">>, Uri}, singleton, undefined}),
    {deployed, _} = parrot_gw:admin_deploy(
        {<<"n1x">>, <<"1">>, {beam, <<"test_admin_component">>, Uri}, singleton, undefined}),
    stopped = parrot_gw:admin_stop(<<"/user/n1">>),
    %% /user/n1x 未被误停（status 仍可见）
    {status_reply, [_]} = parrot_gw:admin_status(<<"/user/n1x">>),
    stopped = parrot_gw:admin_stop(<<"/user/n1x">>) end).

t_status_not_found() ->
    parrot_gw:admin_init(),
    {failed, 16#0A03, _} = parrot_gw:admin_status(<<"/user/never">>).

%% ============ 4. 热加载全链（真实网关 + 行为切换断言） ============
%%
%% fixture：test_hot_v1.erl / test_hot_v2.erl——同名模块 test_hot 两版本
%% （parrot_service/2 返回辨识值 111 / 222）。测试运行前由本模块动态生成源码
%% + compile:file 编译到临时目录 → PARROT_ARTIFACT_DIR 指向 →
%% Deploy 部署 v1 → ASK 111 → 再编译 v2 → Deploy → ASK 222（热切换）。

t_hot_reload() -> t_hot_reload(false).

t_hot_reload(CheckIds) ->
    %% 1. 生成两版 beam 到 artifact dir
    Dir = filename:join("/tmp", "parrot-hot-contract-" ++ os:getpid() ++ "-" ++
        integer_to_list(erlang:unique_integer([positive]))),
    PreviousDir = os:getenv("PARROT_ARTIFACT_DIR"),
    filelib:ensure_dir(filename:join(Dir, "x")),
    compile_version(Dir, 1, 111),
    %% 2. 起网关（artifact dir 经 env 注入——admin_deploy add_patha 消费）
    os:putenv("PARROT_ARTIFACT_DIR", Dir),
    {ok, Port} = parrot_gw:start(0),
    {ok, S} = gw_connect(Port),
    try
        %% 3. Deploy v1 → ASK 行为 = 111
        deploy_frame(S, 301, <<"hot">>, <<"1.0.0">>),
        {deployed, 301, [<<"/user/hot">>]} = recv_admin_reply(S, 301),
        111 = ask_hot(S, 401),
        case CheckIds of
            false -> ok;
            true ->
                Commands = [
                    {deploy_component, 999, {<<"forged">>, <<"1">>, {beam, <<"test_hot">>}, singleton, undefined}},
                    {stop_component, 999, <<"/user/hot">>},
                    {drain_component, 999, <<"/user/hot">>, 1000},
                    {component_status, 999, <<"/user/hot">>},
                    {metrics_report, 999}, {measured_metrics_report, 999}],
                [begin
                    Encoded = parrot_gw:encode_admin_cmd_v2(Command),
                    ok = gen_tcp:send(S, parrot_gw:build_frame(?SYSTEM_EVENT, Cid, <<"/reply">>, <<>>, Encoded)),
                    {failed, Cid, 16#0A02, _} = recv_admin_reply(S, Cid),
                    111 = ask_hot(S, Cid + 100),
                    {failed, 16#0A03, _} = parrot_gw:admin_status(<<"/user/forged">>)
                end || {Command, Cid} <- lists:zip(Commands, lists:seq(501, 506))]
        end,

        %% 4. 编译 v2 → 再 Deploy（同名模块热替换）→ ASK 行为 = 222
        compile_version(Dir, 2, 222),
        deploy_frame(S, 302, <<"hot">>, <<"2.0.0">>),
        {deployed, 302, [<<"/user/hot">>]} = recv_admin_reply(S, 302),
        222 = ask_hot(S, 402),

        %% 5. status 报告新版本
        status_frame(S, 303, <<"/user/hot">>),
        {status, 303, [{<<"/user/hot">>, <<"running">>, <<"2.0.0">>}]} =
            recv_admin_reply(S, 303),

        %% 6. stop → 再 status → NOT_FOUND
        stop_frame(S, 304, <<"/user/hot">>),
        {stopped, 304} = recv_admin_reply(S, 304),
        status_frame(S, 305, <<"/user/hot">>),
        {failed, 305, 16#0A03, _} = recv_admin_reply(S, 305)
    after
        gen_tcp:close(S),
        parrot_gw:admin_stop(<<"/user/hot">>),
        parrot_gw:admin_stop(<<"/user/forged">>),
        code:purge(test_hot), code:delete(test_hot), code:del_path(Dir),
        file:delete(filename:join(Dir, "test_hot.erl")),
        file:delete(filename:join(Dir, "test_hot.beam")), file:del_dir(Dir),
        case PreviousDir of
            false -> os:unsetenv("PARROT_ARTIFACT_DIR");
            _ -> os:putenv("PARROT_ARTIFACT_DIR", PreviousDir)
        end
    end.

compile_version(Dir, Ver, Magic) ->
    Src = io_lib:format(
        "-module(test_hot).~n"
        "-export([parrot_service/2]).~n"
        "%% hot-reload fixture v~p (magic ~p)~n"
        "parrot_service(<<\"hot:request\">>, <<>>) -> {<<\"hot:reply\">>, <<~p:64/little>>}.~n",
        [Ver, Magic, Magic]),
    File = filename:join(Dir, "test_hot.erl"),
    ok = file:write_file(File, unicode:characters_to_binary(Src)),
    {ok, test_hot} = compile:file(File, [{outdir, Dir}]),
    ok.

deploy_frame(S, ReqId, Name, Version) ->
    Cmd = parrot_gw:encode_admin_cmd_v2(
        {deploy_component, ReqId, {Name, Version, {beam, <<"test_hot">>},
                                   singleton, undefined}}),
    ok = gen_tcp:send(S, parrot_gw:build_frame(?SYSTEM_EVENT, ReqId,
                                               <<"parrot://t/_admin">>, <<>>, Cmd)).

status_frame(S, ReqId, Prefix) ->
    Cmd = parrot_gw:encode_admin_cmd_v2({component_status, ReqId, Prefix}),
    ok = gen_tcp:send(S, parrot_gw:build_frame(?SYSTEM_EVENT, ReqId,
                                               <<"parrot://t/_admin">>, <<>>, Cmd)).

stop_frame(S, ReqId, Prefix) ->
    Cmd = parrot_gw:encode_admin_cmd_v2({stop_component, ReqId, Prefix}),
    ok = gen_tcp:send(S, parrot_gw:build_frame(?SYSTEM_EVENT, ReqId,
                                               <<"parrot://t/_admin">>, <<>>, Cmd)).

ask_hot(S, Cid) ->
    %% The assertion must traverse the deployed path and the real gateway.
    ok = gen_tcp:send(S, parrot_gw:build_frame(16#10, Cid,
        <<"/user/hot">>, <<"hot:request">>, <<0:32/little>>)),
    {ok, 16#11, _, Cid, _, <<"hot:reply">>, <<V:64/little>>, <<>>} = recv_frame(S),
    V.

recv_admin_reply(S, ReqId) ->
    {ok, ?SYSTEM_EVENT, _Ft, ReqId, _Path, _Key, Payload, <<>>} = recv_frame(S),
    Reply = parrot_gw:decode_admin_reply_v2(Payload),
    ReqId = element(2, Reply),
    Reply.

recv_frame(S) ->
    Buf = case erase({admin_tail, S}) of undefined -> <<>>; B -> B end,
    recv_frame_loop(S, Buf).

recv_frame_loop(S, Buf) ->
    case parrot_gw:parse_frame(Buf) of
        {ok, Ft, Flags, Cid, Path, Key, Payload, Tail} ->
            put({admin_tail, S}, Tail),
            {ok, Ft, Flags, Cid, Path, Key, Payload, <<>>};
        {more, _} ->
            {ok, D} = gen_tcp:recv(S, 0, 5000),
            recv_frame_loop(S, <<Buf/binary, D/binary>>)
    end.

gw_connect(Port) ->
    {ok, S} = gen_tcp:connect("127.0.0.1", Port, [binary, {packet, raw},
                                                   {active, false}, {nodelay, true}]),
    Body = iolist_to_binary(parrot_gw:handshake_body("test-peer")),
    Hs = parrot_gw:build_frame(16#01, 1, <<>>, <<"__handshake__">>, Body),
    ok = gen_tcp:send(S, Hs),
    {ok, 16#02, _, _, _, _, _, <<>>} = recv_frame(S),
    {ok, S}.

assert_rejected(F) ->
    try F() of
        Value -> erlang:error({malformed_accepted, Value})
    catch
        error:_ -> ok;
        exit:_ -> ok
    end.

%% A returned or thrown EXIT-shaped term is not an error/exit exception.
t_rejection_assertion_contract() ->
    ok = assert_rejected(fun() -> erlang:error(badarg) end),
    ok = assert_rejected(fun() -> exit(rejected) end),
    [begin
        {exception, error, {malformed_accepted, Value}} = assertion_outcome(
            fun() -> assert_rejected(fun() -> Value end) end)
    end || Value <- [ok, false, {'EXIT', normal_return}, {error, badarg}]],
    [begin
        {exception, throw, Value} = assertion_outcome(
            fun() -> assert_rejected(fun() -> throw(Value) end) end)
    end || Value <- [rejected, {'EXIT', thrown_term}]],
    ok.

assertion_outcome(F) ->
    try F() of
        Value -> {returned, Value}
    catch
        Class:Reason -> {exception, Class, Reason}
    end.

t_varint_range() ->
    [assert_rejected(fun() -> parrot_gw:bc_put_varint(V) end)
     || V <- [-1, -256, 16#10000000000000000, 1.5]],
    Max = 16#FFFFFFFFFFFFFFFF,
    <<16#FD, Max:64/little>> = parrot_gw:bc_put_varint(Max),
    {Max, <<>>} = parrot_gw:bc_get_varint(<<16#FD, Max:64/little>>).

t_admin_utf8_decode() ->
    [begin
        N = byte_size(B),
        assert_rejected(fun() -> parrot_gw:decode_admin_cmd_v2(<<3, 2, 1, N, B/binary>>) end),
        assert_rejected(fun() -> parrot_gw:decode_admin_reply_v2(<<4, 4, 1, 1, N, B/binary>>) end)
    end || B <- [<<255>>, <<192, 128>>, <<237, 160, 128>>, <<244, 144, 128, 128>>]].

t_admin_utf8_encode() ->
    assert_rejected(fun() -> parrot_gw:encode_admin_cmd_v2({stop_component, 1, <<255>>}) end),
    assert_rejected(fun() -> parrot_gw:encode_admin_reply_v2({failed, 1, 1, <<255>>}) end).

t_failed_code_encode() ->
    [assert_rejected(fun() -> parrot_gw:encode_admin_reply_v2({failed, 1, V, <<>>}) end)
     || V <- [-1, 65536, 16#100000000]],
    {failed, 1, 65535, <<>>} = parrot_gw:decode_admin_reply_v2(
        parrot_gw:encode_admin_reply_v2({failed, 1, 65535, <<>>})).

t_failed_code_decode() ->
    [assert_rejected(fun() -> parrot_gw:decode_admin_reply_v2(<<4, 4, 1, V/binary, 0>>) end)
      || V <- [<<16#FC, 65536:32/little>>, <<16#FC, 1:32/little>>, <<16#FD, 1:64/little>>]].

t_abi_encode() ->
    [assert_rejected(fun() -> parrot_gw:encode_admin_cmd_v2({deploy_component, 1,
        {<<>>, <<>>, {dylib, <<>>, <<>>, V}, singleton, undefined}}) end)
      || V <- [-1, 16#100000000]].

t_abi_decode() ->
    [assert_rejected(fun() -> parrot_gw:decode_admin_cmd_v2(<<3, 0, 1, 0, 0, 5, 0, 0, V/binary, 0, 0>>) end)
      || V <- [<<16#FD, 16#100000000:64/little>>, <<16#FD, 1:64/little>>]].

t_varint_discriminants() ->
    %% Rust's u32 enum decoder accepts a u16 varint prefix for a small value.
    {stop_component, 1, <<>>} = parrot_gw:decode_admin_cmd_v2(<<3, 16#FB, 2:16/little, 1, 0>>),
    {stopped, 1} = parrot_gw:decode_admin_reply_v2(<<4, 16#FB, 2:16/little, 1>>),
    {deploy_component, 1, {<<>>, <<>>, {props, <<>>}, singleton, undefined}} =
        parrot_gw:decode_admin_cmd_v2(<<3, 0, 1, 0, 0, 16#FB, 0:16/little, 0, 16#FB, 0:16/little, 0>>),
    assert_rejected(fun() -> parrot_gw:decode_admin_cmd_v2(<<3, 16#FD, 2:64/little, 1, 0>>) end).

t_all_frozen_prefixes() ->
    Rows=parrot_test_vectors:admin(),
    true=length(Rows)>=6,
    [begin
        {Decode,Encode} = case Bytes of
            <<3, _/binary>> -> {fun parrot_gw:decode_admin_cmd_v2/1,fun parrot_gw:encode_admin_cmd_v2/1};
            <<4, _/binary>> -> {fun parrot_gw:decode_admin_reply_v2/1,fun parrot_gw:encode_admin_reply_v2/1}
        end,
        Bytes=Encode(Decode(Bytes)),
        [assert_rejected(fun() -> Decode(binary:part(Bytes, 0, N)) end)
         || N <- lists:seq(0, byte_size(Bytes) - 1)],
        assert_rejected(fun() -> Decode(<<Bytes/binary, 0>>) end)
    end || #{<<"payload">>:=Bytes} <- Rows],
    io:format("SHARED_ADMIN_JSON_OK vectors=~p~n",[length(Rows)]).

t_malformed_admin_reply() ->
    {ok, Port} = parrot_gw:start(0),
    {ok, Sock} = gw_connect(Port),
    try
        %% This is a valid Wire envelope containing invalid UTF-8 in Stop's path.
        Frame = parrot_gw:build_frame(?SYSTEM_EVENT, 201, <<"reply">>, <<>>, <<3, 2, 201, 1, 255>>),
        ok = gen_tcp:send(Sock, Frame),
        {failed, 201, 16#0A02, _} = recv_admin_reply(Sock, 201)
    after gen_tcp:close(Sock) end.

t_vector_bounds() ->
    [assert_rejected(fun() -> parrot_gw:decode_admin_reply_v2(<<4, Tag, 1, 16#FD, 16#FFFFFFFFFFFFFFFF:64/little>>) end)
     || Tag <- [0, 3]],
    Paths = [integer_to_binary(I) || I <- lists:seq(1, 1000)],
    {deployed, 1, Paths} = parrot_gw:decode_admin_reply_v2(parrot_gw:encode_admin_reply_v2({deployed, 1, Paths})),
    States = [{P, <<"running">>, <<"1">>} || P <- Paths],
    {status, 2, States} = parrot_gw:decode_admin_reply_v2(parrot_gw:encode_admin_reply_v2({status, 2, States})).


t_init_failure() ->
    parrot_gw:admin_init(),
    Dir=filename:join("/tmp","parrot-init-contract-"++os:getpid()++"-"++integer_to_list(erlang:unique_integer([positive]))),
    ok=file:make_dir(Dir),
    Src=filename:join(Dir,"test_init_failure.erl"),
    try
        [begin
            Body="-module(test_init_failure).\n-export([parrot_init/0]).\nparrot_init() -> "++Expr++".\n",
            ok=file:write_file(Src,Body),
            {ok,test_init_failure}=compile:file(Src,[{outdir,Dir}]),
            {failed,16#0A06,_}=parrot_gw:admin_deploy({<<"init-failure">>,<<"1">>,
                {beam,<<"test_init_failure">>,list_to_binary("file://"++Dir)},singleton,undefined}),
            {failed,16#0A03,_}=parrot_gw:admin_status(<<"/user/init-failure">>)
        end || Expr <- ["erlang:error(fixture_failure)","{error,fixture_failure}"]]
    after
        parrot_gw:admin_stop(<<"/user/init-failure">>),
        code:purge(test_init_failure),code:delete(test_init_failure),code:del_path(Dir),
        file:delete(Src),file:delete(filename:join(Dir,"test_init_failure.beam")),file:del_dir(Dir)
    end.

with_status_artifact(F) ->
    Dir="/tmp/parrot-admin-component-"++os:getpid()++"-"++integer_to_list(erlang:unique_integer([positive])),
    ok=file:make_dir(Dir),Src=filename:join(Dir,"test_admin_component.erl"),
    try
        ok=file:write_file(Src,"-module(test_admin_component).\n-export([parrot_service/2]).\nparrot_service(K,_)->{K,<<>>}.\n"),
        {ok,test_admin_component}=compile:file(Src,[{outdir,Dir}]),F(list_to_binary("file://"++Dir))
    after
        [parrot_gw:admin_stop(<<"/user/",N/binary>>) || N <- [<<"logt">>,<<"n1">>,<<"n1x">>]],
        file:del_dir_r(Dir)
    end.
