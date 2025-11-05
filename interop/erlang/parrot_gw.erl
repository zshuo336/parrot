%% Parrot Erlang Gateway 模块本体（DEV_03 §7 / E6——POC 转正为 Wire 1.0）。
%%
%% Rust 节点 <--Wire 1.0(TLV 握手)--> 本网关 <--OTP 原生--> Erlang 进程
%%
%% 帧布局（07 §2.1，与 Rust/JVM/Python 逐字节一致）：
%%   外层: [u32 body_len LE]
%%   body: [ver u8][ft u8][flags u16 LE][cid u64 LE]
%%         [hop_count u8][hop_limit u8][sequence u32 LE][reserved u16 LE]
%%         [path_len u32 LE][path][key_len u32 LE][key][payload]
%%
%% 握手 TLV: tag u8 + len u16 LE + value（NODE_ID=1 CAPS=4 MAX_FRAME_LEN=5
%% TOPOLOGY_ROLE=6 HOP_LIMIT=7；DIRECT_ADDR=8 ACK 加 CHOSEN_CODEC=9）
-module(parrot_gw).
-behaviour(supervisor).
-include_lib("kernel/include/file.hrl").
-export([init/1, start_owned_work/1, start_owned_driver/3]).
-export([artifact_source/2, load_beam/2]).
-export([main/1, start/1, start/2, start/3, service/2, service/3, service/4, build_frame/5, build_frame/6,
         build_frame/9, parse_frame/1, parse_frame_full/1,
         handshake_body/1, handshake_body/2, handshake_ack_body/1,
         err_payload/2, parse_port/1, decode_handshake/2,
         %% B4（DEV_09）：admin-v2 编解码 + 四命令（测试直呼）
         encode_admin_cmd_v2/1, decode_admin_cmd_v2/1,
         encode_admin_reply_v2/1, decode_admin_reply_v2/1,
         admin_init/0, admin_deploy/1, admin_stop/1, admin_drain/2,
         admin_status/1, admin_instance_paths/2, instance_count/1,
         admin_metrics/0, admin_measured_metrics/0, read_rss/1,
         bc_put_varint/1, bc_get_varint/1]).

