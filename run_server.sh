while true; do
    echo ----------------------
    lldb -b -O "script import adapter; adapter.run_tcp_server(multiple=False)"
done
