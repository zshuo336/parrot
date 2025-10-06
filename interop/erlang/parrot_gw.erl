%% Parrot Erlang Gateway 模块本体（DEV_03 §7 / E6——POC 转正为 Wire 1.0）。
%%
%% Rust 节点 <--Wire 1.0(TLV 握手)--> 本网关 <--OTP 原生--> Erlang 进程
%%
%% 帧布局（07 §2.1，与 Rust/JVM/Python 逐字节一致）：
%%   外层: [u32 body_len LE]
%%   body: [ver u8][ft u8][flags u16 LE][cid u64 LE]
%%         [hop_count u8][hop_limit u8][rsv u48]
%%         [path_len u32 LE][path][key_len u32 LE][key][payload]
%%
%% 握手 TLV: tag u8 + len u16 LE + value（NODE_ID=1 CAPS=4 MAX_FRAME_LEN=5
%% TOPOLOGY_ROLE=6 HOP_LIMIT=7；ACK 加 CHOSEN_CODEC=8）
-module(parrot_gw).
-export([main/1, start/1, start/2, service/2, build_frame/5, build_frame/6,
         parse_frame/1, handshake_body/1, handshake_body/2, handshake_ack_body/1,
         err_payload/2, parse_port/1,
         %% B4（DEV_09）：admin-v2 编解码 + 四命令（测试直呼）
         encode_admin_cmd_v2/1, decode_admin_cmd_v2/1,
         encode_admin_reply_v2/1, decode_admin_reply_v2/1,
         admin_init/0, admin_deploy/1, admin_stop/1, admin_drain/2,
         admin_status/1, admin_instance_paths/2, instance_count/1,
         bc_put_varint/1, bc_get_varint/1]).

