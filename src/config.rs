use std::{collections::HashMap, fs::File};

use anyhow::Result;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct Config {
    pub listen_address: String,
    pub domain_name: HashMap<String, String>,
    pub socks5_server: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let file = std::env::var("CONFIG_FILE")?;
        let file = File::open(&file)?;
        let config = serde_yaml::from_reader(file)?;
        Ok(config)
    }
}
