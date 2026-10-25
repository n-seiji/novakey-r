#![deny(unsafe_code)]
mod composer;
mod config;
mod controller;
mod deps;
mod imk;
mod imk_client;
mod input;
mod judge;
mod keymap;
mod kkc;
mod emoji;
mod english_memory;
mod english_split;
mod loanword;
mod outbound;
mod platform;
mod private_fs;
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
    if let Some(i) = args.iter().position(|a| a == "--judge-dump") {
        simulate::setup_env();
        std::process::exit(judge::dump_cli(args.get(i + 1).map_or("", String::as_str)));
    }
    if let Some(i) = args.iter().position(|a| a == "--judge-bench") {
        simulate::setup_env();
        std::process::exit(judge::bench_cli(args.get(i + 1).map_or("", String::as_str)));
    }
    if let Some(i) = args.iter().position(|a| a == "--simulate") {
        simulate::simulate(args.get(i + 1).map_or("", String::as_str), args.iter().any(|a| a == "--check-preedit"));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--simulate-file") {
        simulate::simulate_file(args.get(i + 1).map_or("", String::as_str), args.iter().any(|a| a == "--check-preedit"));
        return;
    }

    if args.iter().any(|a| a == "--check-main-queue") {
        platform::check_main_queue();
    }

    config::init(config::Config::from_env());
    platform::begin_activity();
    judge::preload();
    platform::install_panic_hook();
    imk::register_controller();
    imk::run();
}
