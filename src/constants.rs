use once_cell::sync::Lazy;

use crate::{config::Config, server::Forward};

pub static CONFIG: Lazy<Config> = Lazy::new(|| Config::from_env().unwrap());
pub static FORWARD: Lazy<Forward> =
    Lazy::new(|| Forward::new(&CONFIG.domain_name, &CONFIG.use_https));
