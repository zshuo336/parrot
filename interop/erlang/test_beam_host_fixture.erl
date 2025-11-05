-module(test_beam_host_fixture).
-export([parrot_init/1,parrot_service/3]).
-define(MAGIC, 1).
parrot_init(C) ->
    <<N:32/big,Control:N/binary,Mode/binary>>=maps:get(config,C),
    Prefix=filename:join(binary_to_list(Control),binary_to_list(binary:encode_hex(erlang:md5(maps:get(path,C))))),
    ok=file:write_file(Prefix++".created",os:getpid()),
    case {Mode,binary:longest_common_suffix([maps:get(path,C),<<"-1">>])} of
        {<<"fail-second">>,2} -> error(intentional_second_initialization_failure);
        {<<"block-init">>,_} -> process_flag(trap_exit,true),receive never -> ok end;
        _ -> ok
    end,
    T=ets:new(private_counter,[private,set]),ets:insert(T,{count,0}),
    {ok,#{table=>T,prefix=>Prefix,context=>C}}.
parrot_service(<<"inc">>,_,#{table:=T}=S) -> reply(ets:update_counter(T,count,1),S);
parrot_service(<<"count">>,_,#{table:=T}=S) -> [{count,N}]=ets:lookup(T,count),reply(N,S);
parrot_service(<<"magic">>,_,S) -> reply(?MAGIC,S);
parrot_service(<<"identity">>,_,S) -> {reply,{<<"pid">>,list_to_binary(os:getpid())},S};
parrot_service(<<"context">>,_,#{context:=C}=S) -> {reply,{<<"context">>,term_to_binary(C)},S};
parrot_service(<<"lazy">>,_,S) ->
    {ok,B}=file:read_file(filename:join(filename:dirname(code:which(?MODULE)),"resource.txt")),
    {reply,{<<"lazy">>,<<(test_beam_host_dependency:value()):64/little,B/binary>>},S};
parrot_service(<<"block">>,_,#{prefix:=P}=S) ->
    ok=file:write_file(P++".entered",<<1>>),wait_file(P++".resume"),parrot_service(<<"inc">>,<<>>,S);
parrot_service(<<"trap">>,_,S) -> process_flag(trap_exit,true),reply(0,S);
parrot_service(<<"die">>,_,_) -> exit(self(),kill);
parrot_service(<<"halt">>,_,_) -> erlang:halt(73);
parrot_service(<<"invalid_reply">>,_,S) -> {reply,{not_a_binary,<<>>},S#{reply_marker=>1}};
parrot_service(<<"reply_marker">>,_,S) -> reply(maps:get(reply_marker,S,0),S);
parrot_service(<<"large_reply">>,_,S) -> {reply,{<<"large">>,binary:copy(<<1>>,16777217)},S};
parrot_service(K,_,_) -> error({unknown_service,K}).
reply(N,S) -> {reply,{<<"count">>,<<N:64/little>>},S}.
wait_file(File) -> case filelib:is_file(File) of true -> ok; false -> timer:sleep(5),wait_file(File) end.
