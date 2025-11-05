%% Real Wire requests to two deployed modules with overlapping message keys.
-module(test_component_routes).
-export([run/0]).
run() ->
    Tests=[{"same key reaches the addressed component",fun(S) ->
        {ok,111}=ask(S,<<"/user/a">>,<<"shared">>),
        {ok,222}=ask(S,<<"/user/b">>,<<"shared">>) end},
      {"unknown path never executes another component",fun(S) ->
        [{error,1}=ask(S,P,<<"shared">>) || P <- [<<"/user/missing">>,<<"/user/a-child">>,<<"/user/a/child">>,<<>>]],
        ok=assert_counts(S,0,0) end},
      {"qualified address validates gateway identity",fun(S) ->
        {ok,222}=ask(S,<<"parrot://route-gw/user/b">>,<<"shared">>),
        {error,7}=ask(S,<<"parrot://other/user/a">>,<<"shared">>),
        {error,7}=ask(S,<<"parrot://route-gwx/user/a">>,<<"shared">>),
        ok=assert_counts(S,0,1) end},
      {"selected component misses and errors cannot fall through",fun(S) ->
        {error,6}=ask(S,<<"/user/a">>,<<"b_only">>),
        {error,12}=ask(S,<<"/user/a">>,<<"failure">>),
        ok=assert_counts(S,0,0) end},
      {"acknowledged TELL routes exactly and fails for absent actor",fun(S) ->
        completion(S,<<"/user/b">>,ok),
        completion(S,<<"/user/absent">>,{error,1}),
        ok=assert_counts(S,0,1) end},
      {"plain TELL reaches only its addressed component",fun(S) ->
        ok=gen_tcp:send(S,parrot_gw:build_frame(16#13,900,<<"/user/a">>,<<"shared">>,<<>>)),
        wait_until(fun() -> ask(S,<<"/user/a">>,<<"calls">>)=:={ok,1} end,500),
        ok=assert_counts(S,1,0),
        {error,timeout}=gen_tcp:recv(S,0,20) end},
      {"pool member paths are exact, not component prefixes",fun(S) ->
        stopped=parrot_gw:admin_stop(<<"/user/a">>),
        {deployed,[<<"/user/a-0">>,<<"/user/a-1">>]}=parrot_gw:admin_deploy(
            {<<"a">>,<<"1">>,{beam,<<"test_route_a">>,list_to_binary("file://"++get(route_artifact_dir))},{pool,2},undefined}),
        {ok,111}=ask(S,<<"/user/a-0">>,<<"shared">>),
        {ok,111}=ask(S,<<"/user/a-1">>,<<"shared">>),
        {error,1}=ask(S,<<"/user/a">>,<<"shared">>),
        {error,1}=ask(S,<<"/user/a-2">>,<<"shared">>) end},
      {"stopped path cannot reach a surviving component",fun(S) ->
        stopped=parrot_gw:admin_stop(<<"/user/a">>),
        {error,1}=ask(S,<<"/user/a">>,<<"shared">>),
        {ok,222}=ask(S,<<"/user/b">>,<<"shared">>) end}],
    Failed=lists:foldl(fun({Name,F},Count) ->
        try fixture(F),io:format("COMPONENT_ROUTE_OK ~s~n",[Name]),Count
        catch C:R:Stack -> io:format("COMPONENT_ROUTE_FAIL ~s ~p:~p ~p~n",[Name,C,R,Stack]),Count+1 end
    end,0,Tests),
    case Failed of 0 -> ok; _ -> erlang:error({route_cases_failed,Failed}) end.

fixture(F) ->
    Old=os:getenv("PARROT_NODE_ID"),os:putenv("PARROT_NODE_ID","route-gw"),
    Dir=filename:join("/tmp","parrot-route-contract-"++os:getpid()++"-"++integer_to_list(erlang:unique_integer([positive]))),
    ok=file:make_dir(Dir),put(route_artifact_dir,Dir),
    parrot_gw:admin_init(),
    try
        [begin
            Special=case Module of test_route_a -> "parrot_service(<<\"failure\">>,_,_) -> erlang:error(route_fixture_failure);\n";
                                   test_route_b -> "parrot_service(<<\"b_only\">>,_,N) -> reply(N);\nparrot_service(<<\"failure\">>,_,N) -> reply(N);\n" end,
            Src=io_lib:format("-module(~p).~n-export([parrot_init/1,parrot_service/3]).~n"
                "parrot_init(_)->{ok,0}.~n~s"
                "parrot_service(<<\"calls\">>,_,N)->{reply,{<<\"result\">>,<<N:64/little>>},N};~n"
                "parrot_service(<<\"shared\">>,_,N) -> reply(N);~n"
                "parrot_service(K,_,_) -> erlang:error({unknown_service,K}).~n"
                "reply(N) -> {reply,{<<\"result\">>,<<~p:64/little>>},N+1}.~n",
                [Module,Special,Magic]),
            File=filename:join(Dir,atom_to_list(Module)++".erl"),ok=file:write_file(File,Src),
            {ok,Module}=compile:file(File,[{outdir,Dir},report_errors]),
            {deployed,_}=parrot_gw:admin_deploy({Name,<<"1">>,{beam,atom_to_binary(Module,utf8),list_to_binary("file://"++Dir)},singleton,undefined})
        end || {Module,Magic,Name} <- [{test_route_a,111,<<"a">>},{test_route_b,222,<<"b">>}]],
        {ok,Port}=parrot_gw:start(0),
        {ok,S}=gen_tcp:connect("127.0.0.1",Port,[binary,{active,false}]),
        try
            H=iolist_to_binary(parrot_gw:handshake_body("route-test-client")),
            ok=gen_tcp:send(S,parrot_gw:build_frame(1,1,<<>>,<<>>,H)),{2,1,_,_}=recv(S),F(S)
        after gen_tcp:close(S) end
    after
        [parrot_gw:admin_stop(P) || P <- [<<"/user/a">>,<<"/user/b">>]],
        [begin code:purge(M),code:delete(M),
            file:delete(filename:join(Dir,atom_to_list(M)++".erl")),
            file:delete(filename:join(Dir,atom_to_list(M)++".beam")) end || M <- [test_route_a,test_route_b]],
        code:del_path(Dir),file:del_dir(Dir),erase(route_artifact_dir),
        case Old of false -> os:unsetenv("PARROT_NODE_ID"); _ -> os:putenv("PARROT_NODE_ID",Old) end
    end.
ask(S,Path,Key) ->
    Cid=erlang:unique_integer([positive]),
    ok=gen_tcp:send(S,parrot_gw:build_frame(16#10,Cid,Path,Key,<<0:32/little>>)),
    case recv(S) of
        {16#11,Cid,<<"result">>,<<N:64/little>>} -> {ok,N};
        {16#12,Cid,_,<<Code:16/little,0:16,_/binary>>} -> {error,Code}
    end.
completion(S,Path,Expected) ->
    Cid=erlang:unique_integer([positive]),
    ok=gen_tcp:send(S,parrot_gw:build_frame(16#13,16#10,Cid,Path,<<"shared">>,<<>>)),
    Actual=case recv(S) of
        {16#11,Cid,<<"parrot:completion#v1">>,<<>>} -> ok;
        {16#12,Cid,_,<<Code:16/little,0:16,_/binary>>} -> {error,Code}
    end,
    Expected=Actual.
recv(S) ->
    {ok,<<N:32/little>>}=gen_tcp:recv(S,4,3000),{ok,B}=gen_tcp:recv(S,N,3000),
    {ok,Ft,_,Cid,_,K,P,<<>>}=parrot_gw:parse_frame(<<N:32/little,B/binary>>),{Ft,Cid,K,P}.

wait_until(F,0) -> true=F();
wait_until(F,N) -> case F() of true -> ok; false -> timer:sleep(2),wait_until(F,N-1) end.

assert_counts(S,A,B) -> {ok,A}=ask(S,<<"/user/a">>,<<"calls">>),{ok,B}=ask(S,<<"/user/b">>,<<"calls">>),ok.
