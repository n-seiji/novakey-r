mod composer;
mod imk;
mod imk_client;
mod keymap;
mod kkc;
mod kkc_bridge;
mod llm_bridge;
mod emoji;
mod loanword;
mod outbound;
mod platform;
mod rerank;
mod romaji_converter;
mod session;
mod simulate;
mod symbols;
mod trace;
mod translit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--test-kkc") {
        kkc_bridge::smoke_test(args.get(i + 1).map(String::as_str));
        return;
    }

    if let Some(i) = args.iter().position(|a| a == "--kkc-dump") {
        let (Some(corpus), Some(out)) = (args.get(i + 1), args.get(i + 2)) else {
            eprintln!("usage: novakey_r --kkc-dump <corpus> <out.jsonl> [--typed]");
            std::process::exit(2);
        };
        let typed = args.iter().any(|a| a == "--typed");
        std::process::exit(kkc_bridge::dump(corpus, out, typed));
    }
    if let Some(i) = args.iter().position(|a| a == "--kkc-check") {
        std::process::exit(kkc::check::run_cli(args.get(i + 1).map(String::as_str)));
    }
    if args.iter().any(|a| a == "--kkc-learn-compare") {
        std::process::exit(kkc::learn_compare::run(&args));
    }
    if args.iter().any(|a| a == "--kkc-compare") {
        std::process::exit(kkc::compare::run(&args));
    }
    if let Some(i) = args.iter().position(|a| a == "--kkc-convert") {
        std::process::exit(kkc::compare::convert_cli(args.get(i + 1).map_or("", String::as_str), args.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(30)));
    }
    if let Some(i) = args.iter().position(|a| a == "--kkc-bench") {
        std::process::exit(kkc::compare::bench_cli(args.get(i + 1).map_or("", String::as_str), args.iter().any(|a| a == "--swift")));
    }
    if let Some(i) = args.iter().position(|a| a == "--kkc-dump-numbers") {
        let (Some(out), Some(width_out)) = (args.get(i + 1), args.get(i + 2)) else {
            eprintln!("usage: novakey_r --kkc-dump-numbers <kansuji.tsv> <width_map.tsv>");
            std::process::exit(2);
        };
        std::process::exit(kkc_bridge::dump_numbers(out, width_out));
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

    imk::register_controller();
    imk::init_llm_dispatch();
    imk::run();
}
