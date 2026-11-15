mod app;
mod config_view;
mod endpoints;
mod filter_bar;
mod flows;
mod icons;
mod map;
mod policies;
mod scene;
mod settings;
mod util;
mod ws;
mod yaml;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
