use once_cell::sync::Lazy;

use crate::config::Config;

pub static CONFIG: Lazy<Config> = Lazy::new(|| Config::from_env().unwrap());
