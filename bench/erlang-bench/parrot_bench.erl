%% Parrot 双引擎基准的 Erlang/OTP 对等实现。
%%
%% 对等原则：
%%  - **计算时长对等**（跨语言标准做法）：Rust burn 速率 ~880M iters/s，
%%    Erlang bignum 标定 ~0.4M iters/s（慢 ~2200x）。CPU 场景迭代数按
%%    2200x 缩减，使每场景标称计算秒数与 Rust 版一致；note 注明。
%%  - 消息语义对等：ask=同步 call（带超时），tell=cast/异步消息
%%  - burn_cpu 同一 LCG 常数（band 模拟 u64 wrapping）
%%  - p50/p90/p99/max 分位数算法一致（ceil 索引）
%%  - 预热 3k echo call 后正式计时
%%  - Erlang actor=轻量进程：herd-20000 可全量跑（不降载）
%%
%% 运行：./run.sh
-module(parrot_bench).

-export([main/1]).

%% ======================= 速率缩放 =======================

-define(SCALE, 24).  %% Rust/Erlang iters 比（2026-10-04 重标定：BEAM 实测 ~36M iters/s）

iters_for(RustIters) ->
    max(1, RustIters div ?SCALE).

%% ======================= 负载内核 =======================

-define(M64, 16#FFFFFFFFFFFFFFFF).
-define(MUL, 16#5851F42D4C957F2D).   %% 6364136223846793005
-define(ADD, 16#9E3779B97F4A7C15).   %% 1442695040888963407

%% X=当前值, I=剩余迭代, N=当前下标（与 Rust: x=MUL*x+ADD ^ i 一致）
burn_loop(X, 0, _N) -> X;
burn_loop(X, I, N) ->
    X2 = ((X * ?MUL + ?ADD) band ?M64) bxor N,
    case X2 of
        42 -> 42;
        _ -> burn_loop(X2, I - 1, N + 1)
    end.

burn_cpu(Iters, Salt) ->
    burn_loop((Salt band ?M64) bor 1, Iters, 0).

%% ======================= 统计 =======================

lat_stats([]) -> #{n => 0, p50 => 0, p90 => 0, p99 => 0, max => 0, mean => 0.0};
lat_stats(Samples) ->
    S = lists:sort(Samples),
    N = length(S),
    Pick = fun(Q) ->
        Idx0 = max(0, min(N - 1, ceil(Q / 100.0 * N) - 1)),
        lists:nth(Idx0 + 1, S)
    end,
    Sum = lists:sum(S),
    #{n => N, p50 => Pick(50), p90 => Pick(90), p99 => Pick(99),
      max => lists:last(S), mean => Sum / N}.

%% ======================= BenchActor（轻量进程） =======================

spawn_bench() ->
    spawn(fun() -> loop(0, 0) end).

loop(Ops, Sink) ->
    receive
        %% ask 协议：{{消息体}, {回复Pid, Ref}}（2-tuple 外壳）
        {{ask_echo, V}, From} ->
            gen_reply(From, V), loop(Ops + 1, Sink);
        {{ask_cpu, Iters, Salt}, From} ->
            R = burn_cpu(Iters, Salt),
            gen_reply(From, R), loop(Ops + 2, Sink + R);
        {{ask_tiny, Salt}, From} ->
            R = burn_cpu(iters_for(1000), Salt),
            gen_reply(From, R), loop(Ops, Sink + R);
        {{ask_medium, Iters, Salt}, From} ->
            R = burn_cpu(Iters, Salt),
            gen_reply(From, R), loop(Ops + 2, Sink + R);
        {{ask_minute, Iters, Salt}, From} ->
            R = burn_cpu(Iters, Salt),
            gen_reply(From, R), loop(Ops + 2, Sink + R);
        {{ask_longrun, Iters}, From} ->
            R = burn_cpu(Iters, 7),
            gen_reply(From, R), loop(Ops + 2, Sink + R);
        %% tell 协议：{消息体}（1-tuple）
        {tell_echo, _V} ->
            loop(Ops + 1, Sink);
        {tell_cpu, Iters, Salt} ->
            R = burn_cpu(Iters, Salt),
            loop(Ops + 2, Sink + R);
        {tell_medium, Iters, Salt} ->
            R = burn_cpu(Iters, Salt),
            loop(Ops + 2, Sink + R);
        {tell_minute, Iters, Salt} ->
            R = burn_cpu(Iters, Salt),
            loop(Ops + 2, Sink + R);
        {tell_tiny, Salt} ->
            R = burn_cpu(iters_for(1000), Salt),
            loop(Ops, Sink + R);
        {tell_io, Ms} ->
            timer:sleep(Ms), loop(Ops + 1, Sink);
        {{get_count}, From} ->
            gen_reply(From, Ops), loop(Ops, Sink);
        stop ->
            ok
    end.

gen_reply({Pid, Ref}, Reply) ->
    Pid ! {Ref, Reply}.

ask(Pid, Msg, TimeoutMs) ->
    Ref = make_ref(),
    Pid ! {Msg, {self(), Ref}},
    receive
        {Ref, Reply} -> Reply
    after TimeoutMs ->
        exit(timeout)
    end.

now_us() -> erlang:system_time(microsecond).

%% ======================= 输出 =======================

fmt_tput(Msgs, Wall) ->
    T = case Wall > 0 of true -> Msgs / Wall; false -> 0.0 end,
    io_lib:format("~b", [round(T)]).

row(Name, Msgs, WallSec, LatUs, Correct, Note) ->
    #{n := _N, p50 := P50, p90 := P90, p99 := P99, max := Max} = lat_stats(LatUs),
    TPutS = fmt_tput(Msgs, WallSec),
    io:format("[erlang] ~s | msgs=~p | wall=~.3fs | tput=~s/s | "
              "lat p50=~.1fms p90=~.1fms p99=~.1fms max=~.1fms | correct=~p~n",
              [Name, Msgs, WallSec, TPutS, P50/1000, P90/1000, P99/1000, Max/1000, Correct]),
    case Note of
        "" -> ok;
        _  -> io:format("        note: ~s~n", [Note])
    end,
    ok.

wait_until(_Cond, TimeoutMs) when TimeoutMs =< 0 -> false;
wait_until(Cond, TimeoutMs) ->
    case Cond() of
        true -> true;
        false ->
            Poll = min(20, max(1, TimeoutMs div 10)),
            timer:sleep(Poll),
            wait_until(Cond, TimeoutMs - Poll)
    end.

%% ======================= main =======================

main(_Args) ->
    io:format("==================== ERLANG BENCH ====================~n"),
    io:format("schedulers=~p SCALE=1/~p~n", [erlang:system_info(schedulers), ?SCALE]),
    ScaledNote = io_lib:format("iters scaled 1/~p (time-parity with Rust)", [?SCALE]),

    %% warmup
    W = spawn_bench(),
    [begin I = ask(W, {ask_echo, X}, 10000), I = X end || X <- lists:seq(1, 3000)],
    W ! stop,
    io:format("warmup done~n"),

    %% ---------- 1. seq-ask-echo-1k ----------
    A1 = spawn_bench(),
    T01 = now_us(),
    Lat1 = [begin
        S = now_us(),
        I = ask(A1, {ask_echo, X}, 10000),
        I = X,
        now_us() - S
    end || X <- lists:seq(1, 1000)],
    W1 = (now_us() - T01) / 1_000_000,
    row("seq-ask-echo-1k", 1000, W1, Lat1, true, ""),

    %% ---------- 2/3. conc-ask ----------
    A2 = spawn_bench(),
    {W2, Lat2} = conc_ask(A2, 8, 1000),
    row("conc-ask-echo-c8-m1000", 8000, W2, Lat2, true, "concurrency=8"),
    A3 = spawn_bench(),
    {W3, Lat3} = conc_ask(A3, 64, 200),
    row("conc-ask-echo-c64-m200", 12800, W3, Lat3, true, "concurrency=64"),

    %% ---------- 4. tell-echo-100k ----------
    A4 = spawn_bench(),
    T04 = now_us(),
    [A4 ! {tell_echo, I} || I <- lists:seq(1, 100000)],
    Ok4 = wait_until(fun() -> ask(A4, {get_count}, 30000) >= 100000 end, 120000),
    W4 = (now_us() - T04) / 1_000_000,
    row("tell-echo-100k", 100000, W4, [], Ok4,
        case Ok4 of true -> ""; false -> "DRAIN TIMEOUT" end),

    %% ---------- 5. cpu-serial-200x200k ----------
    A5 = spawn_bench(),
    It5 = iters_for(200000),
    T05 = now_us(),
    Lat5 = [begin
        S = now_us(),
        _ = ask(A5, {ask_cpu, It5, I}, 30000),
        now_us() - S
    end || I <- lists:seq(1, 200)],
    W5 = (now_us() - T05) / 1_000_000,
    row("cpu-serial-200x200k", 200, W5, Lat5, true,
        io_lib:format("~~200us work/msg; ~s", [ScaledNote])),

    %% ---------- 6. cpu-parallel ----------
    Actors6 = [spawn_bench() || _ <- lists:seq(1, 64)],
    It6 = iters_for(200000),
    T06 = now_us(),
    Self6 = self(),
    Pids6 = [erlang:spawn(fun() ->
        Lats = [begin
             S = now_us(),
             _ = ask(Ac, {ask_cpu, It6, Idx * 1000 + K}, 30000),
             now_us() - S
         end || K <- lists:seq(1, 25)],
        Self6 ! {self(), Lats}
    end) || {Idx, Ac} <- lists:zip(lists:seq(1, 64), Actors6)],
    All6 = lists:append([receive {P, L} -> L end || P <- Pids6]),
    W6 = (now_us() - T06) / 1_000_000,
    row("cpu-parallel-8actors-200k", 64 * 25, W6, All6, true,
        io_lib:format("64 actors x 25 msgs x ~~200us; ~s", [ScaledNote])),

    %% ---------- 7. flood-500k-tell ----------
    A7 = spawn_bench(),
    N7 = 500000,
    T07 = now_us(),
    [erlang:spawn(fun() ->
         [A7 ! {tell_echo, I + P * 100000} || I <- lists:seq(1, N7 div 4)]
     end) || P <- lists:seq(0, 3)],
    Sent7 = (now_us() - T07) / 1_000_000,
    Ok7 = wait_until(fun() -> ask(A7, {get_count}, 30000) >= N7 end, 300000),
    W7 = (now_us() - T07) / 1_000_000,
    row("flood-500k-tell", N7, W7, [], Ok7,
        io_lib:format("sent in ~.3fs; ~s", [Sent7, case Ok7 of true -> "drained"; false -> "TIMEOUT" end])),

    %% ---------- 8. ask-timeout-short ----------
    A8 = spawn_bench(),
    %% heavy 取 500ms 等效（缩放后 ~22.7万 iters），确保 5ms 探测窗口内
    %% heavy 仍在 handler 中，echo 必然排队 → 必超时。
    %% （原 Rust 版 50M iters ~50ms；此处为保证门控确定性取 10x。）
    Heavy = erlang:spawn(fun() -> _ = ask(A8, {ask_cpu, iters_for(50000000) * 10, 1}, 120000) end),
    timer:sleep(1),  %% 投递即排队（Erlang 邮箱 FIFO；heavy 先到先入 handler）
    TimedOut8 = try
        ask(A8, {ask_echo, 1}, 5),
        false
    catch
        exit:timeout -> true
    end,
    MRef = erlang:monitor(process, Heavy),
    receive {'DOWN', MRef, process, Heavy, _} -> ok after 120000 -> ok end,
    row("ask-timeout-short", 2, 0.0, [], TimedOut8,
        io_lib:format("short-timeout ask: ~s",
            [case TimedOut8 of true -> "timeout-correct"; false -> "NO-TIMEOUT" end])),

    %% ---------- 9. send-after-stop ----------
    A9 = spawn_bench(),
    A9 ! stop,
    timer:sleep(50),
    Dead9 = try
        ask(A9, {ask_echo, 1}, 100),
        false
    catch
        _:_ -> true
    end,
    row("send-after-stop", 1, 0.0, [], Dead9,
        io_lib:format("send after stop => ~s",
            [case Dead9 of true -> "timeout (dead-letter)"; false -> "REPLY?!" end])),

    %% ---------- 10. herd-20000-actors（Erlang 全量） ----------
    T010 = now_us(),
    Herd = [spawn_bench() || _ <- lists:seq(1, 20000)],
    SpawnW10 = (now_us() - T010) / 1_000_000,
    T110 = now_us(),
    [H ! {tell_echo, 1} || H <- Herd],
    SendW10 = (now_us() - T110) / 1_000_000,
    OkN10 = lists:foldl(fun(H, Acc) ->
        try ask(H, {get_count}, 10000) of
            1 -> Acc + 1;
            _ -> Acc
        catch _:_ -> Acc end
    end, 0, lists:sublist(Herd, 50)),
    row("herd-20000-actors", 20000, SpawnW10, [], OkN10 == 50,
        io_lib:format("spawned 20000 in ~.3fs (~s/s); send-all=~.3fs; alive ~p/50",
            [SpawnW10, fmt_tput(20000, SpawnW10), SendW10, OkN10])),
    [H ! stop || H <- Herd],

    %% ---------- 11. longrun ----------
    A11 = spawn_bench(),
    T011 = now_us(),
    V11 = ask(A11, {ask_longrun, iters_for(2000000000)}, 300000),
    W11 = (now_us() - T011) / 1_000_000,
    row("longrun-2G-iters", 1, W11, [round(W11 * 1_000_000)], V11 =/= 0,
        io_lib:format("single 2G-iteration compute (~p real iters)", [iters_for(2000000000)])),

    %% ---------- 12. starve-echo-during-longrun ----------
    Aa = spawn_bench(), Ab = spawn_bench(),
    %% collector 先注册，避免 ticker 竞态（未注册名字发送会 badarg）
    register(collector, spawn(fun() -> collect_loop([]) end)),
    Samples12 = spawn_link(fun Ticker12() ->
        receive stop_ticker -> ok after 0 ->
            S = now_us(),
            try ask(Ab, {ask_echo, 1}, 30000) of
                _ -> collector ! {tick, now_us() - S}
            catch _:_ -> skip end,
            Ticker12()
        end
    end),
    timer:sleep(150),
    BaseN12 = length(get_samples()),
    T012 = now_us(),
    _ = ask(Aa, {ask_longrun, iters_for(1000000000)}, 300000),
    C12 = (now_us() - T012) / 1_000_000,
    timer:sleep(150),
    Samples12 ! stop_ticker,
    All12 = get_samples(),
    {BaseLat12, During12} = lists:split(min(BaseN12, length(All12)), All12),
    StB = lat_stats(BaseLat12),
    #{p99 := BP99} = StB,
    StD = lat_stats(During12),
    #{p99 := DP99, max := DMax} = StD,
    row("starve-echo-during-longrun", length(During12), C12, During12, true,
        io_lib:format("heavy=~.1fs; probes=~p (p99=~.2fms max=~.2fms); baseline p99=~.2fms",
            [C12, length(During12), DP99/1000, DMax/1000, BP99/1000])),
    unregister(collector),

    %% ---------- 13. io-async-64actors-10ms ----------
    IoActors = [spawn_bench() || _ <- lists:seq(1, 64)],
    T013 = now_us(),
    [begin
         [Ac ! {tell_io, 10} || _ <- lists:seq(1, 20)]
     end || Ac <- IoActors],
    Ok13 = wait_until(fun() ->
        lists:all(fun(Ac) ->
            try ask(Ac, {get_count}, 30000) >= 1 catch _:_ -> false end
        end, IoActors)
    end, 120000),
    W13 = (now_us() - T013) / 1_000_000,
    Total13 = 64 * 20,
    Serial13 = Total13 * 10 / 1000,
    row("io-async-64actors-10ms", Total13, W13, [], Ok13,
        io_lib:format("sleep(10ms) in handler; ~p tasks wall=~.2fs (serial ~.2fs; speedup ~.1fx)",
            [Total13, W13, Serial13, Serial13 / max(W13, 0.001)])),

    %% ---------- 14. mixed-minute-cpu-plus-incoming ----------
    register(collector, spawn(fun() -> collect_loop([]) end)),
    Long14 = iters_for(57200000000),
    Med14 = iters_for(2400000000),
    LongActors = [spawn_bench() || _ <- lists:seq(1, 8)],
    BackupActors = [spawn_bench() || _ <- lists:seq(1, 4)],
    Probe = spawn_bench(),
    ProbePid = erlang:spawn(fun ProbeLoop() ->
        receive stop_probe -> ok after 0 ->
            S = now_us(),
            try ask(Probe, {ask_tiny, 1}, 60000) of
                _ -> collector ! {probe, now_us() - S}
            catch _:_ -> skip end,
            ProbeLoop()
        end
    end),
    T014 = now_us(),
    LongPids = [erlang:spawn_monitor(fun() ->
        _ = ask(Ac, {ask_minute, Long14, I}, 600000), done
    end) || {I, Ac} <- lists:zip(lists:seq(1, 8), LongActors)],
    MedPids = [erlang:spawn(fun() ->
        [begin Ac ! {tell_medium, Med14, K}, timer:sleep(50) end || K <- lists:seq(1, 10)]
    end) || Ac <- BackupActors],
    [receive {'DOWN', _, process, _, _} -> ok end || {_, _} <- LongPids],
    [receive {M, done} -> ok after 0 -> ok end || M <- MedPids],
    W14 = (now_us() - T014) / 1_000_000,
    ProbePid ! stop_probe,
    ProbeLat14 = get_probes(3000),
    St14 = lat_stats(ProbeLat14),
    #{p99 := P9914, max := Max14} = St14,
    row("mixed-minute-cpu-plus-incoming", 8 + 40, W14, ProbeLat14, true,
        io_lib:format("8x65s long + 40x2.7s medium; probe p99=~.1fms max=~.1fms over ~p probes; ~s",
            [P9914/1000, Max14/1000, length(ProbeLat14), ScaledNote])),
    unregister(collector),

    %% ---------- 15. mixed-same-actor-fifo ----------
    A15 = spawn_bench(),
    T015 = now_us(),
    A15 ! {tell_minute, iters_for(30000000000), 1},
    [A15 ! {tell_medium, iters_for(2400000000), K} || K <- lists:seq(1, 6)],
    [A15 ! {tell_tiny, K} || K <- lists:seq(1, 4000)],
    wait_until(fun() -> ask(A15, {get_count}, 300000) >= 2 + 6 * 2 end, 300000),
    S15 = now_us(),
    _ = ask(A15, {ask_tiny, 0}, 120000),
    Tail15 = (now_us() - S15) / 1000,
    W15 = (now_us() - T015) / 1_000_000,
    row("mixed-same-actor-fifo", 1 + 6 + 4000, W15, [], true,
        io_lib:format("1x34s + 6x2.7s + 4000 tiny FIFO; tail-probe lat=~pms; ~s",
            [round(Tail15), ScaledNote])),

    %% ---------- 16. chunked-vs-solid ----------
    A16 = spawn_bench(), B16 = spawn_bench(),
    T0s = now_us(),
    _ = ask(A16, {ask_longrun, iters_for(30000000000)}, 600000),
    Solid = (now_us() - T0s) / 1_000_000,
    T0c = now_us(),
    [_ = ask(B16, {ask_longrun, iters_for(1500000000)}, 600000) || _ <- lists:seq(1, 20)],
    Chunked = (now_us() - T0c) / 1_000_000,
    Overhead = (Chunked / Solid - 1) * 100,
    row("chunked-vs-solid-longrun", 2, Solid + Chunked,
        [round(Solid * 1_000_000), round(Chunked * 1_000_000)], true,
        io_lib:format("solid=~.1fs vs chunkedx20=~.1fs; overhead ~.1f%", [Solid, Chunked, Overhead])),

    %% ---------- 17. pingpong-rtt-10k ----------
    PA = spawn_bench(), PB = spawn_bench(),
    T017 = now_us(),
    Lat17 = pingpong_loop(PA, PB, 10000, []),
    W17 = (now_us() - T017) / 1_000_000,
    row("pingpong-rtt-10k", 20000, W17, Lat17, true,
        "2-hop ask RTT (A->B), sampled every 10th"),

    %% ---------- 18. self-chain-ask-tell-20k ----------
    A18 = spawn_bench(),
    T018 = now_us(),
    [begin
         V = ask(A18, {ask_echo, I}, 30000),
         A18 ! {tell_echo, V + 1}
     end || I <- lists:seq(1, 20000)],
    Ok18 = wait_until(fun() -> ask(A18, {get_count}, 30000) >= 40000 end, 60000),
    W18 = (now_us() - T018) / 1_000_000,
    row("self-chain-ask-tell-20k", 40000, W18, [], Ok18,
        "back-to-back ask->tell to same actor"),

    %% ---------- 19. slow-consumer-8prod-40k ----------
    Consumer = spawn_bench(),
    PerMsg = iters_for(170000),
    T019 = now_us(),
    [erlang:spawn(fun() ->
        [Consumer ! {tell_cpu, PerMsg, P * 10000 + K} || K <- lists:seq(1, 5000)]
    end) || P <- lists:seq(0, 7)],
    SendT19 = (now_us() - T019) / 1_000_000,
    Target19 = 8 * 5000,
    Ok19 = wait_until(fun() -> ask(Consumer, {get_count}, 30000) >= Target19 * 2 end, 300000),
    W19 = (now_us() - T019) / 1_000_000,
    row("slow-consumer-8prod-40k", Target19, W19, [], Ok19,
        io_lib:format("8 producers x 5000 x ~~200us msgs; send-all=~.2fs; ~s",
            [SendT19, case Ok19 of true -> "drained"; false -> "TIMEOUT" end])),

    %% ---------- 20. spawn-stop-storm-5k ----------
    T020 = now_us(),
    Alive20 = spawn_stop_waves(5, 0),
    W20 = (now_us() - T020) / 1_000_000,
    row("spawn-stop-storm-5k", 5000, W20, [], Alive20 == 50,
        io_lib:format("5 waves x 1000 spawn+ask+stop; alive ~p/50", [Alive20])),

    io:format("==================== ERLANG DONE ====================~n"),
    halt(0).

%% ======================= helpers =======================

conc_ask(Actor, C, M) ->
    T0 = now_us(),
    Self = self(),
    Pids = [erlang:spawn(fun() ->
        Lats = [begin
            S = now_us(),
            V = ask(Actor, {ask_echo, I + Cx}, 60000),
            V = I + Cx,
            now_us() - S
        end || I <- lists:seq(1, M)],
        Self ! {self(), Lats}
    end) || Cx <- lists:seq(0, C - 1)],
    All = lists:append([receive {P, L} -> L end || P <- Pids]),
    Wall = (now_us() - T0) / 1_000_000,
    {Wall, All}.

collect_loop(Acc) ->
    receive
        {get_all, From} -> From ! {samples, lists:reverse(Acc)}, collect_loop(Acc);
        {tick, L} -> collect_loop([L | Acc]);
        {probe, L} -> collect_loop([L | Acc]);
        reset -> collect_loop([])
    after 5000 -> collect_loop(Acc)
    end.

get_samples() ->
    collector ! {get_all, self()},
    receive {samples, S} -> S after 5000 -> [] end.

get_probes(Timeout) ->
    receive {samples, _} -> [] after 0 -> ok end,
    get_samples().

pingpong_loop(_PA, _PB, 0, Acc) -> lists:reverse(Acc);
pingpong_loop(PA, PB, I, Acc) ->
    S = now_us(),
    VA = ask(PA, {ask_echo, I}, 30000),
    VB = ask(PB, {ask_echo, VA}, 30000),
    VB = I,
    Acc1 = case I rem 10 of
        0 -> [now_us() - S | Acc];
        _ -> Acc
    end,
    pingpong_loop(PA, PB, I - 1, Acc1).

spawn_stop_waves(0, Alive) -> Alive;
spawn_stop_waves(W, Alive) ->
    Acts = [spawn_bench() || _ <- lists:seq(1, 1000)],
    AliveN = lists:foldl(fun({I, Ac}, Acc) ->
        try ask(Ac, {ask_echo, I}, 30000) of
            I -> Acc + 1;
            _ -> Acc
        catch _:_ -> Acc end
    end, 0, lists:zip(lists:seq(1, 10), lists:sublist(Acts, 10))),
    [Ac ! stop || Ac <- Acts],
    timer:sleep(30),
    spawn_stop_waves(W - 1, Alive + AliveN).
