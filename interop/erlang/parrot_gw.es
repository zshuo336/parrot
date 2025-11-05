#!/usr/bin/env escript
%%! -noshell
%% Compile this bundle into memory and pin guest bootstrap bytes for deployments.
main(Args) ->
    Dir=filename:dirname(escript:script_name()),
    [begin
        Src=filename:join(Dir,atom_to_list(M)++".erl"),
        case compile:file(Src,[binary,verbose]) of
            {ok,M,Bin} ->
                {module,M}=code:load_binary(M,Src,Bin),
                case lists:member(M,[parrot_trace,parrot_beam_guest,parrot_component_runtime,parrot_gw]) of
                    true -> persistent_term:put({parrot_beam_bootstrap,M},Bin);
                    false -> ok
                end;
            Error -> io:format(standard_error,"compile ~p failed: ~p~n",[M,Error]),halt(2)
        end
     end || M <- [parrot_trace,parrot_component_runtime,parrot_gw,parrot_beam_guest,parrot_beam_snapshot,parrot_beam_host]],
    parrot_gw:main(Args).
