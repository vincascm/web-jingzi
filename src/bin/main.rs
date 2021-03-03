use anyhow::Result;

use web_jingzi::server::run;

fn main() -> Result<()> {
    env_logger::init();
    let config_file = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.yaml".to_string());
    std::env::set_var("CONFIG_FILE", config_file);
    run()
}
