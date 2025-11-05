%% Actor isolation and lifetime via real admin/business frames, one VM per app.
-module(test_frontier_instances).
-export([run/1,run_both/0]).
run(Kind) ->
    Tests=[{"state survives deployment connection retirement",fun(S,Port) ->
        deploy(S,Kind,<<"state">>,singleton),push(S,Kind,<<"/user/state">>,7),
        gen_tcp:close(S),wait_closed(500),S2=connect(Port),
        try [7]=next(S2,Kind,<<"/user/state">>) after gen_tcp:close(S2) end end},
      {"pool members own independent queues and dedup",fun(S,_) ->
        deploy(S,Kind,<<"pool">>,{pool,2}),
        push(S,Kind,<<"/user/pool-0">>,7),[]=next(S,Kind,<<"/user/pool-1">>),
        push(S,Kind,<<"/user/pool-1">>,7),[7]=next(S,Kind,<<"/user/pool-0">>),
        [7]=next(S,Kind,<<"/user/pool-1">>) end},
      {"different deployments own independent queues",fun(S,_) ->
        deploy(S,Kind,<<"a">>,singleton),deploy(S,Kind,<<"b">>,singleton),
        push(S,Kind,<<"/user/a">>,7),[]=next(S,Kind,<<"/user/b">>),
        push(S,Kind,<<"/user/b">>,7),[7]=next(S,Kind,<<"/user/a">>),[7]=next(S,Kind,<<"/user/b">>) end},
      {"stopping one pool member retains its sibling",fun(S,_) ->
        deploy(S,Kind,<<"pool">>,{pool,2}),push(S,Kind,<<"/user/pool-1">>,8),
        {stopped,_}=admin(S,fun(I) -> {stop_component,I,<<"/user/pool-0">>} end),
        {status,_,[{<<"/user/pool-1">>,<<"running">>,<<"1">>}]}=admin(S,fun(I) -> {component_status,I,<<"/user/pool-1">>} end),
        [8]=next(S,Kind,<<"/user/pool-1">>) end},
      {"stop releases dedup and same-name redeploy starts empty",fun(S,_) ->
        deploy(S,Kind,<<"fresh">>,singleton),push(S,Kind,<<"/user/fresh">>,7),[7]=next(S,Kind,<<"/user/fresh">>),
        {stopped,_}=admin(S,fun(I) -> {stop_component,I,<<"/user/fresh">>} end),
        deploy(S,Kind,<<"fresh">>,singleton),push(S,Kind,<<"/user/fresh">>,7),[7]=next(S,Kind,<<"/user/fresh">>) end}],
    Failed=lists:foldl(fun({Name,F},N) ->
        try fixture(Kind,F),io:format("FRONTIER_INSTANCE_OK ~p ~s~n",[Kind,Name]),N
        catch C:R -> io:format("FRONTIER_INSTANCE_FAIL ~p ~s: ~p:~p~n",[Kind,Name,C,R]),N+1 end
    end,0,Tests),
    case Failed of 0 -> ok; _ -> erlang:error({frontier_instance_failures,Failed}) end.
fixture(Kind,F) ->
    App=case Kind of crawler -> "crawler-lab"; websearch -> "websearch" end,
    Dir=filename:join("/tmp","parrot-instance-"++os:getpid()++"-"++integer_to_list(erlang:unique_integer([positive]))),
    ok=file:make_dir(Dir),put(artifact_dir,Dir),
    {ok,frontier}=compile:file("../../apps/"++App++"/erlang/frontier.erl",[{outdir,Dir},report_errors]),
    {ok,Port}=parrot_gw:start(0),S=connect(Port),
    try F(S,Port)
    after
        [parrot_gw:admin_stop(<<"/user/",N/binary>>) || N <- [<<"state">>,<<"pool">>,<<"a">>,<<"b">>,<<"fresh">>]],
        gen_tcp:close(S),wait_closed(500),code:purge(frontier),code:delete(frontier),code:del_path(Dir),
        file:delete(filename:join(Dir,"frontier.beam")),file:del_dir(Dir)
    end.
connect(Port) ->
    {ok,S}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}]),
    H=iolist_to_binary(parrot_gw:handshake_body("instance-contract")),
    ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,H)),{2,1,_,_}=recv(S),S.
recv(S) ->
    {ok,<<N:32/little>>}=gen_tcp:recv(S,4,6000),{ok,B}=gen_tcp:recv(S,N,6000),
    {ok,Ft,_,Cid,_,K,P,<<>>}=parrot_gw:parse_frame(<<N:32/little,B/binary>>),{Ft,Cid,K,P}.
