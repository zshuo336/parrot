#!/usr/bin/env escript
%%! -noshell
main([CodeDirectory | Args]) ->
    true = code:add_patha(CodeDirectory),
    parrot_bench:main(Args);
main(_) -> erlang:error(missing_private_code_directory).
