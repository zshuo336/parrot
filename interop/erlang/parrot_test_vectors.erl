%% Test-only bridge to Python's standard JSON parser, including on OTP24.
-module(parrot_test_vectors).
-export([wire/0,admin/0,read/2]).

wire() -> read(wire,"../../docs/vectors/wire1.json").
admin() -> read(admin,"../../docs/vectors/admin_v2.json").

read(Kind,Path) when Kind=:=wire; Kind=:=admin ->
    Python=case os:getenv("PARROT_VECTOR_PYTHON") of
        false -> os:find_executable("python3"); P -> P end,
    true=is_list(Python),
    Script=filename:absname("../../scripts/protocol_vectors.py"),
    Port=open_port({spawn_executable,Python},[binary,exit_status,use_stdio,stderr_to_stdout,
        {args,[Script,atom_to_list(Kind),filename:absname(Path)]}]),
    try
        Rows=receive_rows(Port,[],0,erlang:monotonic_time(millisecond)+10000),
        true=is_list(Rows) andalso Rows=/=[],Rows
    after try port_close(Port) catch _:_ -> ok end end.

receive_rows(Port,Chunks,Size,Deadline) ->
    Remaining=max(0,Deadline-erlang:monotonic_time(millisecond)),
    receive
        {Port,{data,Bytes}} ->
            true=Size+byte_size(Bytes)=<20*1024*1024,
            receive_rows(Port,[Bytes|Chunks],Size+byte_size(Bytes),Deadline);
        {Port,{exit_status,0}} -> binary_to_term(iolist_to_binary(lists:reverse(Chunks)),[safe]);
        {Port,{exit_status,Code}} ->
            Output=iolist_to_binary(lists:reverse(Chunks)),
            error({vector_reader_exit,Code,binary:part(Output,0,min(byte_size(Output),1024))})
    after Remaining -> error(vector_reader_timeout) end.
