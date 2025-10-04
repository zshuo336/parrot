#!/usr/bin/env escript
%%! -noshell -pa out
%% Erlang 对等基准入口（escript 规避 erl -eval 的 boot watchdog）
main(_) -> parrot_bench:main([]).
