#!/usr/bin/env escript
%%! -noshell
%% 网关 escript 入口：编译加载同目录 parrot_gw.erl 后启动。
%% （escript 沙盒无 file server——用 compile:forms 经 epp 预处理不可行；
%%  直接读源码 compile:file 到内存 beam 再 code:load_binary。）
main(Args) ->
    Dir = filename:dirname(escript:script_name()),
    Src = filename:join(Dir, "parrot_gw.erl"),
    case compile:file(Src, [binary, verbose]) of
        {ok, Mod, Bin} ->
            {module, Mod} = code:load_binary(Mod, Src, Bin),
            parrot_gw:main(Args);
        Error ->
            io:format(standard_error, "compile parrot_gw.erl failed: ~p~n", [Error]),
            halt(2)
    end.
