%% Bounded, deployment-owned filesystem copy. No code is loaded in this VM.
-module(parrot_beam_snapshot).
-include_lib("kernel/include/file.hrl").
-export([create/4, remove/1,claim/1]).
%% Only a successful exclusive mkdir grants cleanup ownership.
claim(Dir) ->
    case file:make_dir(Dir) of
        ok -> {owned,file:change_mode(Dir,8#700)};
        Error -> {unowned,Error}
    end.
create(Root,File,Entry,Deadline) ->
    Data=filename:join(Root,"data"),Boot=filename:join(Root,"boot"),
    ok=file:make_dir(Data),ok=file:make_dir(Boot),
    {Bytes,_}=copy_dir(filename:dirname(File),Data,filename:basename(File),Entry,0,0,0,Deadline),
    %% Bootstrap code is pinned separately from component dependencies.
    BootBytes=lists:foldl(fun({M,B},Total) ->
        true=Total+byte_size(B)=<1048576,
        ok=file:write_file(filename:join(Boot,atom_to_list(M)++".beam"),B,[exclusive]),Total+byte_size(B)
     end,0,parrot_beam_host:bootstrap()),
    {Data,Boot,Bytes+BootBytes}.
copy_dir(Source,Dest,Base,Entry,Depth,Bytes,Count,Deadline) ->
    true=Depth=<32,true=erlang:monotonic_time(millisecond)<Deadline,
    {ok,#file_info{type=directory}}=file:read_link_info(Source),
    {ok,Names}=file:list_dir(Source),true=length(Names)+Count=<4096,
    lists:foldl(fun(Name,{B,N}) ->
        true=N<4096,true=erlang:monotonic_time(millisecond)<Deadline,
        S=filename:join(Source,Name),D=filename:join(Dest,Name),
        case file:read_link_info(S) of
            {ok,#file_info{type=directory}} ->
                ok=file:make_dir(D),copy_dir(S,D,undefined,undefined,Depth+1,B,N+1,Deadline);
            {ok,#file_info{type=regular,size=Size}} when Size+B=<67108864 ->
                Added=case Name=:=Base of
                    true -> ok=file:write_file(D,Entry,[exclusive]),byte_size(Entry);
                    false -> copy_regular(S,D,67108864-B,Deadline)
                end,
                true=B+Added=<67108864,{B+Added,N+1};
            Other -> erlang:error({invalid_snapshot_entry,Name,Other})
        end
    end,{Bytes,Count},lists:sort(Names)).
copy_regular(Source,Dest,Limit,Deadline) ->
    {ok,Input}=file:open(Source,[read,binary,raw]),
    try
        {ok,#file_info{type=regular}}=file:read_file_info(Input),
        {ok,Output}=file:open(Dest,[write,binary,raw,exclusive]),
        try copy_bytes(Input,Output,Limit,0,Deadline) after file:close(Output) end
    after file:close(Input) end.
copy_bytes(Input,Output,Limit,Count,Deadline) ->
    true=erlang:monotonic_time(millisecond)<Deadline,
    case file:read(Input,65536) of
        eof -> Count;
        {ok,B} -> true=Count+byte_size(B)=<Limit,ok=file:write(Output,B),
                  copy_bytes(Input,Output,Limit,Count+byte_size(B),Deadline);
        {error,R} -> erlang:error({snapshot_read,R})
    end.
remove(Dir) -> case file:del_dir_r(Dir) of ok -> ok; {error,enoent} -> ok; Error -> Error end.
