use dropshot::{
    ConfigDropshot, ConfigLogging,
    ConfigLoggingLevel, HttpServer, HttpServerStarter
};
use reqwest;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use propolis_server_lib::config::{Config, Device};
use propolis_server_lib::server;

mod artifacts;

use artifacts::{setup, TEST_IMAGE, TEST_BOOTROM};

async fn initialize_server() -> HttpServer<server::Context> {
    setup().await;

    let mut block_options = BTreeMap::new();
    block_options.insert(
        "disk".to_string(),
        toml::value::Value::String(TEST_IMAGE.path().as_path().to_str().unwrap().to_string())
    );
    block_options.insert(
        "pci-path".to_string(),
        toml::value::Value::String("0.4.0".to_string()),
    );
    let block = Device {
        driver: "pci-virtio-block".to_string(),
        options: block_options,
    };
    let mut devices = BTreeMap::new();
    devices.insert("block0".to_string(), block);

    let config = Config::new(TEST_BOOTROM.path(), devices);
    let context = server::Context::new(config);

    let config_dropshot = ConfigDropshot {
        bind_address: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    };
    let config_logging =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = config_logging
        .to_logger("propolis-server")
        .unwrap();
    HttpServerStarter::new(&config_dropshot, server::api(), context, &log)
        .unwrap()
        .start()
}

#[tokio::test]
async fn test_simple() {
    let server = initialize_server().await;
    server.close().await.unwrap();

    // TODO: Use server.local_addr() + propolis client

    assert!(false);
}
