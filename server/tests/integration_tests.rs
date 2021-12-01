use dropshot::{
    ConfigDropshot, ConfigLogging, ConfigLoggingIfExists, ConfigLoggingLevel,
    HttpServer, HttpServerStarter,
};
use propolis_client::{Client, Error as ClientError};
use propolis_server::{
    config::{BlockDevice, Config, Device},
    server,
};
use slog::{o, Logger};
use std::collections::BTreeMap;
use uuid::Uuid;

mod artifacts;

use artifacts::setup;

fn initialize_log(test_name: &str) -> Logger {
    let path = format!("/tmp/{}.log", test_name);
    eprintln!("Logging at {}", path);
    let config_logging = ConfigLogging::File {
        level: ConfigLoggingLevel::Info,
        if_exists: ConfigLoggingIfExists::Truncate,
        path,
    };
    config_logging.to_logger(test_name).unwrap()
}

async fn initialize_server(log: &Logger) -> HttpServer<server::Context> {
    let artifacts = setup().await;

    let mut block_options = BTreeMap::new();
    block_options.insert(
        "path".to_string(),
        toml::value::Value::String(
            artifacts.image.path().as_path().to_str().unwrap().to_string(),
        ),
    );
    let bd0 =
        BlockDevice { bdtype: "file".to_string(), options: block_options };

    let mut block_devices = BTreeMap::new();
    block_devices.insert("bd0".to_string(), bd0);

    let mut device_options = BTreeMap::new();
    device_options.insert(
        "pci-path".to_string(),
        toml::value::Value::String("0.4.0".to_string()),
    );
    device_options.insert(
        "block_dev".to_string(),
        toml::value::Value::String("bd0".to_string()),
    );
    let dev = Device {
        driver: "pci-virtio-block".to_string(),
        options: device_options,
    };

    let mut devices = BTreeMap::new();
    devices.insert("block0".to_string(), dev);

    let config = Config::new(artifacts.bootrom.path(), devices, block_devices);
    let context = server::Context::new(config, log.new(slog::o!()));

    let config_dropshot = ConfigDropshot {
        bind_address: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    };
    let log = log.new(o!("component" => "server"));
    HttpServerStarter::new(&config_dropshot, server::api(), context, &log)
        .unwrap()
        .start()
}

#[tokio::test]
async fn test_uninitialized_server() {
    let log = initialize_log("test_uninitialized_server");
    let server = initialize_server(&log).await;
    let client =
        Client::new(server.local_addr(), log.new(o!("component" => "client")));
    assert!(matches!(
        client.instance_get(Uuid::nil()).await,
        Err(ClientError::Status(500))
    ));

    server.close().await.unwrap();
}

#[cfg(target_os = "illumos")]
mod illumos_integration_tests {
    use super::*;
    use anyhow::{bail, Result};
    use propolis_client::api::{InstanceState, InstanceStateRequested};
    use propolis_client::{Client, Error as ClientError};
    use slog::o;
    use std::str::FromStr;
    use uuid::Uuid;

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
            disks: vec![],
            nics: vec![],
            migrate: None,
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
            let monitor_response =
                client.instance_state_monitor(id, gen).await?;
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
    async fn test_ensure_instance_put_running() {
        let log = initialize_log("test_ensure_instance_put_running");
        let server = initialize_server(&log).await;
        let client = Client::new(
            server.local_addr(),
            log.new(o!("component" => "client")),
        );

        let id =
            Uuid::from_str("0000002a-000c-0005-0c03-0938362b0809").unwrap();
        let ensure_request = create_ensure_request(id);
        client.instance_ensure(&ensure_request).await.unwrap();

        // Validate preliminary information about the newly created instance.
        let instance = client.instance_get(id).await.unwrap().instance;
        assert_eq!(instance.properties, ensure_request.properties);
        assert_eq!(instance.state, InstanceState::Creating);
        assert!(instance.disks.is_empty());
        assert!(instance.nics.is_empty());

        // Set the state to running.
        client
            .instance_state_put(id, InstanceStateRequested::Run)
            .await
            .unwrap();

        // Wait for monitor to report that the transitions have
        // occurred (Creating -> Starting -> Running).
        wait_until_state(&client, id, InstanceState::Running).await.unwrap();

        // Validate an accurate reporting of the state through
        // the "instance_get" interface too.
        let instance = client.instance_get(id).await.unwrap().instance;
        assert_eq!(instance.state, InstanceState::Running);

        // Set the state to "Stop". Observe that the instance is destroyed.
        client
            .instance_state_put(id, InstanceStateRequested::Stop)
            .await
            .unwrap();
        wait_until_state(&client, id, InstanceState::Destroyed).await.unwrap();
        server.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_stop_instance_causes_destroy() {
        let log = initialize_log("test_stop_instance_causes_destroy");
        let server = initialize_server(&log).await;
        let client = Client::new(
            server.local_addr(),
            log.new(o!("component" => "client")),
        );

        let id =
            Uuid::from_str("0000002a-000c-0005-0c03-0938362b080a").unwrap();
        let ensure_request = create_ensure_request(id);
        client.instance_ensure(&ensure_request).await.unwrap();

        // Set the state to "Run". Observe "Running".
        client
            .instance_state_put(id, InstanceStateRequested::Run)
            .await
            .unwrap();
        wait_until_state(&client, id, InstanceState::Running).await.unwrap();

        // Set the state to "Stop". Observe that the instance is destroyed.
        client
            .instance_state_put(id, InstanceStateRequested::Stop)
            .await
            .unwrap();
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
        let log = initialize_log("test_reboot_returns_to_running");
        let server = initialize_server(&log).await;
        let client = Client::new(
            server.local_addr(),
            log.new(o!("component" => "client")),
        );

        let id =
            Uuid::from_str("0000002a-000c-0005-0c03-0938362b080b").unwrap();
        let ensure_request = create_ensure_request(id);
        client.instance_ensure(&ensure_request).await.unwrap();

        // Set the state to "Run". Observe "Running".
        client
            .instance_state_put(id, InstanceStateRequested::Run)
            .await
            .unwrap();
        wait_until_state(&client, id, InstanceState::Running).await.unwrap();

        // Reboot the instance. Observe that it becomes running once again.
        client
            .instance_state_put(id, InstanceStateRequested::Reboot)
            .await
            .unwrap();
        wait_until_state(&client, id, InstanceState::Running).await.unwrap();

        // Set the state to "Stop". Observe that the instance is destroyed.
        client
            .instance_state_put(id, InstanceStateRequested::Stop)
            .await
            .unwrap();
        wait_until_state(&client, id, InstanceState::Destroyed).await.unwrap();
        server.close().await.unwrap();
    }
}
