%% Explicit fixed-size identities; the embedding host owns its OpenTelemetry SDK.
-module(parrot_trace).
-export([current/0, operation/0, validate/1, split/1, scope/3, capture/1,
         handler/4, set_tracer/1, clear_tracer/0]).
-define(CURRENT, {?MODULE,current}).
-define(TRACER, {?MODULE,tracer}).

current() -> case get(?CURRENT) of {Parent,_} -> Parent; _ -> undefined end.
operation() -> case get(?CURRENT) of {_,Op} -> Op; _ -> 'receive' end.

validate(undefined) -> undefined;
validate(<<1,Trace:128,Span:64>>) when Trace>0,Span>0 -> <<1,Trace:128,Span:64>>;
validate(_) -> erlang:error(invalid_causal_parent).

split(<<1,Trace:128,Span:64,Rest/binary>>) when Trace>0,Span>0 ->
    %% Do not retain an entire incoming frame through a small sub-binary parent.
    {<<1,Trace:128,Span:64>>,Rest};
split(_) -> erlang:error(invalid_causal_parent).

set_tracer({Module,_}=Tracer) when is_atom(Module) ->
    [{module,M}=code:ensure_loaded(M) || M <- [otel_ctx,otel_tracer,otel_span,Module]],
    true=erlang:function_exported(otel_tracer,from_remote_span,3),
    true=erlang:function_exported(Module,start_span,4),
    persistent_term:put(?TRACER,Tracer),ok;
set_tracer(_) -> erlang:error(invalid_tracer).
clear_tracer() -> persistent_term:erase(?TRACER),ok.

scope(Parent0,Operation,Fun) when is_function(Fun,0),
                               (Operation=:=ask orelse Operation=:=tell orelse Operation=:='receive') ->
    Parent=validate(Parent0),
    Context=case code:is_loaded(otel_ctx) of
        false -> disabled;
        _ -> {enabled,otel_context(Parent)}
    end,
    with_context(Parent,Operation,Context,Fun).

otel_context(undefined) -> otel_ctx:new();
otel_context(<<1,Trace:128,Span:64>>) ->
    %% Wire carries identities only, not a sampled flag or tracestate.
    otel_tracer:set_current_span(otel_ctx:new(),otel_tracer:from_remote_span(Trace,Span,0)).

with_context(Parent,Operation,Context,Fun) ->
    Previous=get(?CURRENT),Metadata=logger:get_process_metadata(),
    case Metadata of
        undefined -> ok;
        _ -> logger:set_process_metadata(maps:without([otel_trace_id,otel_span_id,otel_trace_flags],Metadata))
    end,
    Token=case Context of disabled -> disabled; {enabled,Ctx} -> {enabled,otel_ctx:attach(Ctx)} end,
    put(?CURRENT,{Parent,Operation}),
    try Fun()
    after
        case Previous of undefined -> erase(?CURRENT); _ -> put(?CURRENT,Previous) end,
        try case Token of disabled -> ok; {enabled,T} -> otel_ctx:detach(T) end
        after
            %% The API's detach(undefined) leaves logger span fields behind.
            case Metadata of undefined -> logger:unset_process_metadata(); _ -> logger:set_process_metadata(Metadata) end
        end
    end.

capture(Fun) when is_function(Fun,0) ->
    Parent=current(),Op=operation(),
    Context=case code:is_loaded(otel_ctx) of false -> disabled; _ -> {enabled,otel_ctx:get_current()} end,
    fun() -> with_context(Parent,Op,Context,Fun) end.

handler(Target,Parent,Operation,Fun) when is_binary(Target),is_function(Fun,0) ->
    scope(Parent,Operation,fun() ->
        case {Parent,persistent_term:get(?TRACER,undefined)} of
            {undefined,_} -> Fun();
            {_,undefined} -> Fun();
            {_,Tracer} -> recorded(Target,Operation,Tracer,Fun)
        end
    end).

recorded(Target,Operation,Tracer,Fun) ->
    Bounded=bounded(Target,Target,0),
    Span=otel_tracer:start_span(otel_ctx:get_current(),Tracer,<<"parrot.erlang.handler">>,
        #{kind=>internal,attributes=>#{<<"parrot.kind">>=>atom_to_binary(Operation,utf8),
          <<"parrot.target">>=>Bounded,<<"parrot.target_truncated">>=>Bounded=/=Target}}),
    Parent=case otel_span:is_valid(Span) of
        true -> <<1,(otel_span:trace_id(Span)):128,(otel_span:span_id(Span)):64>>;
        false -> current()
    end,
    Context=otel_tracer:set_current_span(otel_ctx:get_current(),Span),
    try with_context(Parent,Operation,{enabled,Context},fun() ->
        try
            Result=Fun(),
            otel_span:set_attribute(Span,<<"parrot.outcome">>,<<"Returned">>),Result
        catch Class:Reason:Stack ->
            otel_span:set_status(Span,error),
            otel_span:set_attribute(Span,<<"parrot.outcome">>,<<"Panicked">>),
            erlang:raise(Class,Reason,Stack)
        end
    end)
    after otel_span:end_span(Span) end.

bounded(<<>>,Original,_) -> Original;
bounded(<<Point/utf8,Rest/binary>>,Original,Bytes) ->
    Width=byte_size(<<Point/utf8>>),
    case Bytes+Width=<256 of
        true -> bounded(Rest,Original,Bytes+Width);
        false -> binary:part(Original,0,Bytes)
    end;
bounded(_,Original,Bytes) -> binary:part(Original,0,Bytes).
