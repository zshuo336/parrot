-module(test_beam_artifacts).
-export([run/0,compile_fixture/2]).
run() ->
    Tests=[{"invalid explicit URI cannot fall back to code path",fun(D) ->
        compile_fixture(D,1),true=code:add_patha(D),
        [begin
            {failed,16#0A06,_}=deploy(<<"bad">>,U),
            {failed,16#0A03,_}=parrot_gw:admin_status(<<"/user/bad">>)
         end || U <- [<<"https://invalid/artifact">>,<<"file://remote/tmp/x">>,
                       list_to_binary("file://"++D++"?q=1"),list_to_binary("file://"++D++"#fragment"),
                       <<"file://relative">>,<<"file:///tmp/%QZ">>,<<"file:///tmp/%">>,
                       <<"file:///tmp/%00">>,<<"file:///tmp/%FF">>,<<"file://user@localhost/tmp">>,
                       <<"file://localhost:80/tmp">>,<<"file:relative">>,<<>>]] end},
      {"percent encoded directory with spaces and Unicode is loaded",fun(D) ->
        Space=unicode:characters_to_binary(filename:join(D,"space dir 中文")),
        %% Binary paths name the same UTF-8 bytes regardless of VM locale.
        ok=file:make_dir(Space),
        Native=case file:native_name_encoding() of utf8 -> unicode:characters_to_list(Space); latin1 -> binary_to_list(Space) end,
        compile_fixture(Native,7),
        Uri=iolist_to_binary(["file://",uri_string:quote(Space,"/")]),
        {deployed,_}=deploy(<<"encoded">>,Uri),7=magic(<<"/user/encoded">>) end},
      {"explicit directory takes precedence over environment search paths",fun(D) ->
        A=filename:join(D,"a"),B=filename:join(D,"b"),ok=file:make_dir(A),ok=file:make_dir(B),
        compile_fixture(A,1),compile_fixture(B,2),os:putenv("PARROT_ARTIFACT_DIR",B),
        {deployed,_}=deploy(<<"chosen">>,uri(A)),1=magic(<<"/user/chosen">>) end},
      {"missing explicit artifact does not reuse an older directory",fun(D) ->
        compile_fixture(D,1),{deployed,_}=deploy(<<"first">>,uri(D)),
        Empty=filename:join(D,"empty"),ok=file:make_dir(Empty),
        {failed,16#0A06,_}=deploy(<<"missing">>,uri(Empty)),
        1=magic(<<"/user/first">>) end},
      {"missing module requests do not allocate permanent atoms",fun(D) ->
        miss(<<"missing_warmup">>,D),miss(<<"missing_warmup">>,D),
        Before=erlang:system_info(atom_count),
        [miss(<<"parrot_missing_143_",(integer_to_binary(I))/binary>>,D) || I <- lists:seq(1,128)],
        After=erlang:system_info(atom_count),true=(After-Before=<4) end},
      {"failed load does not change the code search path",fun(D) ->
        Before=code:get_path(),{failed,16#0A06,_}=deploy(<<"missing">>,uri(D)),Before=code:get_path() end},
      {"module-name mismatch cannot replace a valid deployed module",fun(D) ->
        compile_fixture(D,3),{deployed,_}=deploy(<<"old">>,uri(D)),
        Src=filename:join(D,"other_name.erl"),ok=file:write_file(Src,"-module(other_name).\n-export([magic/0]).\nmagic()->999.\n"),
        {ok,other_name}=compile:file(Src,[{outdir,D}]),
        {ok,B}=file:read_file(filename:join(D,"other_name.beam")),ok=file:write_file(filename:join(D,"beam_fixture.beam"),B),
        {failed,16#0A06,_}=deploy(<<"wrong">>,uri(D)),3=magic(<<"/user/old">>) end},
      {"localhost file authority is accepted",fun(D) ->
        compile_fixture(D,8),{deployed,_}=deploy(<<"chosen">>,iolist_to_binary(["file://localhost",D])),
        8=magic(<<"/user/chosen">>) end},
      {"omitted URI searches app directory before environment root",fun(D) ->
        A=filename:join(D,"beam_fixture"),ok=file:make_dir(A),
        compile_fixture(D,2),compile_fixture(A,1),os:putenv("PARROT_ARTIFACT_DIR",D),
        {deployed,_}=deploy(<<"chosen">>,undefined),1=magic(<<"/user/chosen">>) end},
      {"invalid bytecode preserves live code and search paths",fun(D) ->
        compile_fixture(D,3),{deployed,_}=deploy(<<"old">>,uri(D)),Before=code:get_path(),
        File=filename:join(D,"beam_fixture.beam"),
        {ok,beam_fixture,Chunks}=beam_lib:all_chunks(File),
        {ok,BadCode}=beam_lib:build_module(lists:keyreplace("Code",1,Chunks,{"Code",<<0>>})),
        [begin ok=file:write_file(File,Bytes),{failed,16#0A06,_}=deploy(<<"wrong">>,uri(D)),
               3=magic(<<"/user/old">>),Before=code:get_path()
         end || Bytes <- [BadCode,<<>>,<<"invalid">>,<<"FOR1",12:32/big,"BEAM","AtU8",100:32/big>>]] end},
      {"symlink and oversized artifact are rejected without path changes",fun(D) ->
        compile_fixture(D,3),File=filename:join(D,"beam_fixture.beam"),
        Real=filename:join(D,"real.beam"),ok=file:rename(File,Real),ok=file:make_symlink(Real,File),
        Before=code:get_path(),{failed,16#0A06,_}=deploy(<<"wrong">>,uri(D)),Before=code:get_path(),
        ok=file:delete(File),{ok,F}=file:open(File,[write,binary]),
        {ok,16777216}=file:position(F,16777216),ok=file:write(F,<<0>>),ok=file:close(F),
        {failed,16#0A06,_}=deploy(<<"wrong">>,uri(D)),Before=code:get_path() end},
      {"local loader third version never kills a process executing old code",fun(D) ->
        compile_fixture(D,1),beam_fixture=parrot_gw:load_beam(<<"beam_fixture">>,uri(D)),
        Parent=self(),P=spawn(fun() -> beam_fixture:hold(Parent) end),
        receive {holding,P} -> ok after 1000 -> erlang:error(hold_not_started) end,
        try
            compile_fixture(D,2),beam_fixture=parrot_gw:load_beam(<<"beam_fixture">>,uri(D)),
            beam_fixture=parrot_gw:load_beam(<<"beam_fixture">>,uri(D)),true=is_process_alive(P),
            compile_fixture(D,3),Before=code:get_path(),
            rejected=try parrot_gw:load_beam(<<"beam_fixture">>,uri(D)),accepted
                     catch error:{old_code_in_use,<<"beam_fixture">>} -> rejected end,
            true=is_process_alive(P),{reply,{<<"magic">>,<<2:64/little>>},undefined}=beam_fixture:parrot_service(<<>>,<<>>,undefined),
            Before=code:get_path(),Ref=monitor(process,P),P!finish,
            receive {'DOWN',Ref,process,P,normal} -> ok after 1000 -> erlang:error(hold_not_finished) end,
            beam_fixture=parrot_gw:load_beam(<<"beam_fixture">>,uri(D)),
            {reply,{<<"magic">>,<<3:64/little>>},undefined}=beam_fixture:parrot_service(<<>>,<<>>,undefined)
        after exit(P,kill) end end},
      {"runtime metadata changes are not skipped as identical code",fun(D) ->
        [begin
            Src=filename:join(D,"beam_fixture.erl"),
            Text=io_lib:format("-module(beam_fixture).~n-marker(~p).~n-export([parrot_init/1,parrot_service/3]).~nparrot_init(_)->{ok,undefined}.~nparrot_service(_,_,S)->[N]=proplists:get_value(marker,?MODULE:module_info(attributes)),{reply,{<<\"magic\">>,<<N:64/little>>},S}.~n",[I]),
            ok=file:write_file(Src,Text),{ok,beam_fixture}=compile:file(Src,[{outdir,D}]),
            {deployed,_}=deploy(<<"chosen">>,uri(D)),I=magic(<<"/user/chosen">>)
         end || I <- [1,2]] end},
      {"on_load code is rejected before side effects",fun(D) ->
        Src=filename:join(D,"beam_fixture.erl"),Marker=filename:join(D,"ran"),
        ok=file:write_file(Src,io_lib:format("-module(beam_fixture).~n-on_load(init/0).~n-export([init/0]).~ninit()->file:write_file(~p,<<1>>).~n",[Marker])),
        {ok,beam_fixture}=compile:file(Src,[{outdir,D}]),Before=code:get_path(),
        {failed,16#0A06,_}=deploy(<<"wrong">>,uri(D)),false=filelib:is_file(Marker),Before=code:get_path() end}],
    Failed=lists:foldl(fun({Name,F},N) ->
        try fixture(F),io:format("BEAM_ARTIFACT_OK ~s~n",[Name]),N
        catch C:R:Stack -> io:format("BEAM_ARTIFACT_FAIL ~s ~p:~p ~p~n",[Name,C,R,Stack]),N+1 end
    end,0,Tests),
    case Failed of 0 -> ok; _ -> erlang:error({beam_artifact_failures,Failed}) end.
fixture(F) ->
    D=filename:join("/tmp","parrot-beam-143-"++os:getpid()++"-"++integer_to_list(erlang:unique_integer([positive]))),
    ok=file:make_dir(D),OldPath=code:get_path(),OldEnv=os:getenv("PARROT_ARTIFACT_DIR"),parrot_gw:admin_init(),
    try F(D)
    after
        [parrot_gw:admin_stop(<<"/user/",N/binary>>) || N <- [<<"bad">>,<<"encoded">>,<<"chosen">>,<<"first">>,<<"missing">>,<<"old">>,<<"wrong">>]],
        code:purge(beam_fixture),code:delete(beam_fixture),code:purge(other_name),code:delete(other_name),
        code:set_path(OldPath),file:del_dir_r(D),
        case OldEnv of false -> os:unsetenv("PARROT_ARTIFACT_DIR"); _ -> os:putenv("PARROT_ARTIFACT_DIR",OldEnv) end
    end.
uri(D) -> list_to_binary("file://"++D).
deploy(Name,Uri) -> parrot_gw:admin_deploy({Name,<<"1">>,{beam,<<"beam_fixture">>,Uri},singleton,undefined}).
miss(App,D) -> {failed,16#0A06,_}=parrot_gw:admin_deploy({<<"missing">>,<<"1">>,{beam,App,uri(D)},singleton,undefined}).
magic(Path) -> {<<"magic">>,<<N:64/little>>}=parrot_gw:service(Path,<<"get">>,<<>>),N.
compile_fixture(D,Magic) ->
    Src=filename:join(D,"beam_fixture.erl"),
    ok=file:write_file(Src,io_lib:format("-module(beam_fixture).~n-fixture_kind([holdable]).~n-export([parrot_init/1,parrot_service/3,hold/1]).~nhold(P) -> P ! {holding,self()}, receive finish -> ok end.~nparrot_init(_) -> {ok,undefined}.~nparrot_service(_,_,S) -> {reply,{<<\"magic\">>,<<~p:64/little>>},S}.~n",[Magic])),
    {ok,beam_fixture}=compile:file(Src,[{outdir,D}]),ok.
