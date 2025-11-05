%% Exact startup errors and actual socket ownership, no fixed startup sleep.
-module(test_endpoints).
-export([run/0]).
run() ->
    {ok, Occupied} = gen_tcp:listen(0, [inet, {ip,{0,0,0,0}}, {active,false}]),
    {ok, Port} = inet:port(Occupied),
    try
        {error,eaddrinuse} = parrot_gw:start(Port),
        io:format("ENDPOINT_OK exact bind failure~n")
    after gen_tcp:close(Occupied) end,
    {ok, Reused} = parrot_gw:start(Port),
    Port = Reused,
    {ok, Sock} = gen_tcp:connect({127,0,0,1}, Port, [binary,{active,false}], 1000),
    gen_tcp:close(Sock),
    io:format("ENDPOINT_OK port reusable after failed bind~n"),
    [begin
        {error,badarg} = parrot_gw:start(P, IP, fun parrot_gw:service/3)
     end || {P,IP} <- [{-1,{0,0,0,0}}, {65536,{0,0,0,0}},
                      {0,{256,0,0,1}}, {0,{0,0,0,-1}},
                      {0,{0,0,0,0,0,0,0,65536}}, {0,{0,0,0,0,0,0,0,bad}}]],
    io:format("ENDPOINT_OK invalid API bind arguments~n"),
    ok.
