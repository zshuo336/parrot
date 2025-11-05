%% Fixed RPC boundary: component terms/exceptions never cross into the gateway.
-module(parrot_beam_guest).
-behaviour(gen_server).
-export([boot/7,invoke/3,invoke/4,invoke/6,retire/1,status/1]).
-export([init/1,handle_call/3,handle_cast/2,handle_info/2]).
boot(App,Dir,Name,Version,Paths,Config,Timeout) ->
    try
        {ok,_}=gen_server:start({local,?MODULE},?MODULE,{App,Dir,Name,Version,Paths,Config},[{timeout,Timeout}]),
        <<0>>
    catch C:R -> diagnostic(C,R) end.
init({App,Dir,Name,Version,Paths,Config}) ->
    %% Root owns ETS and runtime independently of the temporary RPC process.
    parrot_gw:admin_init(),
    Module=parrot_gw:load_beam(App,<<"file://",Dir/binary>>),
    ok=parrot_component_runtime:ensure(self()),
    {deployed,Paths}=parrot_component_runtime:deploy(Name,Version,Paths,fun() -> Module end,Config),
    {ok,Name}.
invoke(Path,Key,Body) -> invoke(Path,Key,Body,16777216).
invoke(Path,Key,Body,ReplyBytes) ->
    invoke(Path,Key,Body,ReplyBytes,undefined,'receive').
invoke(Path,Key,Body,ReplyBytes,Parent,Operation) ->
    try
        {K,B}=parrot_trace:scope(Parent,Operation,fun() -> parrot_gw:service(Path,Key,Body,ReplyBytes) end),
        true=is_binary(K) andalso is_binary(B),
        true=byte_size(K)+byte_size(B)=<ReplyBytes,
        <<0,(byte_size(K)):32/big,K/binary,B/binary>>
    catch
        error:{unknown_service,Key} -> <<1>>;
        error:component_overloaded -> <<2>>;
        error:component_stopped -> <<3>>;
        C:R -> diagnostic(C,R)
    end.
retire(Path) ->
    try
        case parrot_component_runtime:retire(Path,stop,0) of
            {retired,_,_} -> <<0>>;
            not_found -> <<0>>;
            Other -> diagnostic(error,Other)
        end
    catch C:R -> diagnostic(C,R) end.
status(Path) ->
    try case parrot_gw:admin_status(Path) of
        {status_reply,[{Path,<<"running">>,_}]} -> <<0>>;
        _ -> <<1>>
    end catch _:_ -> <<1>> end.
diagnostic(Class,Reason) ->
    B=unicode:characters_to_binary(io_lib:format("~p: ~P",[Class,Reason,8],[{chars_limit,1024}])),
    <<4,B/binary>>.
handle_call(_,_,State) -> {reply,unsupported,State}.
handle_cast(_,State) -> {noreply,State}.
handle_info(_,State) -> {noreply,State}.