-define(VER, 1).
-define(ASK, 16#10).
-define(REPLY, 16#11).
-define(REPLY_ERR, 16#12).
-define(TELL, 16#13).
-define(HANDSHAKE, 16#01).
-define(HANDSHAKE_ACK, 16#02).
-define(HEARTBEAT, 16#03).
-define(HEARTBEAT_ACK, 16#04).
-define(ROUTE_HINT, 16#24).
%% 直连表（方案 A）：node_id → socket（hub 注入 hint 后建立；
%% 对该节点的出站帧直发，不经 hub）
-define(DIRECT_TAB, parrot_direct_links).

%% ============ 帧编解码（Wire 1.0 正式布局） ============

build_frame(Ft, Cid, Path, Key, Payload) ->
    build_frame(Ft, 0, Cid, Path, Key, Payload).

build_frame(Ft, Flags, Cid, Path, Key, Payload) ->
    PathB = to_bin(Path),
    KeyB = to_bin(Key),
    %% body = 定长 24 + path_len(4) + path + key_len(4) + key + payload（07 §2.1）
    BodyLen = 28 + byte_size(PathB) + byte_size(KeyB) + byte_size(Payload),
    <<BodyLen:32/little,
      ?VER:8, Ft:8, Flags:16/little, Cid:64/little,
      0:8, 8:8, 0:48,                     %% hop_count / hop_limit / rsv48
      (byte_size(PathB)):32/little, PathB/binary,
      (byte_size(KeyB)):32/little, KeyB/binary,
      Payload/binary>>.

to_bin(B) when is_binary(B) -> B;
to_bin(L) when is_list(L) -> unicode:characters_to_binary(L).

parse_frame(Buf) ->
    case Buf of
        <<BodyLen:32/little, Rest/binary>> when byte_size(Rest) >= BodyLen ->
            Body = binary:part(Rest, 0, BodyLen),
            Tail = binary:part(Rest, BodyLen, byte_size(Rest) - BodyLen),
            <<?VER:8, Ft:8, Flags:16/little, Cid:64/little,
              _HopC:8, _HopL:8, _Rsv:48,
              PathLen:32/little, PathB:PathLen/binary,
              KeyLen:32/little, KeyB:KeyLen/binary,
              Payload/binary>> = Body,
            {ok, Ft, Flags, Cid, PathB, KeyB, Payload, Tail};
        _ ->
            {more, Buf}
    end.

%% ============ 握手 TLV ============

tlv(Tag, V) -> <<Tag:8, (byte_size(V)):16/little, V/binary>>.

caps_pb_only() -> 16#03.  %% bin+pb 双栈（07 §8.1 原始 pb-only；crawler-lab
                           %% 起 bin: 裸键载荷也走本网关——能力位放宽为双栈）

handshake_body(NodeId) ->
    Id = to_bin(NodeId),
    [tlv(1, Id),
     tlv(4, <<(caps_pb_only()):32/little>>),
     tlv(5, <<1048576:32/little>>),
     tlv(6, <<0:8>>),
     tlv(7, <<8:8>>)].

%% 直拨地址声明（方案 A）：有 listen 端口时附加 tag 8——hub 据此向
%% 通信对端注入 ROUTE_HINT。无 listen（纯客户端形态）不附加。
handshake_body(NodeId, DirectAddr) when is_list(DirectAddr) ->
    handshake_body(NodeId) ++ [tlv(8, to_bin(DirectAddr))];
handshake_body(NodeId, undefined) ->
    handshake_body(NodeId).

handshake_ack_body(NodeId) ->
    %% ACK 专有 chosen_codec 用 tag 9（8 已被 DIRECT_ADDR 占用）
    handshake_body(NodeId) ++ [tlv(9, <<"pb">>)].


%% ============ B4（DEV_09）：admin-v2 编解码 + Beam 热加载 ============
%% payload = [u8 tag][bincode standard(body)]；tag 0x03=CMD 0x04=REPLY
%% bincode varint：值≤250 单字节；0xFB+u16LE / 0xFC+u32LE / 0xFD+u64LE。
%% serde 形态：enum = [varint 变体索引][字段序体]（externally tagged）；
%% Option = 0/1+体；String/Vec = varint len + 元素。
%% 冻结事实源：docs/vectors/admin_v2.json（六向量逐字节互锁）。

-define(FT_SYSTEM_EVENT, 16#20).  %% B4 对齐修正：Rust frame_type::SYSTEM_EVENT（0x05 是 HEARTBEAT_ACK 段——错值会静默丢帧）
-define(TAG_ADMIN_CMD_V2, 16#03).
-define(TAG_ADMIN_REPLY_V2, 16#04).
%% v2 错误码扩展段
-define(V2_ARTIFACT_FETCH, 16#0A00).
-define(V2_ARTIFACT_DIGEST, 16#0A01).
-define(V2_DIALECT_MISMATCH, 16#0A02).
-define(V2_COMPONENT_NOT_FOUND, 16#0A03).
-define(V2_DRAIN_TIMEOUT, 16#0A04).
-define(V2_FACTORY_NOT_FOUND, 16#0A05).
-define(V2_SPAWN_FAILED, 16#0A06).
%% 组件登记表（admin-v2 Deploy 写入 / Status 报告 / Stop 清除）
-define(ADMIN_TAB, parrot_admin_components).

%% ---- varint ----

bc_put_varint(V) when V =< 250 -> <<V:8>>;
bc_put_varint(V) when V =< 16#FFFF -> <<16#FB:8, V:16/little>>;
bc_put_varint(V) when V =< 16#FFFFFFFF -> <<16#FC:8, V:32/little>>;
bc_put_varint(V) -> <<16#FD:8, V:64/little>>.

bc_get_varint(<<B0:8, Rest/binary>>) when B0 =< 16#FA -> {B0, Rest};
bc_get_varint(<<16#FB:8, V:16/little, Rest/binary>>) -> {V, Rest};
bc_get_varint(<<16#FC:8, V:32/little, Rest/binary>>) -> {V, Rest};
bc_get_varint(<<16#FD:8, V:64/little, Rest/binary>>) -> {V, Rest}.

%% ---- 基础 serde 形态 ----

bc_put_str(S) -> B = to_bin(S), <<(bc_put_varint(byte_size(B)))/binary, B/binary>>.

bc_get_str(Bin0) ->
    {N, Bin1} = bc_get_varint(Bin0),
    <<S:N/binary, Rest/binary>> = Bin1,
    {S, Rest}.

%% Option<binary>：0=None；1=varint len + bytes
bc_put_opt_bytes(undefined) -> <<0:8>>;
bc_put_opt_bytes(B) -> <<1:8, (bc_put_varint(byte_size(B)))/binary, B/binary>>.

bc_get_opt_bytes(<<0:8, Rest/binary>>) -> {undefined, Rest};
bc_get_opt_bytes(<<1:8, Bin0/binary>>) ->
    {N, Bin1} = bc_get_varint(Bin0),
    <<B:N/binary, Rest/binary>> = Bin1,
    {B, Rest}.

%% ---- AdminArtifactRef（变体 0..5）----

bc_put_artifact({props, Factory}) ->
    <<0:8, (bc_put_str(Factory))/binary>>;
bc_put_artifact({beam, App}) ->
    <<1:8, (bc_put_str(App))/binary, 0:8>>;  %% uri None（R3 补位）
bc_put_artifact({beam, App, Uri}) ->
    <<1:8, (bc_put_str(App))/binary, (bc_put_opt_str(Uri))/binary>>;
bc_put_artifact({pymodule, Module, Env}) ->
    <<2:8, (bc_put_str(Module))/binary, (bc_put_opt_str(Env))/binary, 0:8>>;
bc_put_artifact({pymodule, Module, Env, Uri}) ->
    <<2:8, (bc_put_str(Module))/binary, (bc_put_opt_str(Env))/binary,
      (bc_put_opt_str(Uri))/binary>>;
bc_put_artifact({jvm, MainClass, Coords}) ->
    <<3:8, (bc_put_str(MainClass))/binary, (bc_put_opt_str(Coords))/binary, 0:8>>;
bc_put_artifact({jvm, MainClass, Coords, Uri}) ->
    <<3:8, (bc_put_str(MainClass))/binary, (bc_put_opt_str(Coords))/binary,
      (bc_put_opt_str(Uri))/binary>>;
bc_put_artifact({wasm, Digest, Uri}) ->
    <<4:8, (bc_put_str(Digest))/binary, (bc_put_str(Uri))/binary>>;
bc_put_artifact({dylib, Digest, Uri, Abi}) ->
    <<5:8, (bc_put_str(Digest))/binary, (bc_put_str(Uri))/binary,
      (bc_put_varint(Abi))/binary>>.

bc_put_opt_str(undefined) -> <<0:8>>;
bc_put_opt_str(S) -> <<1:8, (bc_put_str(S))/binary>>.

bc_get_opt_str(<<0:8, Rest/binary>>) -> {undefined, Rest};
bc_get_opt_str(<<1:8, Bin0/binary>>) ->
    {S, Rest} = bc_get_str(Bin0),
    {S, Rest}.

bc_get_artifact(<<0:8, Bin0/binary>>) ->
    {F, Rest} = bc_get_str(Bin0), {{props, F}, Rest};
bc_get_artifact(<<1:8, Bin0/binary>>) ->
    {App, Bin1} = bc_get_str(Bin0),
    {Uri, Rest} = bc_get_opt_str(Bin1),
    {{beam, App, Uri}, Rest};
bc_get_artifact(<<2:8, Bin0/binary>>) ->
    {M, Bin1} = bc_get_str(Bin0),
    {Env, Bin2} = bc_get_opt_str(Bin1),
    {Uri, Rest} = bc_get_opt_str(Bin2),
    {{pymodule, M, Env, Uri}, Rest};
bc_get_artifact(<<3:8, Bin0/binary>>) ->
    {MC, Bin1} = bc_get_str(Bin0),
    {Coords, Bin2} = bc_get_opt_str(Bin1),
    {Uri, Rest} = bc_get_opt_str(Bin2),
    {{jvm, MC, Coords, Uri}, Rest};
bc_get_artifact(<<4:8, Bin0/binary>>) ->
    {D, Bin1} = bc_get_str(Bin0), {U, Rest} = bc_get_str(Bin1),
    {{wasm, D, U}, Rest};
bc_get_artifact(<<5:8, Bin0/binary>>) ->
    {D, Bin1} = bc_get_str(Bin0), {U, Bin2} = bc_get_str(Bin1),
    {Abi, Rest} = bc_get_varint(Bin2),
    {{dylib, D, U, Abi}, Rest}.

%% ---- AdminInstancePolicy（0=Singleton 1=Pool 2=Sharded）----

bc_put_policy(singleton) -> <<0:8>>;
bc_put_policy({pool, N}) -> <<1:8, (bc_put_varint(N))/binary>>;
bc_put_policy({sharded, N}) -> <<2:8, (bc_put_varint(N))/binary>>.

bc_get_policy(<<0:8, Rest/binary>>) -> {singleton, Rest};
bc_get_policy(<<1:8, Bin0/binary>>) ->
    {N, Rest} = bc_get_varint(Bin0), {{pool, N}, Rest};
bc_get_policy(<<2:8, Bin0/binary>>) ->
    {N, Rest} = bc_get_varint(Bin0), {{sharded, N}, Rest}.

instance_count(singleton) -> 1;
instance_count({pool, N}) -> N;
instance_count({sharded, N}) -> N.

%% ---- ComponentDeploy ----
%% {deploy, Name, Version, Artifact, Instances, Config}

bc_put_deploy({Name, Version, Artifact, Instances, Config}) ->
    <<(bc_put_str(Name))/binary, (bc_put_str(Version))/binary,
      (bc_put_artifact(Artifact))/binary, (bc_put_policy(Instances))/binary,
      (bc_put_opt_bytes(Config))/binary>>.

bc_get_deploy(Bin0) ->
    {Name, Bin1} = bc_get_str(Bin0),
    {Version, Bin2} = bc_get_str(Bin1),
    {Artifact, Bin3} = bc_get_artifact(Bin2),
    {Instances, Bin4} = bc_get_policy(Bin3),
    {Config, Rest} = bc_get_opt_bytes(Bin4),
    {{Name, Version, Artifact, Instances, Config}, Rest}.

%% ---- AdminCommandV2 ----
%% {deploy_component, ReqId, Deploy}
%% {drain_component, ReqId, Prefix, TimeoutMs}
%% {stop_component, ReqId, Prefix}
%% {component_status, ReqId, Prefix}

encode_admin_cmd_v2(Cmd) ->
    <<?TAG_ADMIN_CMD_V2:8, (enc_cmd_body(Cmd))/binary>>.

enc_cmd_body({deploy_component, ReqId, Deploy}) ->
    <<0:8, (bc_put_varint(ReqId))/binary, (bc_put_deploy(Deploy))/binary>>;
enc_cmd_body({drain_component, ReqId, Prefix, TimeoutMs}) ->
    <<1:8, (bc_put_varint(ReqId))/binary, (bc_put_str(Prefix))/binary,
      (bc_put_varint(TimeoutMs))/binary>>;
enc_cmd_body({stop_component, ReqId, Prefix}) ->
    <<2:8, (bc_put_varint(ReqId))/binary, (bc_put_str(Prefix))/binary>>;
enc_cmd_body({component_status, ReqId, Prefix}) ->
    <<3:8, (bc_put_varint(ReqId))/binary, (bc_put_str(Prefix))/binary>>.

decode_admin_cmd_v2(<<?TAG_ADMIN_CMD_V2:8, Body/binary>>) ->
    {Cmd, <<>>} = dec_cmd_body(Body),
    Cmd.

dec_cmd_body(<<0:8, Bin0/binary>>) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Deploy, Rest} = bc_get_deploy(Bin1),
    {{deploy_component, ReqId, Deploy}, Rest};
dec_cmd_body(<<1:8, Bin0/binary>>) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Prefix, Bin2} = bc_get_str(Bin1),
    {TimeoutMs, Rest} = bc_get_varint(Bin2),
    {{drain_component, ReqId, Prefix, TimeoutMs}, Rest};
dec_cmd_body(<<2:8, Bin0/binary>>) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Prefix, Rest} = bc_get_str(Bin1),
    {{stop_component, ReqId, Prefix}, Rest};
dec_cmd_body(<<3:8, Bin0/binary>>) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Prefix, Rest} = bc_get_str(Bin1),
    {{component_status, ReqId, Prefix}, Rest}.

%% ---- AdminReplyV2 ----
%% {deployed, ReqId, [Path]}
%% {drained, ReqId, Drained, Aborted}
%% {stopped, ReqId}
%% {status, ReqId, [{Path, State, Version}]}
%% {failed, ReqId, Code, Detail}

encode_admin_reply_v2(Reply) ->
    <<?TAG_ADMIN_REPLY_V2:8, (enc_reply_body(Reply))/binary>>.

enc_reply_body({deployed, ReqId, Paths}) ->
    N = length(Paths),
    List = << <<(bc_put_str(P))/binary>> || P <- Paths >>,
    <<0:8, (bc_put_varint(ReqId))/binary, (bc_put_varint(N))/binary, List/binary>>;
enc_reply_body({drained, ReqId, Drained, Aborted}) ->
    <<1:8, (bc_put_varint(ReqId))/binary, (bc_put_varint(Drained))/binary,
      (bc_put_varint(Aborted))/binary>>;
enc_reply_body({stopped, ReqId}) ->
    <<2:8, (bc_put_varint(ReqId))/binary>>;
enc_reply_body({status, ReqId, States}) ->
    N = length(States),
    List = << <<(bc_put_str(P))/binary, (bc_put_str(S))/binary,
                (bc_put_str(V))/binary>> || {P, S, V} <- States >>,
    <<3:8, (bc_put_varint(ReqId))/binary, (bc_put_varint(N))/binary, List/binary>>;
enc_reply_body({failed, ReqId, Code, Detail}) ->
    <<4:8, (bc_put_varint(ReqId))/binary, (bc_put_varint(Code))/binary,
      (bc_put_str(Detail))/binary>>.

decode_admin_reply_v2(<<?TAG_ADMIN_REPLY_V2:8, Body/binary>>) ->
    {Reply, <<>>} = dec_reply_body(Body),
    Reply.

dec_reply_body(<<0:8, Bin0/binary>>) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {N, Bin2} = bc_get_varint(Bin1),
    {Paths, Rest} = bc_get_str_list(N, Bin2),
    {{deployed, ReqId, Paths}, Rest};
dec_reply_body(<<1:8, Bin0/binary>>) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {D, Bin2} = bc_get_varint(Bin1),
    {A, Rest} = bc_get_varint(Bin2),
    {{drained, ReqId, D, A}, Rest};
dec_reply_body(<<2:8, Bin0/binary>>) ->
    {ReqId, Rest} = bc_get_varint(Bin0),
    {{stopped, ReqId}, Rest};
dec_reply_body(<<3:8, Bin0/binary>>) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {N, Bin2} = bc_get_varint(Bin1),
    {States, Rest} = bc_get_states(N, Bin2),
    {{status, ReqId, States}, Rest};
dec_reply_body(<<4:8, Bin0/binary>>) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Code, Bin2} = bc_get_varint(Bin1),
    {Detail, Rest} = bc_get_str(Bin2),
    {{failed, ReqId, Code, Detail}, Rest}.

bc_get_str_list(0, Bin) -> {[], Bin};
bc_get_str_list(N, Bin0) ->
    {S, Bin1} = bc_get_str(Bin0),
    {Rest, Bin2} = bc_get_str_list(N - 1, Bin1),
    {[S | Rest], Bin2}.

bc_get_states(0, Bin) -> {[], Bin};
bc_get_states(N, Bin0) ->
    {P, Bin1} = bc_get_str(Bin0),
    {S, Bin2} = bc_get_str(Bin1),
    {V, Bin3} = bc_get_str(Bin2),
    {Rest, Bin4} = bc_get_states(N - 1, Bin3),
    {[{P, S, V} | Rest], Bin4}.

%% ---- admin-v2 四命令执行（Beam 热加载方言）----

admin_init() ->
    case ets:info(?ADMIN_TAB) of
        undefined -> ets:new(?ADMIN_TAB, [named_table, public, set,
                                          {read_concurrency, true}]);
        _ -> ok
    end.

%% 实例路径展开（与 parrot-app/Rust/Python 同规）
admin_instance_paths(Name, singleton) -> [<<"/user/", Name/binary>>];
admin_instance_paths(Name, {pool, N}) ->
    [<<"/user/", Name/binary, $-, (integer_to_binary(I))/binary>> || I <- lists:seq(0, N - 1)];
admin_instance_paths(Name, {sharded, N}) ->
    admin_instance_paths(Name, {pool, N}).

%% Deploy{Beam}：ArtifactDir 加 code path → code:load_abs(Module) 热加载
%% （新 beam 替换旧版本——OTP code replacement 语义；后续 service 分发即新
%% 模块行为）。ArtifactDir 发现顺序：
%%   1. $PARROT_ARTIFACT_DIR（与 Rust/Python 方言同约定）
%%   2. /tmp/parrot-artifacts/{app}/（Manifest 形态惯例）
%%   3. 当前 code path（beam 已在搜索路径——纯 load_abs）
%% 登记表写入 {Name, Version, Paths, Module}。
admin_deploy({Name, Version, {beam, App, Uri}, Instances, _Config}) ->
    try
        Module = binary_to_atom(App, utf8),
        %% R3：uri（file:// 目录形态）优先加入 code path——app 构建产物直发
        case Uri of
            undefined -> ok;
            << "file://", Dir/binary >> ->
                DirS = binary_to_list(Dir),
                case lists:member(DirS, code:get_path()) of
                    true  -> ok;
                    false -> code:add_patha(DirS)
                end;
            _ -> ok
        end,
        add_artifact_paths(App),
        %% OTP 热替换语义（B4 规格 code:load_abs + restart_child 的方言落点）：
        %%   purge（清旧版）→ load_file（沿 code path 载新版——artifact dir
        %%   已 add_patha 在首，同名模块即热替换；后续调用走新版代码）。
        code:purge(Module),
        case code:load_file(Module) of
            {module, Module} ->
                %% 组件契约：deploy 后首问前调用 parrot_init/0（幂等——
                %% 组件自建状态；frontier 的 ets 表等）。无导出则跳过。
                case erlang:function_exported(Module, parrot_init, 0) of
                    true -> catch Module:parrot_init();
                    false -> ok
                end,
                Paths = admin_instance_paths(Name, Instances),
                ets:insert(?ADMIN_TAB, {Name, Version, Paths, Module}),
                {deployed, Paths};
            {error, Why} ->
                {failed, ?V2_SPAWN_FAILED,
                 iolist_to_binary(io_lib:format("load_file ~s: ~p", [App, Why]))}
        end
    catch
        _:R -> {failed, ?V2_SPAWN_FAILED,
                iolist_to_binary(io_lib:format("deploy: ~p", [R]))}
    end;
admin_deploy({_Name, _V, Other, _I, _C}) ->
    {failed, ?V2_DIALECT_MISMATCH,
     iolist_to_binary(io_lib:format("erl executor expects Beam, got ~p", [Other]))}.

%% artifact 目录发现 + add_patha（幂等——已在 path 则跳过）
add_artifact_paths(App) ->
    Dirs = artifact_dirs(App),
    [begin
         case lists:member(D, code:get_path()) of
             true -> ok;
             false -> code:add_patha(D)
         end
     end || D <- Dirs],
    ok.

artifact_dirs(App) ->
    EnvDir = case os:getenv("PARROT_ARTIFACT_DIR") of
                 false -> "/tmp/parrot-artifacts";
                 D -> D
             end,
    AppS = binary_to_list(App),
    [filename:join(EnvDir, AppS), EnvDir].

%% Stop：登记表清除（beam 模块保留——code:replace_semaphore 语义之外
%% 的最小方言实现；热加载测试关注行为切换而非卸载）。
admin_stop(Prefix) ->
    case admin_match(Prefix) of
        [] -> {failed, ?V2_COMPONENT_NOT_FOUND,
               iolist_to_binary(io_lib:format("no component under ~s", [Prefix]))};
        Comps ->
            lists:foreach(fun({Name, _, _, _}) ->
                            ets:delete(?ADMIN_TAB, Name)
                          end, Comps),
            stopped
    end.

%% Drain：erlang 无邮箱排空原语映射（网关 service 无状态进程 per-ask）——
%% 语义映射为登记表组件全部优雅清除（drained=实例数 / aborted=0）。
admin_drain(Prefix, _TimeoutMs) ->
    case admin_match(Prefix) of
        [] -> {failed, ?V2_COMPONENT_NOT_FOUND,
               iolist_to_binary(io_lib:format("no component under ~s", [Prefix]))};
        Comps ->
            N = lists:sum([length(P) || {_, _, P, _} <- Comps]),
            lists:foreach(fun({Name, _, _, _}) ->
                            ets:delete(?ADMIN_TAB, Name)
                          end, Comps),
            {drained_reply, N, 0}
    end.

admin_status(Prefix) ->
    case admin_match(Prefix) of
        [] -> {failed, ?V2_COMPONENT_NOT_FOUND,
               iolist_to_binary(io_lib:format("no component under ~s", [Prefix]))};
        Comps ->
            States = [{P, <<"running">>, V}
                      || {_, V, Paths, _} <- Comps, P <- Paths],
            {status_reply, States}
    end.

%% 前缀匹配（Rust/Python 同规：整段相等或后随 '-'）
admin_match(Prefix) ->
    ets:foldl(fun({_Name, _V, Paths, _M} = E, Acc) ->
                  Match = fun(P) ->
                                  P =:= Prefix orelse
                                    (binary:match(P, Prefix) =:= {0, byte_size(Prefix)}
                                     andalso binary:at(P, byte_size(Prefix)) =:= $-)
                          end,
                  case lists:any(Match, Paths) of
                      true -> [E | Acc];
                      false -> Acc
                  end
              end, [], ?ADMIN_TAB).

%% SYSTEM_EVENT payload 分发：admin-v2 命令 → 执行 → 回帧。
%% 返回 {ok, ReplyFrame} | ignore（非 admin-v2 tag）。
admin_handle_frame(Sock, Cid, Path, <<?TAG_ADMIN_CMD_V2:8, _/binary>> = Payload) ->
    Cmd = decode_admin_cmd_v2(Payload),
    log("admin cmd decoded: ~p~n", [Cmd]),
    Reply = case Cmd of
                {deploy_component, ReqId, Deploy} ->
                    admin_reply(ReqId, admin_deploy(Deploy));
                {drain_component, ReqId, Prefix, TimeoutMs} ->
                    admin_reply(ReqId, admin_drain(Prefix, TimeoutMs));
                {stop_component, ReqId, Prefix} ->
                    admin_reply(ReqId, admin_stop(Prefix));
                {component_status, ReqId, Prefix} ->
                    admin_reply(ReqId, admin_status(Prefix))
            end,
    gen_tcp:send(Sock, build_frame(?FT_SYSTEM_EVENT, Cid, Path, <<>>,
                                   encode_admin_reply_v2(Reply))),
    ok;
admin_handle_frame(_Sock, _Cid, _Path, _) ->
    ignore.

%% 方言内部形态 → AdminReplyV2 tuple
admin_reply(ReqId, {deployed, Paths}) -> {deployed, ReqId, Paths};
admin_reply(ReqId, {drained_reply, D, A}) -> {drained, ReqId, D, A};
admin_reply(ReqId, stopped) -> {stopped, ReqId};
admin_reply(ReqId, {status_reply, States}) -> {status, ReqId, States};
admin_reply(ReqId, {failed, Code, Detail}) -> {failed, ReqId, Code, Detail}.

%% ============ Erlang actor 服务（方言可辨识） ============
%% R4（应用体系架构纠正）：已部署组件优先——service/2 先查 ADMIN_TAB
%% （Deploy{Beam} 载入的业务模块），命中即转派 Module:parrot_service/2；
%% 未命中走网关内置探针（bin:u:*——连通性/方言键验证，非业务）。
%% 网关不再内置 crawler 业务 handler（业务代码已迁 apps/*/erlang/）。

service(Key, Payload) ->
    case component_service(Key, Payload) of
        {ok, Reply}    -> Reply;
        {error, miss}  -> builtin_service(Key, Payload)
    end.

%% 已部署组件转派（按登记序遍历——单键单组件为常态）。
component_service(Key, Payload) ->
    try ets:tab2list(?ADMIN_TAB) of
        Comps -> component_service_loop(Comps, Key, Payload)
    catch
        _:_ -> {error, miss}
    end.

component_service_loop([], _Key, _Payload) -> {error, miss};
component_service_loop([{_, _, _, Module} | T], Key, Payload) ->
    case erlang:function_exported(Module, parrot_service, 2) of
        true ->
            case try Module:parrot_service(Key, Payload) catch _:_ -> error end of
                {ReplyKey, ReplyBody} -> {ok, {ReplyKey, ReplyBody}};
                _ -> component_service_loop(T, Key, Payload)
            end;
        false -> component_service_loop(T, Key, Payload)
    end.
builtin_service(<<"bin:u:Ping">>, <<N:64/little>>) ->
    {<<"bin:u:Pong">>, <<(N + 3):64/little>>};   %% erlang 方言 +3
builtin_service(<<"bin:u:Add">>, <<A:64/little, B:64/little>>) ->
    {<<"bin:u:AddR">>, <<(A + B + 10000):64/little>>};  %% erlang 方言 +10000
builtin_service(Key, _Payload) ->
    erlang:error({unknown_service, Key}).

%% ============ 错误体（[u16 code][u16 rsv][detail utf8]） ============

err_payload(Code, Detail) ->
    D = to_bin(Detail),
    <<Code:16/little, 0:16/little, D/binary>>.

%% ============ main（网关进程入口） ============

main(Args) ->
    %% 双模式组网（业界标准：网关既可被动等 parrot 连入，也可主动注册）：
    %%   ["Port"]                    —— 被动模式（listen 等 parrot 拨入）
    %%   ["Port", "parrot=Host:Port"]—— 注册模式（启动后主动拨号 parrot 应用，
    %%                                  握手后同一 loop 服务——生产拓扑形态）
    case Args of
        [P, "parrot=" ++ Target] ->
            Port = ?MODULE:parse_port(P),
            {ok, RealPort} = start(Port),
            io:format("PARROT_ERL_PORT=~p~n", [RealPort]),
            register_parrot(Target),
            receive stop_gateway -> halt(0) end;
        _ ->
            Port = case Args of
                       [P] when is_list(P) -> list_to_integer(P);
                       [P] when is_integer(P) -> P;
                       _ -> 0
                   end,
            {ok, RealPort} = start(Port),
            io:format("PARROT_ERL_PORT=~p~n", [RealPort]),
            receive stop_gateway -> halt(0) end   %% 驻留（被 kill 或 halt）
    end.

parse_port(P) when is_list(P) -> list_to_integer(P);
parse_port(P) when is_integer(P) -> P;
parse_port(_) -> 0.

%% 注册模式：主动拨号 parrot 节点 + **断线自动重连 + 双向心跳健康检查**。
%% 连接进程结构（工业级容灾）：
%%   registrar(监督) —— 持有目标地址与重连退避状态，连接死亡即重拨
%%   conn(工作者)   —— 一条到 parrot 的连接；loop/2 收帧 + 心跳监测
%%     · 收 HEARTBEAT 秒回 ACK（hub 探活本网关）
%%     · 10s 无任何入帧 → 判 hub 半开死亡 → 自杀 → registrar 重拨
%%   直连学习（方案 A）：收 ROUTE_HINT(node, addr) → spawn 直拨该网关
%%     建第二条链路（后续对它的帧直发，不经 hub 中转）
register_parrot(Target) ->
    [Host, PortS] = string:split(Target, ":"),
    Port = list_to_integer(PortS),
    Self = self(),
    Pid = spawn(fun() -> registrar(Host, Port, Self, 0) end),
    receive
        registered           -> ok;
        {register_failed, R} -> halt(1), exit({register_failed, R})
    after 15000 -> halt(2)
    end,
    {ok, Pid}.

%% 监督者：无限重连（指数退避 1s→60s；连接建立重置）。首连失败也重试
%% （原先一次失败即 halt——网络抖动场景下网关成了孤儿）。
registrar(Host, Port, Parent, Attempt) ->
    Delay = case Attempt of
                0 -> 0;                                  %% 首连立即
                N -> min(60000, 1000 * trunc(math:pow(2, min(N, 6))))
            end,
    case Delay of 0 -> ok; D -> timer:sleep(D) end,
    case gen_tcp:connect(Host, Port, [binary, {packet, raw},
                                      {active, false},
                                      {nodelay, true}], 5000) of
        {ok, Sock} ->
            Hs = iolist_to_binary(handshake_body("erl-gw-1")),
            ok = gen_tcp:send(Sock, build_frame(?HANDSHAKE, 1, <<"">>,
                                                <<"__handshake__">>, Hs)),
            case recv_frame(Sock, <<>>) of
                {ok, ?HANDSHAKE_ACK, _Cid, _Path, _Key, _Payload} ->
                    log("parrot handshake ok — serving~n", []),
                    put(service_fun, fun ?MODULE:service/2),
                    case Parent of
                        P when is_pid(P) -> P ! registered;
                        _ -> ok  %% 重连成功——上层早已就绪，不重复通知
                    end,
                    %% 工作者：死亡（断连/半开）→ 本进程（registrar）重拨。
                    %% registered 消息只在首连发（重连不重复通知——上层已就绪）。
                    Worker = spawn(fun() -> conn_loop(Sock, <<>>) end),
                    Ref = erlang:monitor(process, Worker),
                    receive
                        {'DOWN', Ref, _, _, Reason} ->
                            log("conn down (~p) — reconnecting~n", [Reason]),
                            registrar(Host, Port, none, 0)  %% 立即重连
                    end;
                Other ->
                    log("parrot handshake FAILED: ~p~n", [Other]),
                    registrar(Host, Port, none, Attempt + 1)
            end;
        {error, Why} ->
            log("connect parrot FAILED: ~p~n", [Why]),
            registrar(Host, Port, none, Attempt + 1)
    end.

%% 半开检测说明：conn_loop/2 的 {more,_} 分支用 5s 上限 recv 周期性醒查
%% last_inbound；任何入帧（含 HEARTBEAT）刷新之。10s 无帧 → 判 peer 死亡。
conn_loop(Sock, Buf0) ->
    case parse_frame(Buf0) of
        {ok, Ft, Flags, Cid, Path, Key, Payload, Tail} ->
            put(last_inbound, erlang:system_time(millisecond)),
            log("RX ft=~p cid=~p len=~p~n", [Ft, Cid, byte_size(Payload)]),
            handle(Sock, Ft, Flags, Cid, Path, Key, Payload),
            conn_loop(Sock, Tail);
        {more, Rest} ->
            Now = erlang:system_time(millisecond),
            Last = case get(last_inbound) of undefined -> Now; L -> L end,
            case Now - Last > 10000 of
                true ->
                    log("peer silent >10s — half-open, closing~n", []),
                    gen_tcp:close(Sock),
                    exit(hb_timeout);
                false ->
                    case gen_tcp:recv(Sock, 0, 5000) of
                        {ok, Data} ->
                            conn_loop(Sock, <<Rest/binary, Data/binary>>);
                        {error, closed} ->
                            log("connection closed~n", []),
                            exit(closed);
                        {error, timeout} ->
                            conn_loop(Sock, Rest);
                        {error, Reason} ->
                            log("recv error ~p~n", [Reason]),
                            exit(Reason)
                    end
            end
    end.

recv_frame(Sock, Buf) ->
    case parse_frame(Buf) of
        {ok, Ft, _Flags, Cid, Path, Key, Payload, _Tail} ->
            {ok, Ft, Cid, Path, Key, Payload};
        {more, Rest} ->
            case gen_tcp:recv(Sock, 0, 5000) of
                {ok, D} -> recv_frame(Sock, <<Rest/binary, D/binary>>);
                {error, E} -> {error, E}
            end
    end.

%% DEV_08 测试口：启动网关并直接返回端口（不打印——测试免 stdout 捕获）。
%% ServiceFun 可注入（默认 ?MODULE:service/2——测试注入慢实现验证并发结构）。
start(Port) -> start(Port, fun ?MODULE:service/2).
start(Port, ServiceFun) ->
    try ets:info(?DIRECT_TAB) =:= undefined andalso
        ets:new(?DIRECT_TAB, [named_table, public, {read_concurrency, true}])
    catch _:_ -> ok end,   %% 网关直连表（幂等——业务表已迁 app 组件）
    try admin_init() catch _:_ -> ok end,   %% B4：admin-v2 登记表（同幂等语义）
    Self = self(),
    _Gw = spawn(fun() ->
                        {ok, LSock} = gen_tcp:listen(Port, [binary, {packet, raw},
                                                            {active, false},
                                                            {nodelay, true},
                                                            {reuseaddr, true},
                                                            {send_timeout, 5000},
                                                            {send_timeout_close, true}]),
                        {ok, RealPort} = inet:port(LSock),
                        Self ! {gw_port, RealPort},
                        {ok, Sock} = gen_tcp:accept(LSock),
                        gen_tcp:close(LSock),
                        put(service_fun, ServiceFun),
                        log("rust node connected: ~p~n", [inet:peername(Sock)]),
                        loop(Sock, <<>>)
                end),
    receive {gw_port, P} -> {ok, P} after 5000 -> {error, gw_start_timeout} end.

log(Fmt, Args) ->
    %% 网关日志走 stderr（stdout 契约只留端口行）
    io:format(standard_error, Fmt, Args).

%% loop（被动模式）：Frame path 字段透传 handle（admin 回帧的 reply_to）
loop(Sock, Buf0) ->
    case parse_frame(Buf0) of
        {ok, Ft, Flags, Cid, Path, Key, Payload, Tail} ->
            handle(Sock, Ft, Flags, Cid, Path, Key, Payload),
            loop(Sock, Tail);
        {more, Rest} ->
            case gen_tcp:recv(Sock, 0, infinity) of
                {ok, Data} ->
                    loop(Sock, <<Rest/binary, Data/binary>>);
                {error, closed} ->
                    log("connection closed~n", []);
                {error, Reason} ->
                    log("recv error ~p~n", [Reason])
            end
    end.

handle(Sock, Ft, _Flags, Cid, Path, Key, Payload) ->
    case Ft of
        ?HANDSHAKE ->
            log("handshake received~n", []),
            AckBody = iolist_to_binary(handshake_ack_body("erl-gw-1")),
            gen_tcp:send(Sock, build_frame(?HANDSHAKE_ACK, Cid, <<"">>,
                                            <<"__handshake__">>,
                                            AckBody));
        ?HEARTBEAT ->
            gen_tcp:send(Sock, build_frame(?HEARTBEAT_ACK, Cid, <<"">>, <<"">>, <<>>));
        ?FT_SYSTEM_EVENT ->
            %% B4（DEV_09）：admin-v2 命令（0x03）——执行+回帧；其余 tag 忽略
            try admin_handle_frame(Sock, Cid, Path, Payload)
            catch Class:R:Stk -> log("admin frame error ~p:~p~n  at ~p~n", [Class, R, Stk])
            end;
        ?ROUTE_HINT ->
            %% 方案 A：hub 背书的直连地址 → 后台拨号（不阻塞收帧循环）。
            %% 已有该 node 直连则忽略（断连时 ets 清理由 dial 进程负责）。
            try
                {Node, Addr} = parse_route_hint(Payload),
                case ets:lookup(?DIRECT_TAB, Node) of
                    []   -> spawn(fun() -> direct_dial(Node, Addr) end);
                    [_]  -> ok
                end
            catch _:_ -> ok
            end;
        ?ASK ->
            %% 剥 reply_to 前缀（4B len + path；回程经 cid 配对）
            Real = split_reply_to(Payload),
            %% DEV_08：worker 进程执行（慢 service 不阻塞后续帧——OTP 每
            %% 进程一调度单元，正是 erlang 并发原生的形态）。Svc 闭包捕获
            %% （spawn 不继承 process dictionary——直接变量捕获）。
            Svc = case get(service_fun) of
                      F when is_function(F, 2) -> F;
                      _ -> fun ?MODULE:service/2
                  end,
            spawn(fun() ->
                          Res = try {ok, Svc(Key, Real)}
                                catch _:R -> {error, R} end,
                          case Res of
                              {ok, {RK, RP}} ->
                                  gen_tcp:send(Sock, build_frame(?REPLY, Cid, <<"">>, RK, RP));
                              {error, Reason} ->
                                  Err = unicode:characters_to_binary(
                                          io_lib:format("~p", [Reason])),
                                  gen_tcp:send(Sock, build_frame(?REPLY_ERR, Cid, <<"">>, <<"">>,
                                                                 err_payload(6, Err)))
                          end
                  end);
        ?TELL ->
            Svc2 = case get(service_fun) of
                       F2 when is_function(F2, 2) -> F2;
                       _ -> fun ?MODULE:service/2
                   end,
            spawn(fun() -> try Svc2(Key, Payload) catch _:_ -> ok end end);
        _ ->
            ok
    end.

split_reply_to(<<Len:32/little, _Skip:Len/binary, Rest/binary>>) when Len < 4096 ->
    Rest;
split_reply_to(B) -> B.

%% ============ 方案 A：直连学习 ============

%% ROUTE_HINT payload: [u16 node_len][node][u16 addr_len][addr]
parse_route_hint(<<NLen:16/little, Node:NLen/binary,
                   ALen:16/little, Addr:ALen/binary>>) ->
    {binary_to_list(Node), binary_to_list(Addr)};
parse_route_hint(_) -> erlang:error(bad_hint).

%% 直拨目标网关：完整客户端握手（对端 accept——所有网关 accept 侧都活着）
%% → ets 登记 → conn_loop 服务（同 hub 链路语义）。断连：ets 移除（下次
%% hint 或 hub 中转兜底重新学习）。退避 1s→30s 重试。
direct_dial(Node, Addr) ->
    [Host, PortS] = string:split(Addr, ":"),
    Port = list_to_integer(PortS),
    direct_dial_try(Node, Host, Port, 0).

direct_dial_try(Node, Host, Port, Attempt) ->
    Delay = case Attempt of 0 -> 0; N -> min(30000, 1000 * trunc(math:pow(2, min(N, 5)))) end,
    case Delay of 0 -> ok; D -> timer:sleep(D) end,
    case gen_tcp:connect(Host, Port, [binary, {packet, raw}, {active, false},
                                      {nodelay, true}], 5000) of
        {ok, Sock} ->
            Hs = iolist_to_binary(handshake_body("erl-gw-1")),
            case gen_tcp:send(Sock, build_frame(?HANDSHAKE, 1, <<"">>,
                                                <<"__handshake__">>, Hs)) of
                ok ->
                    case recv_frame(Sock, <<>>) of
                        {ok, ?HANDSHAKE_ACK, _, _, _, _} ->
                            ets:insert(?DIRECT_TAB, {Node, Sock}),
                            log("direct link to ~s up~n", [Node]),
                            conn_loop(Sock, <<>>),
                            %% conn_loop 退出 = 断连 → 清理后由 hint 重学习
                            ets:delete(?DIRECT_TAB, Node),
                            gen_tcp:close(Sock);
                        _ ->
                            gen_tcp:close(Sock),
                            direct_dial_try(Node, Host, Port, Attempt + 1)
                    end;
                {error, _} ->
                    gen_tcp:close(Sock),
                    direct_dial_try(Node, Host, Port, Attempt + 1)
            end;
        {error, Why} ->
            log("direct dial ~s failed: ~p~n", [Node, Why]),
            direct_dial_try(Node, Host, Port, Attempt + 1)
    end.
