use std::{collections::HashMap, fs::File};

use anyhow::Result;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct Config {
    pub listen_address: String,
    pub socks5_server: Option<String>,
    pub domain_name: HashMap<String, String>,
    pub use_https: Option<Vec<String>>,
    pub authorization: Authorization,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let file = std::env::var("CONFIG_FILE")?;
        let file = File::open(&file)?;
        let config = serde_yaml::from_reader(file)?;
        Ok(config)
    }

    pub fn check_domain(&self) -> Result<()> {
        for i in self.domain_name.keys() {
            for j in self.domain_name.keys() {
                anyhow::ensure!(
                    !(j != i && j.contains(i)),
                    "conflict two domain \"{}\" and \"{}\"",
                    j,
                    i
                )
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Debug)]
pub struct Authorization {
    pub enabled: bool,
    pub domain_list: Option<Vec<String>>,
    pub account: Option<Vec<Account>>,
}

#[derive(Deserialize, PartialEq, Debug)]
pub struct Account {
    pub username: String,
    pub password: String,
}
