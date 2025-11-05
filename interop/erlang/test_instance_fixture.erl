%% Controlled stateful component: all externally visible effects are test-owned.
-module(test_instance_fixture).
-export([parrot_init/1,parrot_service/3]).
parrot_init(Context) ->
    Tab=ets:new(instance_state,[private,set]),ets:insert(Tab,{count,0}),
    Path=maps:get(path,Context),
    whereis(instance_contract_observer)!{created,Path,self(),Tab},
    case {maps:get(config,Context),binary:longest_common_suffix([Path,<<"-1">>])} of
        {<<"fail-second">>,2} -> erlang:error(instance_initialization_failed);
        {<<"block-init">>,_} -> process_flag(trap_exit,true),receive finish_init -> {ok,{Context,Tab}} end;
        _ -> {ok,{Context,Tab}}
    end.
parrot_service(<<"block">>,_,State) ->
    whereis(instance_contract_observer)!{entered,self()},
    receive resume -> parrot_service(<<"inc">>,<<>>,State) end;
parrot_service(<<"inc">>,_,{_,Tab}=State) ->
    N=ets:update_counter(Tab,count,1),{reply,{<<"count">>,<<N:64/little>>},State};
parrot_service(<<"count">>,_,{_,Tab}=State) ->
    [{count,N}]=ets:lookup(Tab,count),{reply,{<<"count">>,<<N:64/little>>},State};
parrot_service(<<"trap">>,_,State) ->
    process_flag(trap_exit,true),{reply,{<<"trap">>,<<>>},State};
parrot_service(<<"die">>,_,_) -> exit(self(),kill);
parrot_service(Key,_,_) -> erlang:error({unknown_service,Key}).
