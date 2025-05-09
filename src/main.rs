use anyhow::Result;

use web_jingzi::server::run;

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let config_file = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    unsafe {
        std::env::set_var("CONFIG_FILE", config_file);
    }
    run()
}
