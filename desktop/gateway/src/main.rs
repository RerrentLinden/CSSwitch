fn main() {
    let args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    // 默认形态:单进程服务(网关 + 控制 API + WebUI)。
    if args.len() == 1 || args.get(1).map(String::as_str) == Some("serve") {
        if let Err(e) = csswitch_gateway::control::serve(port_arg(&args)) {
            csswitch_gateway::log_line!("csswitch: {e}");
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("official-passthrough") {
        if let Err(e) = csswitch_gateway::official_passthrough::run_cli(&args[2..]) {
            eprintln!("csswitch-gateway official-passthrough: {e}");
            std::process::exit(2);
        }
        return;
    }
    match csswitch_gateway::config::GatewayConfig::from_env_args(args) {
        Ok(cfg) => {
            if let Err(e) = csswitch_gateway::server::serve(cfg) {
                csswitch_gateway::log_line!("csswitch-gateway: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            csswitch_gateway::log_line!("csswitch-gateway: {e}");
            std::process::exit(2);
        }
    }
}

fn port_arg(args: &[String]) -> Option<u16> {
    args.iter()
        .position(|arg| arg == "--port")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u16>().ok())
}
