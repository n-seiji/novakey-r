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
mod symbols;
mod trace;
mod translit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--test-kkc") {
        kkc_bridge::smoke_test(args.get(i + 1).map(String::as_str));
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
