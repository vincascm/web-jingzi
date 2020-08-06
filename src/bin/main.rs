use anyhow::Result;

use web_jingzi::server::run;

fn main() -> Result<()> {
    env_logger::init();
    std::env::set_var("CONFIG_FILE", "config.yaml");
    run()
}
