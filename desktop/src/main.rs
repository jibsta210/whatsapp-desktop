#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod app;
mod bridge;
mod ui;

use app::WhatsAppApp;

fn main() {
    env_logger::init();

    let app = WhatsAppApp::new();
    app.run();
}
