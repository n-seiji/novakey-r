#![deny(unsafe_code)]
mod composer;
mod config;
mod imk;
mod imk_client;
mod input;
mod keymap;
mod kkc;
mod llm_bridge;
mod emoji;
mod english_memory;
mod english_split;
mod loanword;
mod outbound;
mod platform;
mod private_fs;
mod rerank;
mod romaji_converter;
mod session;
mod simulate;
mod symbols;
mod trace;
mod translit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--kkc-check") {
        std::process::exit(kkc::check::run_cli(args.get(i + 1).map(String::as_str)));
    }
    if args.iter().any(|a| a == "--kkc-compare") {
        std::process::exit(kkc::compare::run(&args));
    }
    if let Some(i) = args.iter().position(|a| a == "--kkc-convert") {
        std::process::exit(kkc::compare::convert_cli(args.get(i + 1).map_or("", String::as_str), args.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(30)));
    }
    if let Some(i) = args.iter().position(|a| a == "--kkc-bench") {
        std::process::exit(kkc::compare::bench_cli(args.get(i + 1).map_or("", String::as_str)));
    }
    if let Some(i) = args.iter().position(|a| a == "--simulate") {
        simulate::simulate(args.get(i + 1).map_or("", String::as_str));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--simulate-file") {
        simulate::simulate_file(args.get(i + 1).map_or("", String::as_str));
        return;
    }

    if args.iter().any(|a| a == "--test-llm") {
        llm_bridge::smoke_test();
        return;
    }

    config::init(config::Config::from_env());
    platform::install_panic_hook();
    imk::register_controller();
    imk::init_llm_dispatch();
    imk::run();
}
