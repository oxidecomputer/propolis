use anyhow::{bail, Result};
use dropshot::{
    ConfigDropshot, ConfigLogging, ConfigLoggingLevel, HttpServer,
    HttpServerStarter,
};
use propolis_client::{
    api::{InstanceState, InstanceStateRequested},
    Client, Error as ClientError,
};
use propolis_server_lib::{
    config::{Config, Device},
    server,
};
use slog::Logger;
use std::collections::BTreeMap;
use std::str::FromStr;
use uuid::Uuid;

mod artifacts;

use artifacts::{setup, TEST_BOOTROM, TEST_IMAGE};

fn initialize_log() -> Logger {
    let config_logging =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    config_logging.to_logger("propolis-server-integration-test").unwrap()
}

async fn initialize_server(log: Logger) -> HttpServer<server::Context> {
    setup().await;

    let mut block_options = BTreeMap::new();
    block_options.insert(
        "disk".to_string(),
        toml::value::Value::String(
            TEST_IMAGE.path().as_path().to_str().unwrap().to_string(),
        ),
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
    HttpServerStarter::new(&config_dropshot, server::api(), context, &log)
        .unwrap()
        .start()
}

// Create a new instance.
//
// NOTE: Many of these values are placeholders, and can be replaced
// when we have "real" integration of UUID-based images / bootroms.
fn create_ensure_request(
    id: Uuid,
) -> propolis_client::api::InstanceEnsureRequest {
    propolis_client::api::InstanceEnsureRequest {
        properties: propolis_client::api::InstanceProperties {
            id,
            name: "test-instance".to_string(),
            description: "it's a test instance".to_string(),
            image_id: Uuid::new_v4(),
            bootrom_id: Uuid::new_v4(),
            memory: 256,
            vcpus: 2,
        },
    }
}

// Utility wrapper for monitoring an instance's state until it reaches a
// requested target.
async fn wait_until_state(
    client: &Client,
    id: Uuid,
    state: InstanceState,
) -> Result<()> {
    let mut gen = 0;
    loop {
        let monitor_response = client.instance_state_monitor(id, gen).await?;
        if monitor_response.gen < gen {
            bail!(
                "Gen should be increasing: (requested {}, saw {})",
                gen,
                monitor_response.gen
            );
        }
        eprintln!(
            "Monitor response: {:?} (waiting for {:?})",
            monitor_response, state
        );
        if monitor_response.state == state {
            return Ok(());
        }
        gen = monitor_response.gen + 1;
    }
}

#[tokio::test]
async fn test_uninitialized_server() {
    let log = initialize_log();
    let server = initialize_server(log.clone()).await;

    let client = Client::new(server.local_addr(), log);
    assert!(matches!(
        client.instance_get(Uuid::nil()).await,
        Err(ClientError::Status(500))
    ));

    server.close().await.unwrap();
}

#[tokio::test]
async fn test_ensure_instance_put_running() {
    let log = initialize_log();
    let server = initialize_server(log.clone()).await;

    let client = Client::new(server.local_addr(), log);

    let id = Uuid::from_str("0000002a-000c-0005-0c03-0938362b0809").unwrap();
    let ensure_request = create_ensure_request(id);
    client.instance_ensure(&ensure_request).await.unwrap();

    // Validate preliminary information about the newly created instance.
    let instance = client.instance_get(id).await.unwrap().instance;
    assert_eq!(instance.properties, ensure_request.properties);
    assert_eq!(instance.state, InstanceState::Creating);
    assert!(instance.disks.is_empty());
    assert!(instance.nics.is_empty());

    // Set the state to running.
    client.instance_state_put(id, InstanceStateRequested::Run).await.unwrap();

    // Wait for monitor to report that the transitions have
    // occurred (Creating -> Starting -> Running).
    wait_until_state(&client, id, InstanceState::Running).await.unwrap();

    // Validate an accurate reporting of the state through
    // the "instance_get" interface too.
    let instance = client.instance_get(id).await.unwrap().instance;
    assert_eq!(instance.state, InstanceState::Running);

    // Set the state to "Stop". Observe that the instance is destroyed.
    client.instance_state_put(id, InstanceStateRequested::Stop).await.unwrap();
    wait_until_state(&client, id, InstanceState::Destroyed).await.unwrap();
    server.close().await.unwrap();
}

#[tokio::test]
async fn test_stop_instance_causes_destroy() {
    let log = initialize_log();
    let server = initialize_server(log.clone()).await;

    let client = Client::new(server.local_addr(), log);

    let id = Uuid::from_str("0000002a-000c-0005-0c03-0938362b080a").unwrap();
    let ensure_request = create_ensure_request(id);
    client.instance_ensure(&ensure_request).await.unwrap();

    // Set the state to "Run". Observe "Running".
    client.instance_state_put(id, InstanceStateRequested::Run).await.unwrap();
    wait_until_state(&client, id, InstanceState::Running).await.unwrap();

    // Set the state to "Stop". Observe that the instance is destroyed.
    client.instance_state_put(id, InstanceStateRequested::Stop).await.unwrap();
    wait_until_state(&client, id, InstanceState::Destroyed).await.unwrap();

    // After the instance has been stopped, the state cannot be modified
    // further.
    //
    // TODO: Probably should update the actual status to something other than
    // 500, but it's important that the server throws an expected error here.
    assert!(matches!(
        client
            .instance_state_put(id, InstanceStateRequested::Run)
            .await
            .unwrap_err(),
        ClientError::Status(500),
    ));
    assert!(matches!(
        client
            .instance_state_put(id, InstanceStateRequested::Stop)
            .await
            .unwrap_err(),
        ClientError::Status(500),
    ));
    assert!(matches!(
        client
            .instance_state_put(id, InstanceStateRequested::Reboot)
            .await
            .unwrap_err(),
        ClientError::Status(500),
    ));

    server.close().await.unwrap();
}

#[tokio::test]
async fn test_reboot_returns_to_running() {
    let log = initialize_log();
    let server = initialize_server(log.clone()).await;

    let client = Client::new(server.local_addr(), log);

    let id = Uuid::from_str("0000002a-000c-0005-0c03-0938362b080b").unwrap();
    let ensure_request = create_ensure_request(id);
    client.instance_ensure(&ensure_request).await.unwrap();

    // Set the state to "Run". Observe "Running".
    client.instance_state_put(id, InstanceStateRequested::Run).await.unwrap();
    wait_until_state(&client, id, InstanceState::Running).await.unwrap();

    // Reboot the instance. Observe that it becomes running once again.
    client
        .instance_state_put(id, InstanceStateRequested::Reboot)
        .await
        .unwrap();
    wait_until_state(&client, id, InstanceState::Running).await.unwrap();

    // Set the state to "Stop". Observe that the instance is destroyed.
    client.instance_state_put(id, InstanceStateRequested::Stop).await.unwrap();
    wait_until_state(&client, id, InstanceState::Destroyed).await.unwrap();
    server.close().await.unwrap();
}