-define(VER, 1).
-define(MAX_FRAME_LEN, 16777216).
-define(GATEWAY_FRAME_LEN, 1048576).
-define(MAX_CONNECTIONS, 256).
-define(MAX_CONNECTION_WORKERS, 64).
-define(MAX_DIRECT_CHILDREN, 8).
-define(ASK, 16#10).
-define(REPLY, 16#11).
-define(REPLY_ERR, 16#12).
-define(ERROR, 16#7F).
-define(TELL, 16#13).
-define(TELL_ACK, 16#10).
-define(TRACE_CONTEXT, 16#40).
-define(CAPS_TRACE_CONTEXT, 8192).
-define(TELL_COMPLETION_KEY, <<"parrot:completion#v1">>).
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
    build_frame(Ft, Flags, Cid, 0, 8, 0, Path, Key, Payload).

%% General Wire codec; execution support is checked separately at ingress.
build_frame(Ft, Flags, Cid, HopCount, HopLimit, Sequence, Path, Key, Payload) ->
    PathB = to_bin(Path),
    KeyB = to_bin(Key),
    wire_require(known_frame_type(Ft), bad_frame_type),
    wire_require(is_integer(Flags) andalso Flags >= 0 andalso Flags =< 65535, bad_flags),
    wire_require(is_integer(Cid) andalso Cid >= 0 andalso Cid =< 16#FFFFFFFFFFFFFFFF, bad_correlation_id),
    wire_require(is_integer(HopCount) andalso is_integer(HopLimit) andalso
                 HopCount >= 0 andalso HopLimit > 0 andalso HopLimit =< 255 andalso
                 HopCount < HopLimit, invalid_hop),
    wire_require(is_integer(Sequence) andalso Sequence >= 0 andalso
                 Sequence =< 16#FFFFFFFF, bad_sequence),
    wire_utf8(PathB),
    wire_utf8(KeyB),
    %% Fixed body overhead includes both length fields.
    BodyLen = 28 + byte_size(PathB) + byte_size(KeyB) + byte_size(Payload),
    wire_require(BodyLen =< ?MAX_FRAME_LEN, frame_too_large),
    <<BodyLen:32/little,
      ?VER:8, Ft:8, Flags:16/little, Cid:64/little,
      HopCount:8, HopLimit:8, Sequence:32/little, 0:16/little,
      (byte_size(PathB)):32/little, PathB/binary,
      (byte_size(KeyB)):32/little, KeyB/binary,
      Payload/binary>>.

to_bin(B) when is_binary(B) -> B;
to_bin(L) when is_list(L) -> unicode:characters_to_binary(L).

wire_require(true, _) -> ok;
wire_require(false, Reason) -> erlang:error({invalid_wire_frame, Reason}).

known_frame_type(Ft) ->
    lists:member(Ft, [1, 2, 3, 4, 16#10, 16#11, 16#12, 16#13, 16#14, 16#15,
                     16#20, 16#21, 16#22, 16#23, 16#24, 16#7F]).

wire_utf8(Bin) when is_binary(Bin) ->
    wire_require(is_binary(unicode:characters_to_binary(Bin, utf8, utf8)), invalid_utf8).

wire_node_id(Id) ->
    wire_utf8(Id),
    wire_require(byte_size(Id) > 0 andalso byte_size(Id) =< 65535 andalso
                 binary:match(Id, <<0>>) =:= nomatch, invalid_node_id).

parse_frame(Buf) -> parse_frame(Buf, ?MAX_FRAME_LEN).

parse_frame(Buf, Limit) ->
    case parse_frame_full(Buf, Limit) of
        {ok, Frame, Tail} -> legacy_frame(Frame, Tail);
        More -> More
    end.

legacy_frame(#{ft:=Ft, flags:=Flags, cid:=Cid, path:=Path, key:=Key, payload:=Payload}, Tail) ->
    {ok, Ft, Flags, Cid, Path, Key, Payload, Tail}.

%% Preserve every public header field without changing the legacy tuple shape.
parse_frame_full(Buf) -> parse_frame_full(Buf, ?MAX_FRAME_LEN).

%% Validate length at the four-byte prefix, even before the body has arrived.
%% The full body is isolated before reading variable fields, so an invalid path
%% or key cannot borrow bytes from the next frame. Partial input is unchanged.
parse_frame_full(Buf, Limit) when is_binary(Buf) ->
    case Buf of
        <<BodyLen:32/little, _/binary>> when BodyLen < 28; BodyLen > Limit ->
            erlang:error({invalid_wire_frame, {body_length, BodyLen}});
        <<BodyLen:32/little, Rest/binary>> when byte_size(Rest) >= BodyLen ->
            <<Body:BodyLen/binary, Tail/binary>> = Rest,
            <<?VER:8, Ft:8, Flags:16/little, Cid:64/little,
              HopC:8, HopL:8, Sequence:32/little, Reserved:16/little,
              PathLen:32/little, PathB:PathLen/binary,
              KeyLen:32/little, KeyB:KeyLen/binary,
              Payload/binary>> = Body,
            wire_require(known_frame_type(Ft), bad_frame_type),
            wire_require(HopL > 0 andalso HopC < HopL, invalid_hop),
            wire_require(Reserved =:= 0, invalid_reserved),
            wire_utf8(PathB),
            wire_utf8(KeyB),
            {ok, #{ft=>Ft, flags=>Flags, cid=>Cid, hop_count=>HopC, hop_limit=>HopL,
                   sequence=>Sequence, path=>PathB, key=>KeyB, payload=>Payload}, Tail};
        _ -> {more, Buf}
    end.

%% A structurally valid frame is not necessarily executable. Neither batch,
%% compression nor encrypted business payloads may be interpreted as plain bytes.
parse_received_frame(Buf) ->
    %% Handshake metadata can exceed the eventual negotiated body limit.
    %% Consult the phase for every frame, including an already-buffered tail.
    Limit = case get(wire_phase) of
        active -> min(?GATEWAY_FRAME_LEN, get(peer_limit));
        _ -> ?MAX_FRAME_LEN
    end,
    case parse_frame_full(Buf, Limit) of
        {ok, #{ft:=Ft, flags:=Flags, cid:=Cid, sequence:=Sequence,
               path:=Path, key:=Key, payload:=Payload}=Frame, Tail} ->
            metrics_count(Ft, Path, Key, Payload),
            Supported = 2 bor 8 bor ?TELL_ACK bor ?TRACE_CONTEXT,
            wire_require(Flags band (bnot Supported) =:= 0, unsupported_payload_flags),
            wire_require(Flags band ?TRACE_CONTEXT =:= 0 orelse
                         (get(peer_trace) =:= true andalso (Ft =:= ?ASK orelse Ft =:= ?TELL)),
                         causal_context_not_negotiated_or_business),
            case Flags band ?TRACE_CONTEXT of 0 -> ok; _ -> parrot_trace:split(Payload) end,
            wire_require(Flags band ?TELL_ACK =:= 0 orelse
                         (Ft =:= ?TELL andalso Cid =/= 0 andalso Sequence =:= 0),
                         invalid_completion_identity),
            wire_require(Key =/= <<"parrot:durable#v1">>, durable_delivery_not_supported),
            legacy_frame(Frame, Tail);
        More -> More
    end.

%% ============ 握手 TLV ============

tlv(Tag, V) ->
    wire_require(byte_size(V) =< 65535, tlv_too_long),
    <<Tag:8, (byte_size(V)):16/little, V/binary>>.

caps_pb_only() -> 16#03.  %% bin+pb 双栈（07 §8.1 原始 pb-only；crawler-lab
                           %% 起 bin: 裸键载荷也走本网关——能力位放宽为双栈）

handshake_body(NodeId) ->
    Id = to_bin(NodeId),
    wire_node_id(Id),
    [tlv(1, Id),
     tlv(4, <<(caps_pb_only()):32/little>>),
     tlv(5, <<?GATEWAY_FRAME_LEN:32/little>>),
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



%% Strict shared server/client handshake validation. Unknown tags remain extensible.
decode_handshake(Body, Ack) ->
    Fields = decode_tlvs(Body, #{}),
    Node = maps:get(1, Fields),
    wire_node_id(Node),
    [wire_utf8(V) || Tag <- [2, 3, 8], {ok, V} <- [maps:find(Tag, Fields)]],
    <<Caps:32/little>> = maps:get(4, Fields),
    Common = Caps band 3,
    wire_require(Common =/= 0, no_common_codec),
    <<Limit:32/little>> = maps:get(5, Fields),
    wire_require(Limit >= 28, invalid_peer_limit),
    <<Role>> = maps:get(6, Fields),
    wire_require(Role =< 3, invalid_role),
    case maps:find(7, Fields) of
        error -> ok;
        {ok, <<Hop>>} when Hop > 0 -> ok;
        _ -> erlang:error(invalid_handshake_hop)
    end,
    Chosen = case Ack of
        true -> maps:get(9, Fields, <<"bin">>);
        false ->
            wire_require(not maps:is_key(9, Fields), ack_only_tag),
            case Common band 1 of 1 -> <<"bin">>; _ -> <<"pb">> end
    end,
    wire_require((Chosen =:= <<"bin">> andalso Common band 1 =/= 0) orelse
                 (Chosen =:= <<"pb">> andalso Common band 2 =/= 0), invalid_codec),
    #{node => Node, limit => Limit, codec => Chosen, capabilities => Caps}.

decode_tlvs(<<>>, Acc) -> Acc;
decode_tlvs(<<Tag, Len:16/little, Value:Len/binary, Tail/binary>>, Acc) ->
    wire_require(not (Tag >= 1 andalso Tag =< 9 andalso maps:is_key(Tag, Acc)), duplicate_tlv),
    decode_tlvs(Tail, Acc#{Tag => Value});
decode_tlvs(_, _) -> erlang:error(truncated_tlv).

handshake_ack_body(NodeId, Codec) ->
    gateway_handshake_body(NodeId) ++ [tlv(9, Codec)].

%% Actual gateway capabilities; generic handshake helpers promise neither
%% admin metrics nor handler completion.
gateway_handshake_body(NodeId) ->
    [case Field of
        <<4, 4:16/little, Caps:32/little>> -> tlv(4, <<(Caps bor 1024 bor 256 bor ?CAPS_TRACE_CONTEXT):32/little>>);
        _ -> Field
     end || Field <- handshake_body(NodeId)].

now_ms() -> erlang:monotonic_time(millisecond).

begin_connection() ->
    put(wire_phase, handshake),
    put(wire_deadline, now_ms() + 10000),
    put(last_inbound, now_ms()),
    put(peer_trace, false),
    put(peer_limit, ?MAX_FRAME_LEN),
    lease_call(begin_lease).

complete_connection() ->
    ok = lease_call(complete),
    put(wire_phase, active),
    put(last_inbound, now_ms()),
    ok.

with_connection(Sock, F) ->
    try
        begin_connection(),
        case get(task_sup) of
            undefined ->
                {ok, Sup} = supervisor:start_link(?MODULE, connection),
                put(task_sup, Sup), put(owns_task_sup, true);
            _ -> put(owns_task_sup, false)
        end,
        F()
    catch C:R -> log("connection exit ~p:~p~n", [C, R])
    after
        gen_tcp:close(Sock),
        retire_connection_tasks(),
        lease_call(retire)
    end.

%% A real OTP tree: a direct connection is a child supervisor, so parent
%% retirement joins all descendants rather than just killing the dialer PID.
init(connection) ->
    {ok, {#{strategy => one_for_one, intensity => 0, period => 1}, []}};
init({direct, Node, Addr}) ->
    Driver = #{id => driver, start => {?MODULE, start_owned_driver, [self(), Node, Addr]},
               restart => temporary, shutdown => brutal_kill, type => worker, significant => true},
    {ok, {#{strategy => one_for_one, intensity => 0, period => 1,
            auto_shutdown => any_significant}, [Driver]}}.

start_owned_work(Fun) -> {ok, spawn_link(Fun)}.
start_owned_driver(Sup, Node, Addr) ->
    {ok, spawn_link(fun() -> put(task_sup, Sup), direct_dial(Node, Addr) end)}.

admit_work(Fun) ->
    Sup = get(task_sup),
    Children = supervisor:which_children(Sup),
    Workers = [Pid || {{work, _}, Pid, _, _} <- Children],
    case length(Workers) < ?MAX_CONNECTION_WORKERS of
        true -> supervisor:start_child(Sup, #{id => {work, make_ref()},
            start => {?MODULE, start_owned_work, [Fun]}, restart => temporary,
            shutdown => brutal_kill, type => worker});
        false -> {error, overloaded}
    end.

admit_direct(Node, Addr) ->
    Sup = get(task_sup),
    Children = supervisor:which_children(Sup),
    Direct = [Id || {{direct, _} = Id, _, _, _} <- Children],
    case lists:member({direct, Node}, Direct) of
        true -> ok;
        false when length(Direct) < ?MAX_DIRECT_CHILDREN ->
            supervisor:start_child(Sup, #{id => {direct, Node},
                start => {supervisor, start_link, [?MODULE, {direct, Node, Addr}]},
                restart => temporary, shutdown => infinity, type => supervisor});
        false -> {error, overloaded}
    end.

retire_connection_tasks() ->
    case get(task_sup) of
        undefined -> ok;
        Sup ->
            case erase(owns_task_sup) of
                true ->
                    %% normal avoids killing the linked driver while it awaits
                    %% the supervisor's actual shutdown receipt.
                    ok = gen_server:stop(Sup, normal, infinity),
                    erase(task_sup), ok;
                _ ->
                    %% A direct driver lives inside its supervisor. Retire its
                    %% descendants before returning; significant driver exit
                    %% then retires this now-empty branch supervisor.
                    [begin
                        case supervisor:terminate_child(Sup, Id) of
                            ok -> ok;
                            {error, not_found} -> ok
                        end
                     end || {Id, _, _, _} <- supervisor:which_children(Sup), Id =/= driver],
                    ok
            end
    end.

wire_send(Sock, Data0) ->
    Data = iolist_to_binary(Data0),
    Limit = case get(peer_limit) of undefined -> ?MAX_FRAME_LEN; L -> L end,
    case byte_size(Data) - 4 =< Limit of
        true -> ok;
        false -> gen_tcp:close(Sock), erlang:error(peer_frame_limit)
    end,
    case gen_tcp:send(Sock, Data) of
        ok ->
            mbump(bytes_tx, byte_size(Data)),
            <<_:4/binary, _Version, Ft, _/binary>> = Data,
            case Ft of
                ?REPLY -> mbump(replies, 1);
                ?REPLY_ERR -> mbump(replies, 1), mbump(r_errs, 1);
                _ -> ok
            end,
            ok;
        {error, Reason} -> gen_tcp:close(Sock), erlang:error({send_failed, Reason})
    end.

client_handshake(Sock, ExpectedNode) ->
    Hs = iolist_to_binary(gateway_handshake_body(gateway_node_id())),
    ok = wire_send(Sock, build_frame(?HANDSHAKE, 1, <<>>, <<"__handshake__">>, Hs)),
    {ok, ?HANDSHAKE_ACK, _Cid, _Path, _Key, Payload, Tail} = recv_frame(Sock, <<>>),
    Peer = decode_handshake(Payload, true),
    wire_require(ExpectedNode =:= any orelse maps:get(node, Peer) =:= to_bin(ExpectedNode), wrong_direct_peer),
    put(peer_limit, min(?GATEWAY_FRAME_LEN, maps:get(limit, Peer))),
    put(peer_trace, maps:get(capabilities,Peer) band ?CAPS_TRACE_CONTEXT =/= 0),
    complete_connection(),
    Tail.

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
%% 观测五件套：跨连接指标计数表（key → 累计整数）
-define(METRICS_TAB, parrot_gw_metrics).

%% ---- varint ----

bc_put_varint(V) when is_integer(V), V >= 0, V =< 250 -> <<V:8>>;
bc_put_varint(V) when is_integer(V), V >= 0, V =< 16#FFFF -> <<16#FB:8, V:16/little>>;
bc_put_varint(V) when is_integer(V), V >= 0, V =< 16#FFFFFFFF -> <<16#FC:8, V:32/little>>;
bc_put_varint(V) when is_integer(V), V >= 0, V =< 16#FFFFFFFFFFFFFFFF -> <<16#FD:8, V:64/little>>;
bc_put_varint(_) -> erlang:error(invalid_u64).

bc_get_varint(<<B0:8, Rest/binary>>) when B0 =< 16#FA -> {B0, Rest};
bc_get_varint(<<16#FB:8, V:16/little, Rest/binary>>) -> {V, Rest};
bc_get_varint(<<16#FC:8, V:32/little, Rest/binary>>) -> {V, Rest};
bc_get_varint(<<16#FD:8, V:64/little, Rest/binary>>) -> {V, Rest}.

%% ---- 基础 serde 形态 ----

bc_put_uint(V, Max) when is_integer(V), V >= 0, V =< Max -> bc_put_varint(V);
bc_put_uint(_, _) -> erlang:error(integer_out_of_range).

bc_get_uint(<<Tag, _/binary>>, Width) when Tag =:= 16#FC, Width < 4;
                                            Tag =:= 16#FD, Width < 8 ->
    erlang:error(integer_prefix_too_wide);
bc_get_uint(Bin, _) -> bc_get_varint(Bin).

bc_put_str(S) -> B = to_bin(S), wire_utf8(B), <<(bc_put_varint(byte_size(B)))/binary, B/binary>>.

bc_get_str(Bin0) ->
    {N, Bin1} = bc_get_varint(Bin0),
    <<S:N/binary, Rest/binary>> = Bin1,
    wire_utf8(S),
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
      (bc_put_uint(Abi, 16#FFFFFFFF))/binary>>.

bc_put_opt_str(undefined) -> <<0:8>>;
bc_put_opt_str(S) -> <<1:8, (bc_put_str(S))/binary>>.

bc_get_opt_str(<<0:8, Rest/binary>>) -> {undefined, Rest};
bc_get_opt_str(<<1:8, Bin0/binary>>) ->
    {S, Rest} = bc_get_str(Bin0),
    {S, Rest}.

bc_get_artifact(Bin) ->
    {Tag, Rest} = bc_get_uint(Bin, 4),
    bc_get_artifact_variant(Tag, Rest).

bc_get_artifact_variant(0, Bin0) ->
    {F, Rest} = bc_get_str(Bin0), {{props, F}, Rest};
bc_get_artifact_variant(1, Bin0) ->
    {App, Bin1} = bc_get_str(Bin0),
    {Uri, Rest} = bc_get_opt_str(Bin1),
    {{beam, App, Uri}, Rest};
bc_get_artifact_variant(2, Bin0) ->
    {M, Bin1} = bc_get_str(Bin0),
    {Env, Bin2} = bc_get_opt_str(Bin1),
    {Uri, Rest} = bc_get_opt_str(Bin2),
    {{pymodule, M, Env, Uri}, Rest};
bc_get_artifact_variant(3, Bin0) ->
    {MC, Bin1} = bc_get_str(Bin0),
    {Coords, Bin2} = bc_get_opt_str(Bin1),
    {Uri, Rest} = bc_get_opt_str(Bin2),
    {{jvm, MC, Coords, Uri}, Rest};
bc_get_artifact_variant(4, Bin0) ->
    {D, Bin1} = bc_get_str(Bin0), {U, Rest} = bc_get_str(Bin1),
    {{wasm, D, U}, Rest};
bc_get_artifact_variant(5, Bin0) ->
    {D, Bin1} = bc_get_str(Bin0), {U, Bin2} = bc_get_str(Bin1),
    {Abi, Rest} = bc_get_uint(Bin2, 4),
    {{dylib, D, U, Abi}, Rest}.

%% ---- AdminInstancePolicy（0=Singleton 1=Pool 2=Sharded）----

bc_put_policy(singleton) -> <<0:8>>;
bc_put_policy({pool, N}) -> <<1:8, (bc_put_varint(N))/binary>>;
bc_put_policy({sharded, N}) -> <<2:8, (bc_put_varint(N))/binary>>.

bc_get_policy(Bin) ->
    {Tag, Rest} = bc_get_uint(Bin, 4),
    bc_get_policy_variant(Tag, Rest).

bc_get_policy_variant(0, Rest) -> {singleton, Rest};
bc_get_policy_variant(1, Bin0) ->
    {N, Rest} = bc_get_varint(Bin0), {{pool, N}, Rest};
bc_get_policy_variant(2, Bin0) ->
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
    <<3:8, (bc_put_varint(ReqId))/binary, (bc_put_str(Prefix))/binary>>;
enc_cmd_body({metrics_report, ReqId}) ->
    <<4:8, (bc_put_varint(ReqId))/binary>>;
enc_cmd_body({measured_metrics_report, ReqId}) ->
    <<5:8, (bc_put_varint(ReqId))/binary>>.

decode_admin_cmd_v2(<<?TAG_ADMIN_CMD_V2:8, Body/binary>>) ->
    {Cmd, <<>>} = dec_cmd_body(Body),
    Cmd.

dec_cmd_body(Bin) ->
    {Tag, Rest} = bc_get_uint(Bin, 4),
    dec_cmd_body_variant(Tag, Rest).

dec_cmd_body_variant(0, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Deploy, Rest} = bc_get_deploy(Bin1),
    {{deploy_component, ReqId, Deploy}, Rest};
dec_cmd_body_variant(1, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Prefix, Bin2} = bc_get_str(Bin1),
    {TimeoutMs, Rest} = bc_get_varint(Bin2),
    {{drain_component, ReqId, Prefix, TimeoutMs}, Rest};
dec_cmd_body_variant(2, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Prefix, Rest} = bc_get_str(Bin1),
    {{stop_component, ReqId, Prefix}, Rest};
dec_cmd_body_variant(3, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Prefix, Rest} = bc_get_str(Bin1),
    {{component_status, ReqId, Prefix}, Rest};
dec_cmd_body_variant(4, Bin0) ->
    {ReqId, Rest} = bc_get_varint(Bin0),
    {{metrics_report, ReqId}, Rest};
dec_cmd_body_variant(5, Bin0) ->
    {ReqId, Rest} = bc_get_varint(Bin0),
    {{measured_metrics_report, ReqId}, Rest}.

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
    <<4:8, (bc_put_varint(ReqId))/binary, (bc_put_uint(Code, 16#FFFF))/binary,
      (bc_put_str(Detail))/binary>>;
%% {metrics, ReqId, Snapshot}——观测五件套（字段序 = Rust MetricsSnapshot 声明序）
enc_reply_body({metrics, ReqId, Snap}) ->
    <<5:8, (bc_put_varint(ReqId))/binary, (bc_put_snapshot(Snap))/binary>>;
enc_reply_body({measured_metrics, ReqId, Metrics}) ->
    validate_measured(Metrics),
    #{snapshot := Snap, available := Available, epoch := Epoch, process_kind := Unit} = Metrics,
    <<6, (bc_put_varint(ReqId))/binary, (bc_put_snapshot(Snap))/binary,
      (bc_put_varint(Available))/binary, (bc_put_str(Epoch))/binary, (bc_put_str(Unit))/binary>>.

validate_measured(#{available := Mask, epoch := Epoch, process_kind := Unit}) ->
    wire_require(is_integer(Mask) andalso Mask >= 0 andalso Mask < (1 bsl 17), invalid_metrics_mask),
    [begin
        wire_utf8(B),
        wire_require(byte_size(B) =< 128, metrics_identity_too_long)
     end || B <- [Epoch, Unit]],
    wire_require(Mask band (1 bsl 14) =:= 0 orelse byte_size(Unit) > 0, missing_process_unit).

bc_put_snapshot(Snap) ->
    TsMs       = maps:get(ts_ms, Snap, 0),
    Runtime    = maps:get(runtime, Snap, <<>>),
    Conn       = maps:get(connections, Snap, 0),
    HsOk       = maps:get(handshakes_ok, Snap, 0),
    HsFail     = maps:get(handshakes_failed, Snap, 0),
    Asks       = maps:get(asks_rx, Snap, 0),
    Tells      = maps:get(tells_rx, Snap, 0),
    Replies    = maps:get(replies_tx, Snap, 0),
    RErrs      = maps:get(reply_errs, Snap, 0),
    BRx        = maps:get(bytes_rx, Snap, 0),
    BTx        = maps:get(bytes_tx, Snap, 0),
    HbRx       = maps:get(heartbeats_rx, Snap, 0),
    Comps      = maps:get(components, Snap, 0),
    States     = maps:get(component_states, Snap, []),
    Procs      = maps:get(processes, Snap, 0),
    Rss        = maps:get(memory_rss, Snap, 0),
    Uptime     = maps:get(uptime_start_ms, Snap, 0),
    N = length(States),
    StateBin = << <<(bc_put_str(P))/binary, (bc_put_str(S))/binary,
                    (bc_put_str(V))/binary>> || {P, S, V} <- States >>,
    <<(bc_put_varint(TsMs))/binary, (bc_put_str(Runtime))/binary,
      (bc_put_varint(Conn))/binary, (bc_put_varint(HsOk))/binary,
      (bc_put_varint(HsFail))/binary, (bc_put_varint(Asks))/binary,
      (bc_put_varint(Tells))/binary, (bc_put_varint(Replies))/binary,
      (bc_put_varint(RErrs))/binary, (bc_put_varint(BRx))/binary,
      (bc_put_varint(BTx))/binary, (bc_put_varint(HbRx))/binary,
      (bc_put_varint(Comps))/binary, (bc_put_varint(N))/binary, StateBin/binary,
      (bc_put_varint(Procs))/binary, (bc_put_varint(Rss))/binary,
      (bc_put_varint(Uptime))/binary>>.

decode_admin_reply_v2(<<?TAG_ADMIN_REPLY_V2:8, Body/binary>>) ->
    {Reply, <<>>} = dec_reply_body(Body),
    Reply.

dec_reply_body(Bin) ->
    {Tag, Rest} = bc_get_uint(Bin, 4),
    dec_reply_body_variant(Tag, Rest).

dec_reply_body_variant(0, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {N, Bin2} = bc_get_varint(Bin1),
    {Paths, Rest} = bc_get_str_list(N, Bin2),
    {{deployed, ReqId, Paths}, Rest};
dec_reply_body_variant(1, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {D, Bin2} = bc_get_varint(Bin1),
    {A, Rest} = bc_get_varint(Bin2),
    {{drained, ReqId, D, A}, Rest};
dec_reply_body_variant(2, Bin0) ->
    {ReqId, Rest} = bc_get_varint(Bin0),
    {{stopped, ReqId}, Rest};
dec_reply_body_variant(3, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {N, Bin2} = bc_get_varint(Bin1),
    {States, Rest} = bc_get_states(N, Bin2),
    {{status, ReqId, States}, Rest};
dec_reply_body_variant(4, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Code, Bin2} = bc_get_uint(Bin1, 2),
    {Detail, Rest} = bc_get_str(Bin2),
    {{failed, ReqId, Code, Detail}, Rest};
dec_reply_body_variant(5, Bin0) ->
    {ReqId, Bin1} = bc_get_varint(Bin0),
    {Snap, Rest} = bc_get_snapshot(Bin1),
    {{metrics, ReqId, Snap}, Rest};
dec_reply_body_variant(6, Bin0) ->
    {ReqId, B1} = bc_get_varint(Bin0),
    {Snap, B2} = bc_get_snapshot(B1),
    {Mask, B3} = bc_get_varint(B2),
    {Epoch, B4} = bc_get_str(B3),
    {Unit, Rest} = bc_get_str(B4),
    Metrics = #{snapshot => Snap, available => Mask, epoch => Epoch, process_kind => Unit},
    validate_measured(Metrics),
    {{measured_metrics, ReqId, Metrics}, Rest}.

bc_get_snapshot(Bin0) ->
    {TsMs,    B1} = bc_get_varint(Bin0),
    {Runtime, B2} = bc_get_str(B1),
    {Conn,    B3} = bc_get_varint(B2),
    {HsOk,    B4} = bc_get_varint(B3),
    {HsFail,  B5} = bc_get_varint(B4),
    {Asks,    B6} = bc_get_varint(B5),
    {Tells,   B7} = bc_get_varint(B6),
    {Replies, B8} = bc_get_varint(B7),
    {RErrs,   B9} = bc_get_varint(B8),
    {BRx,     B10} = bc_get_varint(B9),
    {BTx,     B11} = bc_get_varint(B10),
    {HbRx,    B12} = bc_get_varint(B11),
    {Comps,   B13} = bc_get_varint(B12),
    {N,       B14} = bc_get_varint(B13),
    {States,  B15} = bc_get_states(N, B14),
    {Procs,   B16} = bc_get_varint(B15),
    {Rss,     B17} = bc_get_varint(B16),
    {Uptime,  Rest} = bc_get_varint(B17),
    Snap = #{ts_ms => TsMs, runtime => Runtime, connections => Conn,
             handshakes_ok => HsOk, handshakes_failed => HsFail,
             asks_rx => Asks, tells_rx => Tells, replies_tx => Replies,
             reply_errs => RErrs, bytes_rx => BRx, bytes_tx => BTx,
             heartbeats_rx => HbRx, components => Comps,
             component_states => States, processes => Procs,
             memory_rss => Rss, uptime_start_ms => Uptime},
    {Snap, Rest}.

bc_get_str_list(N, Bin) when N >= 0, N =< byte_size(Bin) -> bc_get_str_list(N, Bin, []);
bc_get_str_list(_, _) -> erlang:error(truncated_string_list).

bc_get_str_list(0, Bin, Acc) -> {lists:reverse(Acc), Bin};
bc_get_str_list(N, Bin0, Acc) ->
    {S, Bin1} = bc_get_str(Bin0),
    bc_get_str_list(N - 1, Bin1, [S | Acc]).

bc_get_states(N, Bin) when N >= 0, N =< byte_size(Bin) div 3 -> bc_get_states(N, Bin, []);
bc_get_states(_, _) -> erlang:error(truncated_states).

bc_get_states(0, Bin, Acc) -> {lists:reverse(Acc), Bin};
bc_get_states(N, Bin0, Acc) ->
    {P, Bin1} = bc_get_str(Bin0),
    {S, Bin2} = bc_get_str(Bin1),
    {V, Bin3} = bc_get_str(Bin2),
    bc_get_states(N - 1, Bin3, [{P, S, V} | Acc]).

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

%% Management is owned by a gateway-lifetime coordinator, not its TCP caller.
admin_deploy({Name, Version, {beam, App, Uri}, Instances, Config}) ->
    try
        true=valid_component_name(Name,256),
        true=valid_component_name(App,255),
        true=is_binary(Version) andalso byte_size(Version)=<256
            andalso is_binary(unicode:characters_to_binary(Version,utf8,utf8)),
        N=instance_count(Instances),
        true=is_integer(N) andalso N>0 andalso N=<4096,
        admin_init(),ensure_component_runtime(),
        Paths=admin_instance_paths(Name,Instances),
        case parrot_component_runtime:deploy(Name,Version,Paths,fun() -> parrot_beam_host:start(self(),App,Uri,Name,Version,Paths,Config) end,Config) of
            {deployed,_}=Reply -> Reply;
            {error,component_management_overloaded} -> {failed,10,<<"component management capacity exceeded">>};
            {error,{error,beam_host_overloaded}} -> {failed,10,<<"BEAM host or snapshot capacity exceeded">>};
            {error,R} -> erlang:error(R)
        end
    catch _:Reason -> {failed,?V2_SPAWN_FAILED,
        unicode:characters_to_binary(io_lib:format("deploy: ~P",[Reason,8],[{chars_limit,1024}]))} end;
admin_deploy({_Name,_V,Other,_I,_C}) ->
    {failed,?V2_DIALECT_MISMATCH,iolist_to_binary(io_lib:format("erl executor expects Beam, got ~p",[Other]))}.
%% Check before expanding pool paths or converting an application to an atom.
valid_component_name(Name,Limit) when is_binary(Name),byte_size(Name)>0,byte_size(Name)=<Limit ->
    case unicode:characters_to_list(Name,utf8) of
        Chars when is_list(Chars) ->
            lists:all(fun(C) -> C>32 andalso C=/=127 andalso not lists:member(C,"/\\?#") end,Chars);
        _ -> false
    end;
valid_component_name(_,_) -> false.
ensure_component_runtime() -> parrot_component_runtime:ensure(ets:info(?ADMIN_TAB,owner)).
%% Resolve a concrete file before interning any request-controlled module name.
%% Explicit URIs never fall back to the environment or an earlier deployment.
artifact_source(App,Uri) ->
    Dirs=case Uri of
        undefined -> artifact_dirs(App)++code:get_path();
        _ -> [artifact_uri_dir(Uri)]
    end,
    File=artifact_file(Dirs,artifact_native_name(App)++".beam"),
    Beam=read_artifact(File),
    App=beam_module_name(Beam),{File,Beam}.

load_beam(App,Uri) ->
    {File,Beam}=artifact_source(App,Uri),
    Module=binary_to_atom(App,utf8),
    case code:prepare_loading([{Module,File,Beam}]) of
        {ok,Prepared} ->
            Same=same_loaded_artifact(Module,File,Beam),
            case Same of
                true -> ok; % Reusing code still creates fresh instance state.
                false ->
                    %% Never kill a process using old code to make an upgrade fit.
                    case code:soft_purge(Module) of
                        true -> ok;
                        false -> erlang:error({old_code_in_use,App})
                    end,
                    case code:finish_loading(Prepared) of
                        ok -> ok;
                        {error,Why} -> erlang:error({finish_loading,Why})
                    end
            end,
            %% Dependencies use the selected directory only after a successful
            %% code load. Code paths remain VM-global.
            true=code:add_patha(filename:dirname(File)),Module;
        {error,Why} -> erlang:error({prepare_loading,Why})
    end.

%% OTP's executable checksum excludes attributes and compile metadata. They
%% are observable via module_info and must participate in the reuse decision.
%% Missing metadata conservatively reloads rather than guessing equivalence.
same_loaded_artifact(Module,File,Beam) ->
    try
        erlang:get_module_info(Module,md5)=:=code:module_md5(Beam) andalso
        filename:absname(code:which(Module))=:=filename:absname(File) andalso
        case beam_lib:chunks(Beam,["Attr","CInf"]) of
            {ok,{Module,[{"Attr",Attrs},{"CInf",Compile}]}} ->
                %% The symbolic attributes reader sorts and merges entries;
                %% module_info exposes the original order and duplicates.
                erlang:get_module_info(Module,attributes)=:=binary_to_term(Attrs) andalso
                erlang:get_module_info(Module,compile)=:=binary_to_term(Compile);
            _ -> false
        end
    catch _:_ -> false end.

artifact_uri_dir(Uri) when is_binary(Uri),byte_size(Uri)=<8192 ->
    #{scheme:=<<"file">>,path:=Encoded}=Parts=uri_string:parse(Uri),
    true=lists:all(fun(K) -> lists:member(K,[scheme,host,path]) end,maps:keys(Parts)),
    true=lists:member(maps:get(host,Parts,<<>>),[<<>>,<<"localhost">>]),
    <<"/",_/binary>>=Encoded,
    %% uri_string's percent decoder tolerates stray '%' in some OTP versions.
    true=valid_percent_encoding(Encoded),
    Decoded=uri_string:percent_decode(Encoded),
    true=is_binary(Decoded),
    Chars=unicode:characters_to_list(Decoded,utf8),
    true=is_list(Chars),
    true=lists:all(fun(C) -> C>=32 andalso C=/=127 end,Chars),
    absolute=filename:pathtype(Chars),artifact_native_name(Decoded);
artifact_uri_dir(_) -> erlang:error(invalid_artifact_uri).
valid_percent_encoding(<<"%",A,B,Rest/binary>>) ->
    is_hex(A) andalso is_hex(B) andalso valid_percent_encoding(Rest);
valid_percent_encoding(<<"%",_/binary>>) -> false;
valid_percent_encoding(<<_,Rest/binary>>) -> valid_percent_encoding(Rest);
valid_percent_encoding(<<>>) -> true.
is_hex(C) -> (C>=$0 andalso C=<$9) orelse (C>=$a andalso C=<$f) orelse (C>=$A andalso C=<$F).

artifact_file([Dir|Dirs],Base) ->
    File=filename:join(Dir,Base),
    case file:read_link_info(File) of
        {ok,#file_info{type=regular}} -> File;
        {error,enoent} -> artifact_file(Dirs,Base);
        {error,enotdir} -> artifact_file(Dirs,Base);
        Other -> erlang:error({invalid_artifact_file,File,Other})
    end;
artifact_file([],_) -> erlang:error(artifact_not_found).

%% Cap bytes read even when a regular file grows after the metadata check.
%% The trusted local directory must not be concurrently swapped for a FIFO;
%% Erlang's portable file API does not provide openat/O_NOFOLLOW here.
read_artifact(File) ->
    {ok,#file_info{type=regular,size=Size}}=file:read_link_info(File),
    true=Size>0 andalso Size=<16777216,
    {ok,Fd}=file:open(File,[read,binary,raw]),
    try
        {ok,#file_info{type=regular}}=file:read_file_info(Fd),
        {ok,Beam}=file:read(Fd,16777217),
        true=byte_size(Beam)=<16777216,Beam
    after file:close(Fd) end.

%% Inspect the IFF header and first atom without beam_lib, which interns the
%% module atom even for inspection. OTP remains the authority on executable
%% validity. Support the classic Atom/AtU8 and OTP 28+ long-atom encoding.
beam_module_name(<<"FOR1",Size:32/big,"BEAM",Chunks/binary>>) when Size=:=byte_size(Chunks)+4 ->
    beam_module_chunks(Chunks,undefined);
beam_module_name(_) -> erlang:error(invalid_beam_header).
beam_module_chunks(<<>>,Name) when is_binary(Name) -> Name;
beam_module_chunks(<<Id:4/binary,Size:32/big,Rest/binary>>,Name) ->
    Pad=(4-Size rem 4) rem 4,
    <<Data:Size/binary,_:Pad/binary,Tail/binary>>=Rest,
    Next=case Id of
        <<"AtU8">> when Name=:=undefined -> beam_first_atom(Data,utf8);
        <<"Atom">> when Name=:=undefined -> beam_first_atom(Data,latin1);
        <<"AtU8">> -> erlang:error(duplicate_atom_chunk);
        <<"Atom">> -> erlang:error(duplicate_atom_chunk);
        _ -> Name
    end,
    beam_module_chunks(Tail,Next);
beam_module_chunks(_,_) -> erlang:error(invalid_beam_chunks).
beam_first_atom(<<Count:32/signed-big,Data/binary>>,utf8) when Count<0 ->
    {Len,Rest}=beam_atom_length(Data),
    <<Name:Len/binary,_/binary>>=Rest,
    beam_name_utf8(Name,utf8);
beam_first_atom(<<Count:32/big,Len,Name:Len/binary,_/binary>>,Encoding) when Count>0 ->
    beam_name_utf8(Name,Encoding);
beam_first_atom(_,_) -> erlang:error(invalid_atom_chunk).
beam_atom_length(<<N:4,0:1,_Tag:3,Rest/binary>>) -> {N,Rest};
beam_atom_length(<<High:3,0:1,1:1,_Tag:3,Low,Rest/binary>>) -> {(High bsl 8) bor Low,Rest}.
beam_name_utf8(Name,Encoding) ->
    Utf8=unicode:characters_to_binary(Name,Encoding,utf8),
    true=is_binary(Utf8) andalso byte_size(Utf8)>0,Utf8.

%% Unix VMs started under a C locale may use latin1 native filenames. Preserve
%% UTF-8 URI bytes on disk instead of passing unrepresentable Unicode codepoints
%% to the file API (or requiring a process-wide +fnu flag).
artifact_native_name(Utf8) ->
    case file:native_name_encoding() of
        utf8 -> unicode:characters_to_list(Utf8,utf8);
        latin1 -> binary_to_list(Utf8)
    end.

artifact_dirs(App) ->
    EnvDir=case os:getenv("PARROT_ARTIFACT_DIR") of
        false -> "/tmp/parrot-artifacts";
        D -> D
    end,
    [filename:join(EnvDir,artifact_native_name(App)),EnvDir].

%% Stop confirms process death; Drain closes admission and waits for accepted
%% work, force-stopping timed-out instances and reporting them as aborted.
admin_stop(Prefix) -> retire_components(Prefix,stop,0).
admin_drain(Prefix,TimeoutMs) -> retire_components(Prefix,drain,TimeoutMs).
retire_components(Prefix,Mode,TimeoutMs) ->
    try
        true=is_integer(TimeoutMs) andalso TimeoutMs>=0 andalso TimeoutMs=<60000,
        ensure_component_runtime(),
        case parrot_component_runtime:retire(Prefix,Mode,TimeoutMs) of
            not_found ->
                case parrot_beam_host:retire_orphans(Prefix) of
                    not_found -> {failed,?V2_COMPONENT_NOT_FOUND,<<"component not found">>};
                    ok -> retirement_reply(Mode,0,0);
                    Error -> erlang:error(Error)
                end;
            {retired,D,A} ->
                case parrot_beam_host:retire_orphans(Prefix) of
                    not_found -> retirement_reply(Mode,D,A);
                    ok -> retirement_reply(Mode,D,A);
                    Error -> erlang:error(Error)
                end;
            {error,component_management_overloaded} -> {failed,10,<<"component management capacity exceeded">>};
            {error,R} -> erlang:error(R)
        end
    catch _:Reason -> {failed,?V2_SPAWN_FAILED,
        unicode:characters_to_binary(io_lib:format("retire: ~P",[Reason,8],[{chars_limit,1024}]))} end.
retirement_reply(stop,_,_) -> stopped;
retirement_reply(drain,D,A) -> {drained_reply,D,A}.
admin_status(Prefix) ->
    case admin_match(Prefix) of
        [] -> {failed,?V2_COMPONENT_NOT_FOUND,<<"component not found">>};
        Comps -> {status_reply,component_states(Comps)}
    end.
component_states(Comps) ->
    [{P,case O of {owned,_,Es} -> parrot_component_runtime:status(maps:get(P,Es)); _ -> <<"running">> end,V}
     || {_,V,Paths,O} <- Comps,P <- Paths].

%% ============ 观测五件套：指标采集（admin-v2 MetricsReport） ============
%% 计数器存 ETS（跨连接累计——accept_loop 多连接生命周期）；快照时读出。

mbump(Key, N) when N >= 0 ->
    ets:insert_new(?METRICS_TAB, {Key, 0}),
    [{Key, Old}] = ets:lookup(?METRICS_TAB, Key),
    New = min(16#FFFFFFFFFFFFFFFF, Old + N),
    case ets:select_replace(?METRICS_TAB, [{{Key, Old}, [], [{{Key, New}}]}]) of
        1 -> New;
        0 -> mbump(Key, N)
    end.

mget(Key) ->
    case ets:lookup(?METRICS_TAB, Key) of
        [{_, V}] -> V;
        [] -> 0
    end.

%% 当前活跃连接数（连接进程注册表——admin_metrics 从任意进程可读）

%% Monitor-backed leases also retire connections killed outside the receive loop.
ensure_lease_owner() ->
    case whereis(parrot_gw_lease_owner) of
        undefined ->
            TableOwner = ets:info(?METRICS_TAB, owner),
            Starter = self(),
            {Pid, Ref} = spawn_monitor(fun() -> lease_owner_start(TableOwner, Starter) end),
            receive
                {lease_owner_ready, Pid} -> erlang:demonitor(Ref, [flush]), ok;
                {'DOWN', Ref, process, Pid, Reason} -> erlang:error({lease_owner_start, Reason})
            after 5000 ->
                erlang:demonitor(Ref, [flush]), exit(Pid, kill), erlang:error(lease_owner_start_timeout)
            end;
        _ -> ok
    end.

lease_owner_start(TableOwner, Starter) ->
    process_flag(trap_exit, true),
    Root = erlang:monitor(process, TableOwner),
    %% The winner activates independently of the transient start caller.
    case try register(parrot_gw_lease_owner, self()) catch error:badarg -> false end of
        true ->
            ets:insert(?METRICS_TAB, {connections, 0}),
            ets:delete_all_objects(?DIRECT_TAB),
            ets:insert(?METRICS_TAB, {connection_owner, self()}),
            Starter ! {lease_owner_ready, self()},
            lease_owner_loop(#{}, Root);
        false ->
            erlang:demonitor(Root, [flush]),
            Starter ! {lease_owner_ready, self()}
    end.

lease_call(Action) ->
    Owner = whereis(parrot_gw_lease_owner),
    Ref = erlang:monitor(process, Owner),
    Owner ! {lease, self(), Ref, Action},
    receive
        {Ref, ok} -> erlang:demonitor(Ref, [flush]), ok;
        {Ref, {error, Reason}} -> erlang:demonitor(Ref, [flush]), erlang:error(Reason);
        {'DOWN', Ref, process, _, Reason} -> erlang:error({lease_owner_down, Reason})
    after 5000 -> erlang:demonitor(Ref, [flush]), erlang:error(lease_owner_timeout)
    end.

lease_owner_loop(Leases, Root) ->
    receive
        {lease, Pid, Reply, own_network} ->
            link(Pid),
            Pid ! {Reply, ok},
            lease_owner_loop(Leases, Root);
        {lease, Caller, Reply, {reserve, Pid}} ->
            Live = retire_dead_leases(Leases),
            case map_size(Live) < ?MAX_CONNECTIONS andalso not maps:is_key(Pid, Live) of
                true ->
                    Ref = erlang:monitor(process, Pid),
                    link(Pid),
                    Caller ! {Reply, ok},
                    lease_owner_loop(Live#{Pid => {Ref, reserved}}, Root);
                false ->
                    Caller ! {Reply, {error, connection_capacity}},
                    lease_owner_loop(Live, Root)
            end;
        {lease, Pid, Reply, begin_lease} ->
            Live = retire_dead_leases(Leases),
            case maps:find(Pid, Live) of
                {ok, {Ref, reserved}} ->
                    Pid ! {Reply, ok},
                    lease_owner_loop(Live#{Pid => {Ref, handshake}}, Root);
                error when map_size(Live) < ?MAX_CONNECTIONS ->
                    Ref = erlang:monitor(process, Pid),
                    link(Pid),
                    Pid ! {Reply, ok},
                    lease_owner_loop(Live#{Pid => {Ref, handshake}}, Root);
                _ ->
                    mbump(hs_failed, 1),
                    Pid ! {Reply, {error, connection_capacity}},
                    lease_owner_loop(Live, Root)
            end;
        {lease, Pid, Reply, complete} ->
            {Result, Next} = case maps:find(Pid, Leases) of
                {ok, {Ref, handshake}} ->
                    mbump(hs_ok, 1), conn_incr(1), {ok, Leases#{Pid => {Ref, active}}};
                _ -> {{error, invalid_connection_lease}, Leases}
            end,
            Pid ! {Reply, Result},
            lease_owner_loop(Next, Root);
        {lease, Pid, Reply, retire} ->
            Next = retire_lease(Pid, retire_dead_leases(Leases)),
            Pid ! {Reply, ok},
            lease_owner_loop(Next, Root);
        %% A nonnormal exit closes linked sockets and their OTP task trees.
        {'DOWN', Root, process, _, Reason} -> exit({gateway_owner_down, Reason});
        {'EXIT', _, _} -> lease_owner_loop(Leases, Root);
        {'DOWN', Ref, process, Pid, _} ->
            Next = case maps:find(Pid, Leases) of
                {ok, {Ref, _}} -> retire_lease(Pid, Leases);
                _ -> Leases
            end,
            lease_owner_loop(Next, Root)
    end.

retire_dead_leases(Leases) ->
    lists:foldl(fun(Pid, Acc) ->
        case is_process_alive(Pid) of true -> Acc; false -> retire_lease(Pid, Acc) end
    end, Leases, maps:keys(Leases)).

retire_lease(Pid, Leases) ->
    case maps:take(Pid, Leases) of
        {{Ref, Phase}, Next} ->
            erlang:demonitor(Ref, [flush]),
            ets:match_delete(?DIRECT_TAB, {'_', '_', Pid}),
            case Phase of active -> conn_incr(-1); handshake -> mbump(hs_failed, 1); reserved -> ok end,
            Next;
        error -> Leases
    end.

conn_incr(D) -> ets:update_counter(?METRICS_TAB, connections, {2, D}, {connections, 0}).

active_connections() ->
    case ets:lookup(?METRICS_TAB, connection_owner) of
        [{connection_owner, Owner}] ->
            case whereis(parrot_gw_lease_owner) =:= Owner of
                true -> max(mget(connections), 0);
                false -> 0
            end;
        [] -> 0
    end.

metrics_count(Ft, Path, Key, Payload) ->
    mbump(bytes_rx, 32 + byte_size(Path) + byte_size(Key) + byte_size(Payload)),
    case Ft of
        ?ASK -> mbump(asks, 1);
        ?TELL -> mbump(tells, 1);
        ?HEARTBEAT -> mbump(hb, 1);
        _ -> ok
    end.

%% 采集全量快照（components 表 + 进程/内存 + 计数器）
admin_metrics() -> maps:get(snapshot, admin_measured_metrics()).

%% The identity belongs to the same ETS owner as the counters. Reopening a
%% listener or connection does not reset uptime or epoch; a new table does.
ensure_metrics_identity() ->
    case ets:member(?METRICS_TAB, identity) of
        true -> ok;
        false ->
            Epoch = binary:encode_hex(rand:bytes(16)),
            ets:insert_new(?METRICS_TAB, {identity, Epoch, os:system_time(millisecond)}),
            ok
    end.

admin_measured_metrics() ->
    Comps = case ets:info(?ADMIN_TAB) of
                undefined -> [];
                _ -> ets:tab2list(?ADMIN_TAB)
            end,
    States = component_states(Comps),
    Rss = read_rss("/proc/self/status"),
    {Mem, RssBit} = case Rss of {ok, N} -> {N, 1 bsl 15}; undefined -> {0, 0} end,
    [{identity, Epoch, Started}] = ets:lookup(?METRICS_TAB, identity),
    Snap = #{ts_ms => os:system_time(millisecond),
      runtime => iolist_to_binary(io_lib:format("erl/OTP-~s", [erlang:system_info(otp_release)])),
      connections => active_connections(),
      handshakes_ok => mget(hs_ok),
      handshakes_failed => mget(hs_failed),
      asks_rx => mget(asks),
      tells_rx => mget(tells),
      replies_tx => mget(replies),
      reply_errs => mget(r_errs),
      bytes_rx => mget(bytes_rx),
      bytes_tx => mget(bytes_tx),
      heartbeats_rx => mget(hb),
      components => length(Comps),
      component_states => States,
      processes => erlang:system_info(process_count),
      memory_rss => Mem,
      uptime_start_ms => Started},
    #{snapshot => Snap, available => (((1 bsl 17) - 1) bxor (1 bsl 15)) bor RssBit,
      epoch => Epoch, process_kind => <<"erlang_processes">>}.

%% Bounded current RSS, never VM heap or high-water RSS. Unknown stays unavailable.
read_rss(Path) ->
    case file:open(Path, [read, binary, raw]) of
        {ok, File} ->
            try
                case file:read(File, 65537) of
                    {ok, Bytes} when byte_size(Bytes) =< 65536 -> parse_rss(Bytes);
                    _ -> undefined
                end
            after file:close(File) end;
        {error, _} -> undefined
    end.

parse_rss(Bytes) ->
    Lines = [L || <<"VmRSS:", _/binary>> = L <- binary:split(Bytes, <<"\n">>, [global])],
    case Lines of
        [Line] ->
            case re:run(Line, <<"^VmRSS:[ \\t]+([0-9]+)[ \\t]+kB[ \\t]*$">>, [{capture, [1], binary}]) of
                {match, [Digits]} when byte_size(Digits) =< 20 ->
                    N = binary_to_integer(Digits) * 1024,
                    case N =< 16#FFFFFFFFFFFFFFFF of true -> {ok, N}; false -> undefined end;
                _ -> undefined
            end;
        _ -> undefined
    end.

%% A component root selects its own members; a member path selects only it.
%% A separate component whose name contains a dash is not a pool member.
admin_match(Prefix) ->
    lists:filtermap(fun({Name,V,Paths,O}) ->
        case parrot_component_runtime:select_paths(Prefix,Name,Paths) of
            [] -> false; Selected -> {true,{Name,V,Selected,O}}
        end
    end,ets:tab2list(?ADMIN_TAB)).

%% SYSTEM_EVENT payload 分发：admin-v2 命令 → 执行 → 回帧。
%% 返回 {ok, ReplyFrame} | ignore（非 admin-v2 tag）。
admin_handle_frame(Sock, Cid, Path, <<?TAG_ADMIN_CMD_V2:8, _/binary>> = Payload) ->
    Decoded = try {ok, decode_admin_cmd_v2(Payload)} catch _:_ -> error end,
    Reply = case Decoded of
        error -> {failed, Cid, ?V2_DIALECT_MISMATCH, <<"invalid admin payload">>};
        {ok, Cmd} when element(2, Cmd) =/= Cid ->
            {failed, Cid, ?V2_DIALECT_MISMATCH, <<"admin request ID disagrees with frame CID">>};
        {ok, Cmd} ->
            log("admin cmd decoded: ~p~n", [Cmd]),
            case Cmd of
                {deploy_component, ReqId, Deploy} ->
                    admin_reply(ReqId, admin_deploy(Deploy));
                {drain_component, ReqId, Prefix, TimeoutMs} ->
                    admin_reply(ReqId, admin_drain(Prefix, TimeoutMs));
                {stop_component, ReqId, Prefix} ->
                    admin_reply(ReqId, admin_stop(Prefix));
                {component_status, ReqId, Prefix} ->
                    admin_reply(ReqId, admin_status(Prefix));
                {metrics_report, ReqId} ->
                    admin_reply(ReqId, {metrics, admin_metrics()});
                {measured_metrics_report, ReqId} ->
                    {measured_metrics, ReqId, admin_measured_metrics()}
            end
        end,
    wire_send(Sock, build_frame(?FT_SYSTEM_EVENT, Cid, Path, <<>>,
                                   encode_admin_reply_v2(Reply))),
    ok;
admin_handle_frame(_Sock, _Cid, _Path, _) ->
    ignore.

%% 方言内部形态 → AdminReplyV2 tuple
admin_reply(ReqId, {deployed, Paths}) -> {deployed, ReqId, Paths};
admin_reply(ReqId, {drained_reply, D, A}) -> {drained, ReqId, D, A};
admin_reply(ReqId, stopped) -> {stopped, ReqId};
admin_reply(ReqId, {status_reply, States}) -> {status, ReqId, States};
admin_reply(ReqId, {failed, Code, Detail}) -> {failed, ReqId, Code, Detail};
admin_reply(ReqId, {metrics, Snap}) -> {metrics, ReqId, Snap}.

%% ============ Erlang actor 服务（方言可辨识） ============
%% 生产 service/3 按 ADMIN_TAB 的实例路径选中唯一业务模块。
%% service/2 保留无地址的本地辅助调用；只有该显式入口按消息键遍历。
%% 内置 bin:u:Ping/Add 为无状态连通性探针，不是业务组件。
%% 网关不再内置 crawler 业务 handler（业务代码已迁 apps/*/erlang/）。

service(Key, Payload) ->
    case component_service(Key, Payload) of
        {ok, Reply}    -> Reply;
        {error, miss}  -> builtin_service(Key, Payload)
    end.

%% Addressed wire traffic selects exactly one registered path. The legacy
%% service/2 helper remains an explicitly unaddressed local probe dispatcher.
service(Path, Key, Payload) -> service(Path,Key,Payload,reply_budget()).
service(Path,Key,Payload,ReplyBytes) when is_integer(ReplyBytes), ReplyBytes>=0, ReplyBytes=<16777216 ->
    Local = local_actor_path(Path),
    Comps = try ets:tab2list(?ADMIN_TAB) catch error:badarg -> [] end,
    Matches = [M || {_, _, Paths, M} <- Comps, lists:member(Local, Paths)],
    case Matches of
        [Owned] -> dispatch_component(Owned,Local,Key,Payload,ReplyBytes);
        [] ->
            %% These two fixed, stateless gateway probes predate component
            %% deployment. A missing application path never scans other modules.
            case Key of
                <<"bin:u:Ping">> -> parrot_trace:handler(Local,parrot_trace:current(),parrot_trace:operation(),
                                      fun() -> builtin_service(Key, Payload) end);
                <<"bin:u:Add">> -> parrot_trace:handler(Local,parrot_trace:current(),parrot_trace:operation(),
                                     fun() -> builtin_service(Key, Payload) end);
                _ -> erlang:error({actor_not_found, Local})
            end;
        _ -> erlang:error({ambiguous_actor_path, Local})
    end.

local_actor_path(<<"parrot://", Rest/binary>>) ->
    case binary:split(Rest, <<"/">>) of
        [Node, Suffix] ->
            case Node =:= to_bin(gateway_node_id()) of
                true -> <<"/", Suffix/binary>>;
                false -> erlang:error({route_unreachable, Node})
            end;
        _ -> erlang:error({route_unreachable, Rest})
    end;
local_actor_path(Path) -> Path.

%% 显式无地址本地辅助调用（生产网络默认不走此入口）。
component_service(Key, Payload) ->
    Comps = try ets:tab2list(?ADMIN_TAB) catch error:badarg -> [] end,
    component_service_loop(Comps, Key, Payload).

component_service_loop([], _Key, _Payload) -> {error, miss};
component_service_loop([{_,_,Paths,Owned}|T],Key,Payload) ->
    try dispatch_component(Owned,hd(Paths),Key,Payload) of Reply -> {ok,Reply}
    catch error:{unknown_service,Key} -> component_service_loop(T,Key,Payload) end.
reply_budget() ->
    case get(reply_bytes) of N when is_integer(N),N>=0 -> N; _ -> 16777216 end.
dispatch_component(Owned,Path,Key,Payload) -> dispatch_component(Owned,Path,Key,Payload,reply_budget()).
dispatch_component({owned,_,Entries},Path,Key,Payload,ReplyBytes) ->
    parrot_component_runtime:invoke(maps:get(Path,Entries),Key,Payload,ReplyBytes);
dispatch_component(Module,_Path,Key,Payload,_ReplyBytes) ->
    case erlang:function_exported(Module,parrot_service,2) of
        true -> Module:parrot_service(Key,Payload);
        false -> erlang:error({unknown_service,Key})
    end.
builtin_service(<<"bin:u:Ping">>, <<N:64/little>>) ->
    {<<"bin:u:Pong">>, <<(N + 3):64/little>>};   %% erlang 方言 +3
builtin_service(<<"bin:u:Add">>, <<A:64/little, B:64/little>>) ->
    {<<"bin:u:AddR">>, <<(A + B + 10000):64/little>>};  %% erlang 方言 +10000
builtin_service(Key, _Payload) ->
    erlang:error({unknown_service, Key}).

%% ============ 错误体（[u16 code][u16 rsv][detail utf8]） ============

err_payload(Code, Detail) ->
    wire_require(is_integer(Code) andalso Code >= 1 andalso Code =< 13, bad_error_code),
    D = to_bin(Detail),
    wire_utf8(D),
    <<Code:16/little, 0:16/little, D/binary>>.

%% ============ main（网关进程入口） ============

gateway_node_id() ->
    case os:getenv("PARROT_NODE_ID") of false -> "erl-gw-1"; Value -> Value end.

main(Args) ->
    try
        {Port, Bind, Target} = gateway_arguments(Args),
        {ok, RealPort} = start(Port, Bind, fun ?MODULE:service/3),
        io:format("PARROT_ERL_PORT=~p~n", [RealPort]),
        case Target of undefined -> ok; _ -> register_parrot(Target) end,
        receive stop_gateway -> halt(0) end
    catch C:R ->
        io:format(standard_error, "gateway startup failed: ~p:~p~n", [C,R]),
        halt(2)
    end.

gateway_arguments([]) -> {0, {0,0,0,0}, undefined};
gateway_arguments([PortText | Options]) ->
    Port = parse_port(PortText),
    Parsed = lists:foldl(fun(Option, Acc) ->
        {Key, Value} = case Option of
            "listen=" ++ Address ->
                {ok, IP} = inet:parse_strict_address(Address),
                {listen, IP};
            "parrot=" ++ Endpoint ->
                _ = parse_endpoint(Endpoint),
                {parrot, Endpoint};
            _ -> erlang:error(invalid_gateway_option)
        end,
        false = maps:is_key(Key, Acc),
        Acc#{Key => Value}
    end, #{}, Options),
    {Port, maps:get(listen, Parsed, {0,0,0,0}), maps:get(parrot, Parsed, undefined)}.

parse_port(P) when is_list(P), length(P) >= 1, length(P) =< 5 ->
    endpoint_require(lists:all(fun(C) -> C >= $0 andalso C =< $9 end, P)),
    parse_port(list_to_integer(P));
parse_port(P) when is_integer(P), P >= 0, P =< 65535 -> P;
parse_port(_) -> erlang:error(invalid_port).

endpoint_require(true) -> ok;
endpoint_require(false) -> erlang:error(invalid_endpoint).

%% IPv6 uses brackets so the port is unambiguous. No whitespace, zone IDs or
%% numeric shorthand is accepted. DNS names retain IPv4 and gain IPv6 lookup.
parse_endpoint(Address) when is_list(Address), length(Address) =< 261 ->
    {Host, PortText} = case Address of
        [$[ | Rest] ->
            [IPText, PortPart] = string:split(Rest, "]:", all),
            {ok, IP} = inet:parse_strict_address(IPText),
            endpoint_require(tuple_size(IP) =:= 8),
            {IP, PortPart};
        _ ->
            [Name, PortPart] = string:split(Address, ":", all),
            endpoint_require(Name =/= []),
            case inet:parse_strict_address(Name) of
                {ok, IP} -> {IP, PortPart};
                {error, _} ->
                    endpoint_require(case inet:parse_address(Name) of
                        {error, _} -> valid_dns_name(Name);
                        _ -> false
                    end),
                    {Name, PortPart}
            end
    end,
    Port = parse_port(PortText),
    endpoint_require(Port > 0),
    {Host, Port};
parse_endpoint(_) -> erlang:error(invalid_endpoint).

valid_dns_name(Name) when length(Name) =< 253 ->
    %% Reject noncanonical dotted numeric addresses rather than resolving them
    %% through the OS's legacy numeric shorthand rules.
    NotNumeric = not lists:all(fun(C) -> (C >= $0 andalso C =< $9) orelse C =:= $. end, Name),
    WithoutRoot = case Name of
        [] -> [];
        _ -> case lists:last(Name) of $. -> lists:droplast(Name); _ -> Name end
    end,
    Labels = string:split(WithoutRoot, ".", all),
    NotNumeric andalso lists:all(fun(Label) ->
        length(Label) >= 1 andalso length(Label) =< 63 andalso
        hd(Label) =/= $- andalso lists:last(Label) =/= $- andalso
        lists:all(fun(C) -> (C >= $a andalso C =< $z) orelse
            (C >= $A andalso C =< $Z) orelse (C >= $0 andalso C =< $9) orelse C =:= $- end, Label)
    end, Labels);
valid_dns_name(_) -> false.

socket_family(IP) when tuple_size(IP) =:= 8 -> inet6;
socket_family(IP) when tuple_size(IP) =:= 4 -> inet.

connect_endpoint(Host, Port) ->
    Deadline = now_ms() + 5000,
    case is_tuple(Host) of
        true -> connect_address(Host, Port, 5000);
        false ->
            %% Bound each lookup and all candidate attempts by one dial budget.
            V4 = resolve_addresses(Host, inet, Deadline),
            V6 = resolve_addresses(Host, inet6, Deadline),
            connect_candidates(lists:sublist(interleave(V4, V6), 16), Port, Deadline, nxdomain)
    end.
resolve_addresses(Host, Family, Deadline) ->
    case Deadline - now_ms() of
        Remaining when Remaining > 0 ->
            case inet:getaddrs(Host, Family, min(1000, Remaining)) of
                {ok, Addresses} -> lists:sublist(Addresses, 16);
                {error, _} -> []
            end;
        _ -> []
    end.
interleave([], B) -> B;
interleave([A | Rest], B) -> [A | interleave(B, Rest)].
connect_candidates([], _, _, LastError) -> {error, LastError};
connect_candidates([IP | Rest], Port, Deadline, _) ->
    case Deadline - now_ms() of
        Remaining when Remaining > 0 ->
            case connect_address(IP, Port, min(1000, Remaining)) of
                {ok, Sock} -> {ok, Sock};
                {error, Reason} -> connect_candidates(Rest, Port, Deadline, Reason)
            end;
        _ -> {error, timeout}
    end.
connect_address(IP, Port, Timeout) ->
    gen_tcp:connect(IP, Port, [socket_family(IP), binary, {packet, raw}, {active, false},
        {nodelay, true}, {send_timeout, 5000}, {send_timeout_close, true}], Timeout).

%% 注册模式：registrar 自己持有 socket，完成握手、收帧和关闭后才重试。
%% 仅有效 ACK 后通知首次注册成功；失败重试保留首次等待者。
%%     · 收 HEARTBEAT 秒回 ACK（hub 探活本网关）
%%     · 10s 无任何入帧 → 判 hub 半开死亡 → 自杀 → registrar 重拨
%%   直连学习（方案 A）：收 ROUTE_HINT(node, addr) → spawn 直拨该网关
%%     建第二条链路（后续对它的帧直发，不经 hub 中转）
register_parrot(Target) ->
    {Host, Port} = parse_endpoint(Target),
    Self = self(),
    Pid = spawn(fun() ->
        ok = lease_call(own_network),
        registrar(Host, Port, Self, 0)
    end),
    receive
        registered           -> ok;
        {register_failed, R} -> halt(1), exit({register_failed, R})
    after 15000 -> halt(2)
    end,
    {ok, Pid}.

%% 监督者：无限重连（指数退避 1s→60s；连接建立重置）。首连失败也重试
%% （原先一次失败即 halt——网络抖动场景下网关成了孤儿）。
registrar(Host, Port, Parent, Attempt) ->
    Delay = case Attempt of 0 -> 0; N -> min(60000, 1000 * (1 bsl min(N - 1, 6))) end,
    case Delay of 0 -> ok; D -> timer:sleep(D) end,
    case connect_endpoint(Host, Port) of
        {ok, Sock} ->
            with_connection(Sock, fun() ->
                put(service_fun, fun ?MODULE:service/3),
                Tail = client_handshake(Sock, any),
                case Parent of P when is_pid(P) -> P ! registered; _ -> ok end,
                log("parrot handshake ok - serving~n", []),
                conn_loop(Sock, Tail)
            end),
            case get(wire_phase) of
                active -> registrar(Host, Port, none, 1);
                _ -> registrar(Host, Port, Parent, Attempt + 1)
            end;
        {error, Why} ->
            log("connect parrot failed: ~p~n", [Why]),
            registrar(Host, Port, Parent, Attempt + 1)
    end.

%% 握手采用 10s 总期限；活跃连接 10s 无完整帧则关闭，零碎字节不续期。
conn_loop(Sock, Buf) -> loop(Sock, Buf).

recv_frame(Sock, Buf) ->
    case parse_received_frame(Buf) of
        {ok, Ft, _Flags, Cid, Path, Key, Payload, Tail} ->
            {ok, Ft, Cid, Path, Key, Payload, Tail};
        {more, Rest} ->
            Remaining = get(wire_deadline) - now_ms(),
            wire_require(Remaining > 0, handshake_timeout),
            case gen_tcp:recv(Sock, 0, Remaining) of
                {ok, D} -> recv_frame(Sock, <<Rest/binary, D/binary>>);
                {error, E} -> erlang:error({handshake_recv, E})
            end
    end.

%% DEV_08 测试口：启动网关并直接返回端口（不打印——测试免 stdout 捕获）。
%% ServiceFun 可注入：生产默认 service/3 保留路径；测试兼容二参数 service。
start(Port) -> start(Port, fun ?MODULE:service/3).
start(Port, ServiceFun) -> start(Port, {0,0,0,0}, ServiceFun).
start(Port, Bind, ServiceFun) when is_integer(Port), Port >= 0, Port =< 65535,
    is_tuple(Bind), (tuple_size(Bind) =:= 4 orelse tuple_size(Bind) =:= 8),
    (is_function(ServiceFun, 2) orelse is_function(ServiceFun, 3)) ->
    Max = case tuple_size(Bind) of 4 -> 255; 8 -> 65535 end,
    case lists:all(fun(N) -> is_integer(N) andalso N >= 0 andalso N =< Max end, tuple_to_list(Bind)) of
        true -> start_bound(Port, Bind, ServiceFun);
        false -> {error, badarg}
    end;
start(_, _, _) -> {error, badarg}.

start_bound(Port, Bind, ServiceFun) ->
    Family = socket_family(Bind),
    FamilyOptions = case Family of inet6 -> [inet6, {ipv6_v6only, true}]; inet -> [inet] end,
    %% A rejected bind must not acquire the shared gateway's lifetime.
    case gen_tcp:listen(Port, FamilyOptions ++ [{ip, Bind}, binary, {packet, raw},
            {active, false}, {nodelay, true}, {reuseaddr, true},
            {send_timeout, 5000}, {send_timeout_close, true}]) of
        {error, Reason} -> {error, Reason};
        {ok, LSock} ->
            try
                initialize_gateway(),
                start_listener(LSock, ServiceFun)
            catch Class:Reason:Stack ->
                gen_tcp:close(LSock),
                erlang:raise(Class, Reason, Stack)
            end
    end.

initialize_gateway() ->
    global:trans({parrot_gw_initialization, self()},
        fun initialize_gateway_locked/0, [node()]).

initialize_gateway_locked() ->
    try ets:info(?DIRECT_TAB) =:= undefined andalso
        ets:new(?DIRECT_TAB, [named_table, public, {read_concurrency, true}])
    catch _:_ -> ok end,   %% 网关直连表（幂等——业务表已迁 app 组件）
    try admin_init() catch _:_ -> ok end,   %% B4：admin-v2 登记表（同幂等语义）
    try ets:info(?METRICS_TAB) =:= undefined andalso
        ets:new(?METRICS_TAB, [named_table, public, set,
                               {write_concurrency, true}])
    catch _:_ -> ok end,   %% 观测五件套：跨连接累计计数器（loop 进程字典改为 ETS——连接断不清零）
    ensure_metrics_identity(),
    ensure_lease_owner().

start_listener(LSock, ServiceFun) ->
    {ok, RealPort} = inet:port(LSock),
    Parent = self(),
    Listener = spawn(fun() ->
        Monitor = erlang:monitor(process, Parent),
        receive
            {accept, LSock} ->
                erlang:demonitor(Monitor, [flush]),
                ok = lease_call(own_network),
                Parent ! {listener_ready, self()},
                accept_loop(LSock, ServiceFun);
            {'DOWN', Monitor, process, Parent, _} -> ok
        end
    end),
    Ready = erlang:monitor(process, Listener),
    case gen_tcp:controlling_process(LSock, Listener) of
        ok ->
            Listener ! {accept, LSock},
            receive
                {listener_ready, Listener} ->
                    erlang:demonitor(Ready, [flush]), {ok, RealPort};
                {'DOWN', Ready, process, Listener, Reason} ->
                    gen_tcp:close(LSock), {error, {listener_start, Reason}}
            after 5000 ->
                erlang:demonitor(Ready, [flush]),
                gen_tcp:close(LSock), exit(Listener, kill), {error, listener_start_timeout}
            end;
        {error, Reason} ->
            erlang:demonitor(Ready, [flush]),
            gen_tcp:close(LSock), exit(Listener, kill), {error, Reason}
    end.

%% 多连接生命周期：每连接独立进程服务（Erlang 原生形态——轻量进程隔离；
%% 连接断只影响自身；listener 持续 accept）
accept_loop(LSock, ServiceFun) ->
    case gen_tcp:accept(LSock) of
        {ok, Sock} ->
            Acceptor = self(),
            Pid = spawn(fun() ->
                Parent = erlang:monitor(process, Acceptor),
                receive
                    {serve, Sock} ->
                        erlang:demonitor(Parent, [flush]),
                        put(service_fun, ServiceFun),
                        with_connection(Sock, fun() -> loop(Sock, <<>>) end);
                    {'DOWN', Parent, process, Acceptor, _} -> ok
                end
            end),
            %% Reserve synchronously in the accept loop. A connection flood
            %% cannot create unlimited driver processes waiting for admission.
            try
                ok = lease_call({reserve, Pid}),
                case gen_tcp:controlling_process(Sock, Pid) of
                    ok -> Pid ! {serve, Sock};
                    {error, _} -> gen_tcp:close(Sock), exit(Pid, kill)
                end
            catch _:_ -> gen_tcp:close(Sock), exit(Pid, kill)
            end,
            accept_loop(LSock, ServiceFun);
        {error, closed} -> ok;
        {error, R} -> log("accept ~p~n", [R]), accept_loop(LSock, ServiceFun)
    end.

log(Fmt, Args) ->
    %% 网关日志走 stderr（stdout 契约只留端口行）
    io:format(standard_error, Fmt, Args).

%% loop（被动模式）：Frame path 字段透传 handle（admin 回帧的 reply_to）
loop(Sock, Buf0) ->
    case parse_received_frame(Buf0) of
        {ok, Ft, Flags, Cid, Path, Key, Payload, Tail} ->
            put(last_inbound, now_ms()),
            handle(Sock, Ft, Flags, Cid, Path, Key, Payload),
            loop(Sock, Tail);
        {more, Rest} ->
            Deadline = case get(wire_phase) of
                active -> get(last_inbound) + 10000;
                _ -> get(wire_deadline)
            end,
            Remaining = Deadline - now_ms(),
            wire_require(Remaining > 0, wire_timeout),
            case gen_tcp:recv(Sock, 0, Remaining) of
                {ok, Data} -> loop(Sock, <<Rest/binary, Data/binary>>);
                {error, closed} -> ok;
                {error, Reason} -> erlang:error({recv_failed, Reason})
            end
    end.

handle(Sock, Ft, Flags, Cid, Path, Key, Payload) ->
    case {get(wire_phase), Ft} of
        {handshake, ?HANDSHAKE} ->
            Peer = decode_handshake(Payload, false),
            AckBody = iolist_to_binary(handshake_ack_body(gateway_node_id(), maps:get(codec, Peer))),
            ok = wire_send(Sock, build_frame(?HANDSHAKE_ACK, Cid, <<>>, <<"__handshake__">>, AckBody)),
            put(peer_limit, min(?GATEWAY_FRAME_LEN, maps:get(limit, Peer))),
            put(peer_trace, maps:get(capabilities,Peer) band ?CAPS_TRACE_CONTEXT =/= 0),
            complete_connection();
        {active, ?HANDSHAKE} -> erlang:error(repeated_handshake);
        {active, ?HANDSHAKE_ACK} -> erlang:error(unexpected_ack);
        {active, _} -> handle_active(Sock, Ft, Flags, Cid, Path, Key, Payload);
        _ -> erlang:error(handshake_required)
    end.

handle_active(Sock, Ft, Flags, Cid, Path, Key, Payload0) ->
    {Parent,Payload}=case Flags band ?TRACE_CONTEXT of
        0 -> {undefined,Payload0};
        _ -> parrot_trace:split(Payload0)
    end,
    case Ft of
        ?ERROR -> erlang:error(remote_terminal_error);
        ?HEARTBEAT ->
            wire_require(Flags =:= 0 andalso Payload =:= <<>>, invalid_heartbeat),
            wire_send(Sock, build_frame(?HEARTBEAT_ACK, Cid, <<"">>, <<"">>, <<>>));
        ?HEARTBEAT_ACK ->
            wire_require(Flags =:= 0 andalso Payload =:= <<>>, invalid_heartbeat);
        ?FT_SYSTEM_EVENT ->
            %% B4（DEV_09）：admin-v2 命令（0x03）——执行+回帧；其余 tag 忽略
            try admin_handle_frame(Sock, Cid, Path, Payload)
            catch Class:R:Stk -> log("admin frame error ~p:~p~n  at ~p~n", [Class, R, Stk])
            end;
        ?ROUTE_HINT ->
            %% Advisory hints become bounded, deduplicated supervisor branches.
            %% Live links and pending dials both consume the parent's capacity.
            try
                {Node, Addr} = parse_route_hint(Payload),
                case ets:lookup(?DIRECT_TAB, Node) of
                    []   -> admit_direct(Node, Addr);
                    [{Node, _, Driver} = Entry] ->
                        case is_process_alive(Driver) of
                            true -> ok;
                            false ->
                                ets:delete_object(?DIRECT_TAB, Entry),
                                admit_direct(Node, Addr)
                        end
                end
            catch _:_ -> ok
            end;
        ?ASK ->
            %% Reject malformed metadata before admission or business execution.
            case split_reply_to(Payload) of
                {ok, Real} -> dispatch_business(Sock, ask, Cid, Path, Key, Real,Parent);
                {error, Reason} -> send_business_error(Sock, Cid, 12, Reason)
            end;
        ?TELL ->
            Mode = case Flags band ?TELL_ACK of 0 -> tell; _ -> completion end,
            dispatch_business(Sock, Mode, Cid, Path, Key, Payload,Parent);
        _ -> ok
    end.

dispatch_business(Sock, Mode, Cid, Path, Key, Payload,Parent) ->
    Svc = case get(service_fun) of
        F when is_function(F, 2); is_function(F, 3) ->
            case F =:= fun ?MODULE:service/3 of true -> default; false -> {custom,F} end;
        _ -> default
    end,
    Operation=case Mode of ask -> ask; _ -> tell end,
    PeerLimit = get(peer_limit),
    Work = fun() ->
        put(peer_limit, PeerLimit),
        %% REPLY has no path: 28 bytes of fixed body fields precede key/payload.
        %% TELL does not transmit the component's business reply.
        Budget=case {Mode,PeerLimit} of {ask,L} when is_integer(L) -> max(0,min(?MAX_FRAME_LEN,L)-28); _ -> 16777216 end,
        put(reply_bytes,Budget),
        Result = try
            Value0 = case Svc of
                {custom,F0} -> parrot_trace:handler(Path,Parent,Operation,fun() ->
                    case is_function(F0,3) of
                        true -> F0(Path,Key,Payload);
                        false -> F0(Key,Payload)
                    end
                end);
                default -> parrot_trace:scope(Parent,Operation,fun() -> ?MODULE:service(Path,Key,Payload) end)
            end,
            case Value0 of
                {error, Reason} -> {error, 12, Reason};
                Value ->
                    case business_result(Mode,Value) of
                        {ok,{RK,RB}}=Good when Mode=:=ask ->
                            case byte_size(to_bin(RK))+byte_size(RB)=<Budget of
                                true -> Good; false -> {error,10,reply_capacity_exceeded}
                            end;
                        Other -> Other
                    end
            end
        catch
            error:component_overloaded -> {error, 10, component_capacity_exceeded};
            error:component_stopped -> {error, 3, component_stopped};
            error:{actor_not_found, Missing} -> {error, 1, {actor_not_found, Missing}};
            error:{route_unreachable, Node} -> {error, 7, {route_unreachable, Node}};
            error:{unknown_service, Key} -> {error, 6, {unknown_service, Key}};
            Class:Reason0 -> {error, 12, {Class, Reason0}}
        end,
        case {Mode, Result} of
            {tell, _} -> ok; %% Plain TELL never acquires completion semantics.
            {completion, {ok, _}} ->
                wire_send(Sock, build_frame(?REPLY, Cid, <<>>, ?TELL_COMPLETION_KEY, <<>>));
            {ask, {ok, {ReplyKey, ReplyPayload}}} ->
                wire_send(Sock, build_frame(?REPLY, Cid, <<>>, ReplyKey, ReplyPayload));
            {_, {error, Code, Detail}} -> send_business_error(Sock, Cid, Code, Detail)
        end
    end,
    case Mode=:=completion andalso is_integer(PeerLimit) andalso PeerLimit<28+byte_size(?TELL_COMPLETION_KEY) of
        true -> send_business_error(Sock,Cid,10,completion_receipt_capacity_exceeded);
        false ->
            case admit_work(Work) of
                {ok, _} -> ok;
                {error, overloaded} when Mode =:= tell -> erlang:error(tell_work_capacity);
                {error, overloaded} ->
                    send_business_error(Sock, Cid, 10, connection_work_capacity_exceeded)
            end
    end.

business_result(ask, {Key, Payload}) when (is_binary(Key) orelse is_list(Key)), is_binary(Payload) ->
    %% Validate output before a bad result can strand its caller without a reply.
    wire_utf8(to_bin(Key)),
    {ok, {Key, Payload}};
business_result(ask, _) -> {error, 12, invalid_handler_reply};
business_result(_, Value) -> {ok, Value}.

send_business_error(Sock, Cid, Code, Reason) ->
    %% Bound remote diagnostics independently of arbitrary handler terms.
    Detail = unicode:characters_to_binary(io_lib:format("~P", [Reason, 8], [{chars_limit, 1024}])),
    %% A valid small negotiated frame must still carry a minimal error receipt.
    %% Drop the entire optional diagnostic rather than split a UTF-8 codepoint.
    Bounded=case get(peer_limit) of L when is_integer(L),byte_size(Detail)+32>L -> <<>>; _ -> Detail end,
    wire_send(Sock, build_frame(?REPLY_ERR, Cid, <<>>, <<>>, err_payload(Code, Bounded))).

split_reply_to(<<Len:32/little, ReplyTo:Len/binary, Rest/binary>>) ->
    case unicode:characters_to_binary(ReplyTo, utf8, utf8) of
        B when is_binary(B) -> {ok, Rest};
        _ -> {error, invalid_reply_to_utf8}
    end;
split_reply_to(_) -> {error, truncated_reply_to}.

%% ============ 方案 A：直连学习 ============

%% ROUTE_HINT payload: [u16 node_len][node][u16 addr_len][addr]
parse_route_hint(<<NLen:16/little, Node:NLen/binary,
                   ALen:16/little, Addr:ALen/binary>>) ->
    wire_utf8(Node),
    wire_utf8(Addr),
    wire_require(NLen > 0 andalso ALen > 0, empty_route_hint),
    Address = unicode:characters_to_list(Addr),
    _ = parse_endpoint(Address),
    {Node, Address};
parse_route_hint(_) -> erlang:error(bad_hint).

%% 直拨目标网关：完整客户端握手（对端 accept——所有网关 accept 侧都活着）
%% → ets 登记 → conn_loop 服务（同 hub 链路语义）。断连：ets 移除（下次
%% hint 或 hub 中转兜底重新学习）。退避 1s→30s 重试。
direct_dial(Node, Addr) ->
    {Host, Port} = parse_endpoint(Addr),
    direct_dial_try(Node, Host, Port, 0).

direct_dial_try(Node, Host, Port, Attempt) ->
    Delay = min(30000, 1000 * (1 bsl min(max(0, Attempt - 1), 5))),
    case Attempt of 0 -> ok; _ -> timer:sleep(Delay) end,
    case connect_endpoint(Host, Port) of
        {ok, Sock} ->
            try with_connection(Sock, fun() ->
                Tail = client_handshake(Sock, Node),
                ets:insert(?DIRECT_TAB, {Node, Sock, self()}),
                conn_loop(Sock, Tail)
            end)
            after
                case ets:lookup(?DIRECT_TAB, Node) of
                    [{Node, Sock, Owner}] when Owner =:= self() ->
                        ets:delete_object(?DIRECT_TAB, {Node, Sock, Owner});
                    _ -> ok
                end
            end,
            case get(wire_phase) of
                active -> ok;
                _ -> direct_dial_try(Node, Host, Port, Attempt + 1)
            end;
        {error, _} -> direct_dial_try(Node, Host, Port, Attempt + 1)
    end.
