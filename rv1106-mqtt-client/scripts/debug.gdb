# debug.gdb — PC 调试 mqtt-client (host debug build, 带 debuginfo)
# 用法:
#   ./target/debug/mqtt-client 已在 EMQX(127.0.0.1:1883) 环境下运行
#   gdb -x scripts/debug.gdb ./target/debug/mqtt-client
# 交互式: 启动后停 main, 用 `c` 继续, `bt` 看栈, `p <var>` 看变量
set pagination off
set breakpoint pending on
set debuginfod enabled off

# 关键非内联符号 (已用 nm 确认存在)
break mqtt_client::main
break mqtt_client::state::ConnStateMachine::on_login_sent
break mqtt_client::state::ConnStateMachine::login_reply_timeout
break mqtt_client::state::ConnStateMachine::on_login_reply
break mqtt_client::gcode_translator::translate

run config/mqtt-client.toml
