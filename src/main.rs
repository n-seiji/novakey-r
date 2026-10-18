mod composer;
mod imk;
mod imk_client;
mod kkc_bridge;
mod llm_bridge;
mod outbound;
mod platform;
mod romaji_converter;

fn main() {
    if std::env::args().any(|a| a == "--test-kkc") {
        kkc_bridge::smoke_test();
        return;
    }

    if std::env::args().any(|a| a == "--test-llm") {
        llm_bridge::smoke_test();
        return;
    }

    imk::register_controller();
    imk::init_llm_dispatch();
    imk::run();
}
