#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    aquatic_cli_helpers::run_app_with_cli_and_config::<aquatic_udp::config::Config>(
        aquatic_udp::APP_NAME,
        aquatic_udp::APP_VERSION,
        aquatic_udp::run,
        None,
    )
}
