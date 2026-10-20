mod composer;
mod imk;
mod imk_client;
mod keymap;
mod kkc_bridge;
mod llm_bridge;
mod loanword;
mod outbound;
mod platform;
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