admin(S,F) ->
    I=erlang:unique_integer([positive]),B=parrot_gw:encode_admin_cmd_v2(F(I)),
    ok=gen_tcp:send(S,parrot_gw:build_frame(16#20,I,<<"/admin">>,<<>>,B)),
    {16#20,I,_,R}=recv(S),parrot_gw:decode_admin_reply_v2(R).
deploy(S,_Kind,Name,Policy) ->
    Uri=list_to_binary("file://"++get(artifact_dir)),
    {deployed,_,_}=admin(S,fun(I) -> {deploy_component,I,{Name,<<"1">>,{beam,<<"frontier">>,Uri},Policy,undefined}} end).
key(crawler,push) -> <<"bin:crawl/FrontierPush">>;
key(crawler,next) -> <<"bin:crawl/FrontierNext">>;
key(websearch,push) -> <<"bin:ws/Push">>;
key(websearch,next) -> <<"bin:ws/Next">>.
ask(S,Path,K,Payload) ->
    I=erlang:unique_integer([positive]),ok=gen_tcp:send(S,parrot_gw:build_frame(16#10,I,Path,K,<<0:32/little,Payload/binary>>)),
    case recv(S) of {16#11,I,_,B} -> B; Other -> erlang:error({business_reply,Other}) end.
push(S,Kind,P,Id) ->
    U=integer_to_binary(Id),Common= <<(byte_size(U)):32/little,U/binary,0:16/little>>,
    Entry=case Kind of crawler -> <<Id:64/little,Common/binary>>; websearch -> Common end,
    <<1:32/little>>=ask(S,P,key(Kind,push),<<1:32/little,Entry/binary>>).
next(S,Kind,P) -> decode(Kind,ask(S,P,key(Kind,next),<<100:32/little>>)).
decode(Kind,<<N:32/little,B/binary>>) -> decode(Kind,N,B,[]).
decode(_,0,<<>>,A) -> lists:reverse(A);
decode(crawler,N,<<Id:64/little,L:32/little,_U:L/binary,_D:16/little,R/binary>>,A) -> decode(crawler,N-1,R,[Id|A]);
decode(websearch,N,<<L:32/little,U:L/binary,_D:16/little,R/binary>>,A) -> decode(websearch,N-1,R,[binary_to_integer(U)|A]).
wait_closed(0) -> 0=maps:get(connections,parrot_gw:admin_metrics());
wait_closed(N) -> case maps:get(connections,parrot_gw:admin_metrics()) of 0 -> ok; _ -> timer:sleep(2),wait_closed(N-1) end.

%% The two real applications intentionally use the same BEAM module name.
run_both() ->
    Base="/tmp/parrot-frontier-coexist-"++os:getpid(),ok=file:make_dir(Base),
    A=filename:join(Base,"crawler"),B=filename:join(Base,"web"),
    ok=file:make_dir(A),ok=file:make_dir(B),
    {ok,frontier}=compile:file("../../apps/crawler-lab/erlang/frontier.erl",[{outdir,A}]),
    {ok,frontier}=compile:file("../../apps/websearch/erlang/frontier.erl",[{outdir,B}]),
    {ok,Port}=parrot_gw:start(0),S=connect(Port),
    try
        put(artifact_dir,A),deploy(S,crawler,<<"crawler">>,singleton),
        put(artifact_dir,B),deploy(S,websearch,<<"web">>,singleton),
        push(S,crawler,<<"/user/crawler">>,11),push(S,websearch,<<"/user/web">>,22),
        [11]=next(S,crawler,<<"/user/crawler">>),[22]=next(S,websearch,<<"/user/web">>),
        ok=file:del_dir_r(A),ok=file:del_dir_r(B),
        push(S,crawler,<<"/user/crawler">>,33),push(S,websearch,<<"/user/web">>,44),
        [33]=next(S,crawler,<<"/user/crawler">>),
        {stopped,_}=admin(S,fun(I)->{stop_component,I,<<"/user/crawler">>} end),
        [44]=next(S,websearch,<<"/user/web">>),
        io:format("FRONTIER_COEXIST_OK two actual frontier modules, real Wire, source removal and independent Stop~n"),ok
    after
        [parrot_gw:admin_stop(P) || P<-[<<"/user/crawler">>,<<"/user/web">>]],
        gen_tcp:close(S),file:del_dir_r(Base)
    end.
