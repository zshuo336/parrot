-module(causal_fixture).
-include_lib("opentelemetry/include/otel_span.hrl").
-include_lib("opentelemetry_api/include/opentelemetry.hrl").
-export([parrot_init/1,parrot_service/3,start_sdk/0,snapshot/0]).

start_sdk() ->
    case ets:info(parrot_causal_spans) of
        undefined ->
            ets:new(parrot_causal_spans,[named_table,public,bag]),
            application:load(opentelemetry),
            ok=application:set_env(opentelemetry,sampler,always_on),
            ok=application:set_env(opentelemetry,processors,
                [{otel_simple_processor,#{exporter=>{otel_exporter_tab,parrot_causal_spans}}}]),
            {ok,_}=application:ensure_all_started(opentelemetry);
        _ -> ok
    end,
    parrot_trace:set_tracer(opentelemetry:get_tracer(?MODULE)).

parrot_init(_) ->
    %% Dependencies are component-owned inside this guest, not loaded in the gateway.
    Dir=filename:dirname(code:which(?MODULE)),
    [case filelib:is_dir(filename:join([Dir,Name,"ebin"])) of
        true -> true=code:add_patha(filename:join([Dir,Name,"ebin"])); false -> ok end
     || Name <- ["opentelemetry_api","opentelemetry"]],
    ok=start_sdk(),{ok,0}.

parrot_service(<<"bin:erlang.context">>,Body,State) ->
    {reply,{<<"bin:erlang.context">>,causal_rust_fixture:service(Body)},State+1};
parrot_service(<<"panic">>,_,_) -> erlang:error(<<"private-exception-text">>);
parrot_service(<<"snapshot">>,_,State) -> {reply,{<<"spans">>,term_to_binary(snapshot())},State};
parrot_service(<<"forward">>,Path,State) ->
    {<<"inspect">>,Next}=parrot_gw:service(Path,<<"inspect">>,<<>>),
    {reply,{<<"forward">>,term_to_binary({parrot_trace:current(),binary_to_term(Next,[safe])})},State};
parrot_service(<<"inspect">>,_,State) ->
    Value=#{parent=>parrot_trace:current(),operation=>parrot_trace:operation(),
            os_pid=>list_to_binary(os:getpid()),worker=>self(),metadata=>logger:get_process_metadata()},
    {reply,{<<"inspect">>,term_to_binary(Value)},State+1}.

snapshot() ->
    [#{trace=>S#span.trace_id,span=>S#span.span_id,parent=>S#span.parent_span_id,
       name=>S#span.name,attributes=>otel_attributes:map(S#span.attributes),
       status=>status(S#span.status),ended=>S#span.end_time=/=undefined}
     || S <- ets:tab2list(parrot_causal_spans)].

status(#status{code=Code}) -> Code;
status(undefined) -> undefined.
