#![cfg(target_family = "unix")]

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, bail, Context as _, Result};
use nkeys::KeyPair;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::Duration;
use wadm_types::{LinkProperty, Manifest, Properties, TraitProperty};
use wasmcloud_control_interface::{ClientBuilder as CtlClientBuilder, Host};

const DEV_WAIT_TIME: Duration = Duration::from_secs(1200);
const DEV_EXIT_TIME: Duration = Duration::from_secs(60);

mod common;
use common::{
    find_open_port, force_cleanup_processes, init, init_path, start_nats, wait_for_no_hosts,
    wait_for_no_nats, wait_for_no_wadm, wait_for_num_hosts,
};

#[tokio::test]
#[serial_test::serial]
async fn integration_dev_hello_component_serial() -> Result<()> {
    // Force cleanup any lingering processes from previous tests
    force_cleanup_processes().await?;

    wait_for_no_hosts()
        .await
        .context("unexpected wasmcloud instance(s) running")?;
    let test_setup = init(
        /* component_name= */ "hello",
        /* template_name= */ "hello-world-rust",
    )
    .await?;
    let project_dir = test_setup.project_dir.clone();

    let dir = tempfile::tempdir()?;

    wait_for_no_hosts()
        .await
        .context("one or more unexpected wasmcloud instances running")?;

    let nats_port = find_open_port().await?;
    let mut nats = start_nats(nats_port, &dir).await?;
    let ui_port = find_open_port().await?;

    let dev_cmd = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .env("WASMCLOUD_WASH_UI_PORT", ui_port.to_string())
            .args([
                "dev",
                "--nats-connect-only",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
                "--dashboard",
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed running wash dev")?,
    ));
    let watch_dev_cmd = dev_cmd.clone();

    let signed_file_path = Arc::new(project_dir.join("build/http_hello_world_s.wasm"));
    let expected_path = signed_file_path.clone();

    // Wait until the signed file is there (this means dev succeeded)
    let _ = tokio::time::timeout(
        DEV_WAIT_TIME,
        tokio::spawn(async move {
            loop {
                // If the command failed (and exited early), bail
                if let Ok(Some(exit_status)) = watch_dev_cmd.write().await.try_wait() {
                    if !exit_status.success() {
                        bail!("dev command failed");
                    }
                }
                // If the file got built, we know dev succeeded
                if expected_path.exists() {
                    break Ok(());
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }),
    )
    .await
    .context("timed out while waiting for file path to get created")?;
    if !signed_file_path.exists() {
        bail!("signed component file was not built");
    }

    let _stream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(("127.0.0.1", ui_port)),
    )
    .await
    .context("timed out connecting to dashboard")??;

    let process_pid = dev_cmd
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;

    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    // Wait until the process stops
    let _ = tokio::time::timeout(DEV_EXIT_TIME, dev_cmd.write().await.wait())
        .await
        .context("dev command did not exit")?;

    wait_for_no_hosts()
        .await
        .context("wasmcloud instance failed to exit cleanly (processes still left over)")?;

    // Kill the nats instance
    nats.kill().await.map_err(|e| anyhow!(e))?;

    wait_for_no_nats()
        .await
        .context("nats instance failed to exit cleanly (processes still left over)")?;

    wait_for_no_wadm()
        .await
        .context("wadm instance failed to exit cleanly (processes still left over)")?;

    Ok(())
}

/// Ensure that overriding manifest YAML works
#[tokio::test]
#[serial_test::serial]
async fn integration_override_manifest_yaml_serial() -> Result<()> {
    // Force cleanup any lingering processes from previous tests
    force_cleanup_processes().await?;

    wait_for_no_hosts()
        .await
        .context("unexpected wasmcloud instance(s) running")?;

    let test_setup = init("hello", "hello-world-rust").await?;
    let project_dir = test_setup.project_dir.clone();
    let dir = tempfile::tempdir()?;

    wait_for_no_hosts()
        .await
        .context("one or more unexpected wasmcloud instances running")?;

    // Start NATS
    let nats_port = find_open_port().await?;
    let mut nats = start_nats(nats_port, &dir).await?;

    // Create a ctl client to check the cluster
    let ctl_client = CtlClientBuilder::new(
        async_nats::connect(format!("127.0.0.1:{nats_port}"))
            .await
            .context("failed to create nats client")?,
    )
    .lattice("default")
    .build();

    // Write out the fixture configuration to disk
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("./tests/fixtures/wadm/hello-world-rust-dev-override.yaml");
    tokio::fs::write(
        project_dir.join("test.wadm.yaml"),
        tokio::fs::read(&fixture_path)
            .await
            .with_context(|| format!("failed to read fixture @ [{}]", fixture_path.display()))?,
    )
    .await
    .context("failed to write out fixture file")?;

    // Manipulate the wasmcloud.toml for the test project and override the manifest
    let wasmcloud_toml_path = project_dir.join("wasmcloud.toml");
    let mut wasmcloud_toml = tokio::fs::File::options()
        .append(true)
        .open(&wasmcloud_toml_path)
        .await
        .with_context(|| {
            format!(
                "failed to open wasmcloud toml file @ [{}]",
                wasmcloud_toml_path.display()
            )
        })?;
    wasmcloud_toml
        .write_all(
            r#"
[dev]
manifests = [
  { component_name = "http-handler", path = "test.wadm.yaml" }
]
"#
            .as_bytes(),
        )
        .await
        .context("failed tow write dev configuration content to file")?;
    wasmcloud_toml.flush().await?;

    // Run wash dev
    let dev_cmd = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .args([
                "dev",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--nats-connect-only",
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed running wash dev")?,
    ));
    let watch_dev_cmd = dev_cmd.clone();

    // Get the host that was created
    let host = tokio::time::timeout(DEV_WAIT_TIME, async {
        loop {
            if let Some(h) = ctl_client
                .get_hosts()
                .await
                .map_err(|e| anyhow!("failed to get hosts: {e}"))
                .context("get components")?
                .into_iter()
                .map(|v| v.into_data())
                .next()
            {
                return Ok::<Option<Host>, anyhow::Error>(h);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("timed out waiting for host to start up")?
    .context("failed to get the host")?;
    let host_id = host
        .as_ref()
        .context("host was missing from request")?
        .id()
        .to_string();

    // Wait until the ferris-says component is present on the host
    let _ = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::spawn(async move {
            loop {
                // If the command failed (and exited early), bail
                if let Ok(Some(exit_status)) = watch_dev_cmd.write().await.try_wait() {
                    if !exit_status.success() {
                        bail!("dev command failed");
                    }
                }
                // If the file got built, we know dev succeeded
                let host_inventory = ctl_client
                    .get_host_inventory(&host_id)
                    .await
                    .map_err(|e| anyhow!(e))
                    .map(|v| v.into_data())
                    .context("failed to get host inventory");
                if host_inventory.is_ok_and(|inv| {
                    inv.is_some_and(|cs| {
                        cs.components()
                            .iter()
                            .any(|c| c.name() == Some("ferris-says"))
                    })
                }) {
                    break Ok(()) as anyhow::Result<()>;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }),
    )
    .await
    .context("timed out while waiting for file path to get created")?;

    let process_pid = dev_cmd
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;

    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    // Wait until the process stops
    let _ = tokio::time::timeout(DEV_EXIT_TIME, dev_cmd.write().await.wait())
        .await
        .context("dev command did not exit")?;

    wait_for_no_hosts()
        .await
        .context("wasmcloud instance failed to exit cleanly (processes still left over)")?;

    // Kill the nats instance
    nats.kill().await.map_err(|e| anyhow!(e))?;

    wait_for_no_nats()
        .await
        .context("nats instance failed to exit cleanly (processes still left over)")?;

    wait_for_no_wadm()
        .await
        .context("wadm instance failed to exit cleanly (processes still left over)")?;

    Ok(())
}

/// Ensure that overriding by interface via project config YAML works
#[tokio::test]
#[serial_test::serial]
async fn integration_override_via_interface_serial() -> Result<()> {
    // Force cleanup any lingering processes from previous tests
    force_cleanup_processes().await?;

    wait_for_no_hosts()
        .await
        .context("unexpected wasmcloud instance(s) running")?;

    let test_setup = init("hello", "hello-world-rust").await?;
    let project_dir = test_setup.project_dir.clone();
    let dir = tempfile::tempdir()?;

    // Create a dir for generated manifests
    let generated_manifests_dir = project_dir.join("generated-manifests");
    tokio::fs::create_dir(&generated_manifests_dir).await?;

    wait_for_no_hosts()
        .await
        .context("one or more unexpected wasmcloud instances running")?;

    // Start NATS
    let nats_port = find_open_port().await?;
    let mut nats = start_nats(nats_port, &dir).await?;

    // Create a ctl client to check the cluster
    let ctl_client = CtlClientBuilder::new(
        async_nats::connect(format!("127.0.0.1:{nats_port}"))
            .await
            .context("failed to create nats client")?,
    )
    .lattice("default")
    .build();

    // Write out the fixture configuration to disk
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("./tests/fixtures/wadm/hello-world-rust-dev-override.yaml");
    tokio::fs::write(
        project_dir.join("test.wadm.yaml"),
        tokio::fs::read(&fixture_path)
            .await
            .with_context(|| format!("failed to read fixture @ [{}]", fixture_path.display()))?,
    )
    .await
    .context("failed to write out fixture file")?;

    // Manipulate the wasmcloud.toml for the test project and override the manifest
    let wasmcloud_toml_path = project_dir.join("wasmcloud.toml");
    let mut wasmcloud_toml = tokio::fs::File::options()
        .append(true)
        .open(&wasmcloud_toml_path)
        .await
        .with_context(|| {
            format!(
                "failed to open wasmcloud toml file @ [{}]",
                wasmcloud_toml_path.display()
            )
        })?;
    wasmcloud_toml
        .write_all(
            r#"
[[dev.overrides.imports]]
interface =  "wasi:http/incoming-handler@0.2.0"
config = { name = "value" }
link_config = { values = { address = "127.0.0.1:8083" } }
secrets = { name = "existing-secret", source = { policy = "nats-kv", key = "test" } }
image_ref = "ghcr.io/wasmcloud/http-server:0.23.0" # intentionally slightly older!
link_name = "default"
"#
            .as_bytes(),
        )
        .await
        .context("failed to write dev configuration content to file")?;
    wasmcloud_toml.flush().await?;

    // Run wash dev
    let dev_cmd = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .args([
                "dev",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--nats-connect-only",
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
                "--manifest-output-dir",
                &format!("{}", generated_manifests_dir.display()),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed running wash dev")?,
    ));
    let watch_dev_cmd = dev_cmd.clone();

    // Get the host that was created
    let host = tokio::time::timeout(DEV_WAIT_TIME, async {
        loop {
            if let Some(h) = ctl_client
                .get_hosts()
                .await
                .map_err(|e| anyhow!("failed to get hosts: {e}"))
                .context("getting hosts failed")?
                .into_iter()
                .map(|v| v.into_data())
                .next()
            {
                return Ok::<Option<Host>, anyhow::Error>(h);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("timed out waiting for host to start up")?
    .context("failed to get the host")?;
    let host_id = host
        .as_ref()
        .context("host was missing from request")?
        .id()
        .to_string();

    // Wait until the http-hello-world component is present on the host
    let _ = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::spawn(async move {
            loop {
                // If the command failed (and exited early), bail
                if let Ok(Some(exit_status)) = watch_dev_cmd.write().await.try_wait() {
                    if !exit_status.success() {
                        bail!("dev command failed");
                    }
                }
                // If the file got built, we know dev succeeded
                let host_inventory = ctl_client
                    .get_host_inventory(&host_id)
                    .await
                    .map_err(|e| anyhow!(e))
                    .map(|v| v.into_data())
                    .context("failed to get host inventory");
                if host_inventory.is_ok_and(|inv| {
                    inv.is_some_and(|cs| {
                        cs.components()
                            .iter()
                            .any(|c| c.name() == Some("http-hello-world"))
                    })
                }) {
                    break Ok(()) as anyhow::Result<()>;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }),
    )
    .await
    .context("timed out while waiting for file path to get created")?;

    // Find the generated manifest (there should only be one
    let generated_manifest = {
        let mut dir_entries = tokio::fs::read_dir(generated_manifests_dir).await?;
        loop {
            let entry = dir_entries
                .next_entry()
                .await
                .context("failed to get dir entry")?
                .context("no more dir entries")?;
            if entry.path().extension().is_some_and(|v| v == "yaml") {
                break serde_yaml::from_slice::<Manifest>(&tokio::fs::read(entry.path()).await?)
                    .context("failed to parse manifest YAML")?;
            }
        }
    };

    // Find the HTTP provider component w/ the overridden image ref
    let _provider_component = generated_manifest
        .components()
        .find(|c| {
            matches!(
                c.properties,
                Properties::Capability { ref properties } if properties.image.as_ref().is_some_and(|i| i == "ghcr.io/wasmcloud/http-server:0.23.0"))
        })
        .context("missing http provider component in manifest w/ updated image_ref")?;

    // Ensure that the overridden address is present
    assert!(generated_manifest.links().any(|l| {
        matches!(&l.properties,
            TraitProperty::Link(LinkProperty {
                namespace, package, interfaces, source, ..
            }) if
                 namespace == "wasi"
                 && package == "http"
                 && interfaces.contains(&"incoming-handler".to_string())
                 && source.as_ref().is_some_and(|s| {
                     s.config.iter().any(|c| {
                         c.properties.as_ref().is_some_and(|ps| {
                             ps.get("address").is_some_and(|v| v == "127.0.0.1:8083")
                         })
                     })
                 })
        )
    }));

    let process_pid = dev_cmd
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;

    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    // Wait until the process stops
    let _ = tokio::time::timeout(DEV_EXIT_TIME, dev_cmd.write().await.wait())
        .await
        .context("dev command did not exit")?;

    wait_for_no_hosts()
        .await
        .context("wasmcloud instance failed to exit cleanly (processes still left over)")?;

    // Kill the nats instance
    nats.kill().await.map_err(|e| anyhow!(e))?;

    wait_for_no_nats()
        .await
        .context("nats instance failed to exit cleanly (processes still left over)")?;

    wait_for_no_wadm()
        .await
        .context("wadm instance failed to exit cleanly (processes still left over)")?;

    Ok(())
}

/// Ensure that overriding multiple interfaces works as expected. The common use case
/// for this is overriding like `wasi:keyvalue@0.2.0` will override all necessary dependencies
/// like `wasi:keyvalue/atomics` and `wasi:keyvalue/store`.
#[tokio::test]
#[serial_test::serial]
async fn integration_override_multiple_interfaces() -> Result<()> {
    // Force cleanup any lingering processes from previous tests
    force_cleanup_processes().await?;

    wait_for_no_hosts()
        .await
        .context("unexpected wasmcloud instance(s) running")?;

    // KV counter
    let test_setup = init_path("hello", "examples/rust/components/http-keyvalue-counter").await?;
    let project_dir = test_setup.project_dir.clone();
    let dir = tempfile::tempdir()?;

    // Create a dir for generated manifests
    let generated_manifests_dir = project_dir.join("generated-manifests");
    tokio::fs::create_dir(&generated_manifests_dir).await?;

    wait_for_no_hosts()
        .await
        .context("one or more unexpected wasmcloud instances running")?;

    // Start NATS
    let nats_port = find_open_port().await?;
    let mut nats = start_nats(nats_port, &dir).await?;

    // Create a ctl client to check the cluster
    let ctl_client = CtlClientBuilder::new(
        async_nats::connect(format!("127.0.0.1:{nats_port}"))
            .await
            .context("failed to create nats client")?,
    )
    .lattice("default")
    .build();

    // Write out the fixture configuration to disk
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("./tests/fixtures/wadm/hello-world-rust-dev-override.yaml");
    tokio::fs::write(
        project_dir.join("test.wadm.yaml"),
        tokio::fs::read(&fixture_path)
            .await
            .with_context(|| format!("failed to read fixture @ [{}]", fixture_path.display()))?,
    )
    .await
    .context("failed to write out fixture file")?;

    // Manipulate the wasmcloud.toml for the test project and override the manifest
    let wasmcloud_toml_path = project_dir.join("wasmcloud.toml");
    let mut wasmcloud_toml = tokio::fs::File::options()
        .append(true)
        .open(&wasmcloud_toml_path)
        .await
        .with_context(|| {
            format!(
                "failed to open wasmcloud toml file @ [{}]",
                wasmcloud_toml_path.display()
            )
        })?;
    wasmcloud_toml
        .write_all(
            r#"
[[dev.overrides.imports]]
interface =  "wasi:keyvalue@0.2.0"
config = { name = "value" }
image_ref = "ghcr.io/wasmcloud/keyvalue-redis:0.28.2" # intentionally slightly older!
link_name = "default"
"#
            .as_bytes(),
        )
        .await
        .context("failed to write dev configuration content to file")?;
    wasmcloud_toml.flush().await?;

    // Run wash dev
    let dev_cmd = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .args([
                "dev",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--nats-connect-only",
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
                "--manifest-output-dir",
                &format!("{}", generated_manifests_dir.display()),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed running wash dev")?,
    ));
    let watch_dev_cmd = dev_cmd.clone();

    // Get the host that was created
    let host = tokio::time::timeout(DEV_WAIT_TIME, async {
        loop {
            if let Some(h) = ctl_client
                .get_hosts()
                .await
                .map_err(|e| anyhow!("failed to get hosts: {e}"))
                .context("getting hosts failed")?
                .into_iter()
                .map(|v| v.into_data())
                .next()
            {
                return Ok::<Option<Host>, anyhow::Error>(h);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("timed out waiting for host to start up")?
    .context("failed to get the host")?;
    let host_id = host
        .as_ref()
        .context("host was missing from request")?
        .id()
        .to_string();

    // Wait until the http-hello-world component is present on the host
    let _ = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::spawn(async move {
            loop {
                // If the command failed (and exited early), bail
                if let Ok(Some(exit_status)) = watch_dev_cmd.write().await.try_wait() {
                    if !exit_status.success() {
                        bail!("dev command failed");
                    }
                }
                // If the file got built, we know dev succeeded
                let host_inventory = ctl_client
                    .get_host_inventory(&host_id)
                    .await
                    .map_err(|e| anyhow!(e))
                    .map(|v| v.into_data())
                    .context("failed to get host inventory");
                if host_inventory.is_ok_and(|inv| {
                    inv.is_some_and(|cs| {
                        cs.components()
                            .iter()
                            .any(|c| c.name() == Some("http-hello-world"))
                    })
                }) {
                    break Ok(()) as anyhow::Result<()>;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }),
    )
    .await
    .context("timed out while waiting for file path to get created")?;

    // Find the generated manifest (there should only be one
    let generated_manifest = {
        let mut dir_entries = tokio::fs::read_dir(generated_manifests_dir).await?;
        loop {
            let entry = dir_entries
                .next_entry()
                .await
                .context("failed to get dir entry")?
                .context("no more dir entries")?;
            if entry.path().extension().is_some_and(|v| v == "yaml") {
                break serde_yaml::from_slice::<Manifest>(&tokio::fs::read(entry.path()).await?)
                    .context("failed to parse manifest YAML")?;
            }
        }
    };

    // Find the HTTP provider component w/ the overridden image ref
    let provider_component = generated_manifest
        .components()
        .find(|c| {
            matches!(
                c.properties,
                Properties::Capability { ref properties } if properties.image.as_ref().is_some_and(|i| i == "ghcr.io/wasmcloud/keyvalue-redis:0.28.2"))
        })
        .context("missing keyvalue provider component in manifest w/ updated image_ref")?;

    // Link from HTTP -> component, component -> keyvalue. Notably, only one link
    // for atomics and store.
    let links_count = generated_manifest.links().count();
    if links_count != 2 {
        bail!("Expected 2 links, but got {links_count}");
    }

    let override_interfaces_link_exists = generated_manifest.links().any(|l| match &l.properties {
        TraitProperty::Link(LinkProperty {
            interfaces, target, ..
        }) => {
            interfaces.contains(&"atomics".to_string())
                && interfaces.contains(&"store".to_string())
                && target.name == provider_component.name
        }
        _ => false,
    });

    if !override_interfaces_link_exists {
        bail!("Link with atomics and store interfaces to provider component not found");
    }

    let process_pid = dev_cmd
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;

    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    // Wait until the process stops
    let _ = tokio::time::timeout(DEV_EXIT_TIME, dev_cmd.write().await.wait())
        .await
        .context("dev command did not exit")?;

    wait_for_no_hosts()
        .await
        .context("wasmcloud instance failed to exit cleanly (processes still left over)")?;

    // Kill the nats instance
    nats.kill().await.map_err(|e| anyhow!(e))?;

    wait_for_no_nats()
        .await
        .context("nats instance failed to exit cleanly (processes still left over)")?;

    wait_for_no_wadm()
        .await
        .context("wadm instance failed to exit cleanly (processes still left over)")?;

    Ok(())
}

// NOTE(thomastaylor312): So this test and integration_dev_running_multiple_hosts_tests are both
// terribly borked and almost always fail in CI. These are fairly brittle and do pass locally, but in
// CI they constantly have issues because processes stick around during failures and other such
// stuff. It might be easier to re-write these in bash or make them less dependent on process
// counts. For now they are ignored
#[tokio::test]
#[serial_test::serial]
#[ignore]
/// This test ensures that dev works when there is already a running host by
/// connecting to it and then starting a dev loop.
async fn integration_dev_running_host_tests() -> Result<()> {
    // Force cleanup any lingering processes from previous tests
    force_cleanup_processes().await?;

    wait_for_no_hosts()
        .await
        .context("unexpected wasmcloud instance(s) running")?;
    let test_setup = init(
        /* component_name= */ "hello",
        /* template_name= */ "hello-world-rust",
    )
    .await?;
    let project_dir = test_setup.project_dir.clone();

    let dir = tempfile::tempdir()?;

    wait_for_no_hosts()
        .await
        .context("one or more unexpected wasmcloud instances running")?;

    let nats_port = find_open_port().await?;
    let mut nats = start_nats(nats_port, &dir).await?;

    // Start a wasmCloud host
    let up_cmd = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .args([
                "up",
                "--nats-connect-only",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed running wash up")?,
    ));

    // Wait for the first host to come up so they don't clobber each other when downloading things
    // Wait until the first host is up to avoid them competing with each other and trying to
    // download twice
    wait_for_num_hosts(1)
        .await
        .context("did not get host running")?;

    // Start a dev loop, which should just work and use the existing host
    let dev_cmd = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .args([
                "dev",
                "--nats-connect-only",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed running wash dev")?,
    ));
    let watch_dev_cmd = dev_cmd.clone();

    let signed_file_path = Arc::new(project_dir.join("build/http_hello_world_s.wasm"));
    let expected_path = signed_file_path.clone();

    // Wait until the signed file is there (this means dev succeeded)
    let _ = tokio::time::timeout(
        DEV_WAIT_TIME,
        tokio::spawn(async move {
            loop {
                // If the command failed (and exited early), bail
                if let Ok(Some(exit_status)) = watch_dev_cmd.write().await.try_wait() {
                    if !exit_status.success() {
                        bail!("dev command failed");
                    }
                }
                // If the file got built, we know dev succeeded
                if expected_path.exists() {
                    break Ok(());
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }),
    )
    .await
    .context("timed out while waiting for file path to get created")?;

    if !signed_file_path.exists() {
        bail!("signed component file was not built");
    }

    let process_pid = dev_cmd
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;

    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    // Wait until the process stops
    let _ = tokio::time::timeout(DEV_EXIT_TIME, dev_cmd.write().await.wait())
        .await
        .context("dev command did not exit")?;

    // Kill the originally launched host
    let process_pid = up_cmd
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;

    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    wait_for_no_hosts()
        .await
        .context("wasmcloud instance failed to exit cleanly (processes still left over)")?;

    // Kill the nats instance
    nats.kill().await.map_err(|e| anyhow!(e))?;

    wait_for_no_nats()
        .await
        .context("nats instance failed to exit cleanly (processes still left over)")?;

    wait_for_no_wadm()
        .await
        .context("wadm instance failed to exit cleanly (processes still left over)")?;

    Ok(())
}

#[tokio::test]
#[serial_test::serial]
#[ignore]
/// This test ensures that dev does not start and exits cleanly when multiple hosts are
/// available and the host ID is not specified. Then, ensures dev does start when
/// the host ID is specified.
async fn integration_dev_running_multiple_hosts_tests() -> Result<()> {
    // Force cleanup any lingering processes from previous tests
    force_cleanup_processes().await?;

    wait_for_no_hosts()
        .await
        .context("unexpected wasmcloud instance(s) running")?;
    let test_setup = init(
        /* component_name= */ "hello",
        /* template_name= */ "hello-world-rust",
    )
    .await?;
    let project_dir = test_setup.project_dir.clone();

    let dir = tempfile::tempdir()?;

    wait_for_no_hosts()
        .await
        .context("one or more unexpected wasmcloud instances running")?;

    let nats_port = find_open_port().await?;
    let mut nats = start_nats(nats_port, &dir).await?;

    // Start a wasmCloud host
    let host_id = KeyPair::new_server();
    let up_cmd = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .stdin(Stdio::null())
            .args([
                "up",
                "--nats-connect-only",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
                "--host-seed",
                host_id.seed().context("failed to get host seed")?.as_str(),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed running wash up")?,
    ));

    // Wait until the first host is up to avoid them competing with each other and trying to
    // download twice
    wait_for_num_hosts(1)
        .await
        .context("did not get first host running")?;

    // Start a second wasmCloud host
    let up_cmd2 = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .stdin(Stdio::null())
            .args([
                "up",
                "--nats-connect-only",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
                "--multi-local",
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed running wash up")?,
    ));

    // Ensure two hosts are running
    wait_for_num_hosts(2)
        .await
        .context("did not get 2 hosts running")?;

    // Start a dev loop, which will not work with more than one host
    let bad_dev_cmd_multiple_hosts =
        // We're going to wait for the output, which should happen right after
        // querying for the host list.
        tokio::time::timeout(
            Duration::from_secs(10),
            test_setup
                .base_command()
                .args([
                    "dev",
                    "--nats-connect-only",
                    "--nats-port",
                    nats_port.to_string().as_ref(),
                    "--ctl-port",
                    nats_port.to_string().as_ref(),
                    "--rpc-port",
                    nats_port.to_string().as_ref(),
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("dev loop did not exit in expected time")?
        .context("dev loop failed to exit cleanly")?;

    if bad_dev_cmd_multiple_hosts.status.success() {
        bail!("Expected dev command to fail with multiple hosts, but it succeeded");
    }
    if !bad_dev_cmd_multiple_hosts.stdout.is_empty() {
        bail!("Expected empty stdout for failed dev command, but got output");
    }
    if !String::from_utf8_lossy(&bad_dev_cmd_multiple_hosts.stderr)
        .contains("found multiple running hosts")
    {
        bail!("Expected error message about multiple hosts, but got different error");
    }

    // Start a dev loop, which will fail to find the desired host
    let bad_dev_cmd_multiple_hosts =
        // We're going to wait for the output, which should happen right after
        // querying for the host list.
        tokio::time::timeout(
            Duration::from_secs(10),
            test_setup
                .base_command()
                .args([
                    "dev",
                    "--nats-connect-only",
                    "--nats-port",
                    nats_port.to_string().as_ref(),
                    "--ctl-port",
                    nats_port.to_string().as_ref(),
                    "--rpc-port",
                    nats_port.to_string().as_ref(),
                    "--host-id",
                    // Real host ID, just not one of the ones we started
                    "NAAX34C3KIELJQJRZBAJSRJ6S3Q5NGAGMAAFITB64F4L5L4LQC6XVAZK"
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("dev loop did not exit in expected time")?
        .context("dev loop failed to exit cleanly")?;

    if bad_dev_cmd_multiple_hosts.status.success() {
        bail!("Expected dev command to fail with invalid host ID, but it succeeded");
    }
    if !bad_dev_cmd_multiple_hosts.stdout.is_empty() {
        bail!("Expected empty stdout for failed dev command, but got output");
    }
    if !String::from_utf8_lossy(&bad_dev_cmd_multiple_hosts.stderr)
        .contains("not found in running hosts")
    {
        bail!("Expected error message about host not found, but got different error");
    }

    let dev_cmd = Arc::new(RwLock::new(
        test_setup
            .base_command()
            .stdin(Stdio::null())
            .args([
                "dev",
                "--nats-connect-only",
                "--nats-port",
                nats_port.to_string().as_ref(),
                "--ctl-port",
                nats_port.to_string().as_ref(),
                "--rpc-port",
                nats_port.to_string().as_ref(),
                "--host-id",
                host_id.public_key().as_str(),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("dev loop did not start successfully with multiple hosts")?,
    ));

    let watch_dev_cmd = dev_cmd.clone();

    let signed_file_path = Arc::new(project_dir.join("build/http_hello_world_s.wasm"));
    let expected_path = signed_file_path.clone();

    // Wait until the signed file is there (this means dev succeeded)
    let _ = tokio::time::timeout(
        DEV_WAIT_TIME,
        tokio::spawn(async move {
            loop {
                // If the command failed (and exited early), bail
                if let Ok(Some(exit_status)) = watch_dev_cmd.write().await.try_wait() {
                    if !exit_status.success() {
                        bail!("dev command failed");
                    }
                }
                // If the file got built, we know dev succeeded
                if expected_path.exists() {
                    break Ok(());
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }),
    )
    .await
    .context("timed out while waiting for file path to get created")?;
    if !signed_file_path.exists() {
        bail!("signed component file was not built");
    }

    let process_pid = dev_cmd
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;

    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    // Wait until the process stops
    let _ = tokio::time::timeout(DEV_EXIT_TIME, dev_cmd.write().await.wait())
        .await
        .context("dev command did not exit")?;

    // Kill the originally launched host
    let process_pid = up_cmd
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;
    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    // Kill the second host
    let process_pid = up_cmd2
        .write()
        .await
        .id()
        .context("failed to get child process pid")?;
    // Send ctrl + c signal to stop the process
    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(process_pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .context("cannot send ctrl-c")?;

    wait_for_no_hosts()
        .await
        .context("wasmcloud instance failed to exit cleanly (processes still left over)")?;

    // Kill the nats instance
    nats.kill().await.map_err(|e| anyhow!(e))?;

    wait_for_no_nats()
        .await
        .context("nats instance failed to exit cleanly (processes still left over)")?;

    wait_for_no_wadm()
        .await
        .context("wadm instance failed to exit cleanly (processes still left over)")?;

    Ok(())
}

/// ### DESCRIPTION
/// Verifies that `wash dev`` does not panic with a broken pipe error in case it is piped
/// to another process; e.g. 'wash dev -o json | wc -l', and the user has entered CTRL+C.
/// Please note this is a regression test for issue [#3639](https://github.com/wasmCloud/wasmCloud/issues/3639).
///
/// #### BACKGROUND
/// When the user enters CTRL+C both processes will receive a SIGINT signal and should shutdown as soon
/// as possible. In many cases the 2nd process will be the first to process the SIGINT signal, especially
/// when it's a fairly simple process like `wc -l`. As a result the 2nd process will exit thus blocking
/// any further writes to the stdout of the first process(`wash dev`).
///
/// ### EXPECTED RESULT
/// Both processes should exit cleanly, the 2nd should report SIGINT(2) as exit reason and the first
/// process should exit with code 0 without any broken pipe errors.
///
#[tokio::test]
#[serial_test::serial]
#[cfg(target_family = "unix")]
async fn integration_dev_hello_component_piped_stdout() -> Result<()> {
    // Force cleanup any lingering processes from previous tests
    force_cleanup_processes().await?;

    // ========================================================================
    // Preamble
    // ========================================================================
    // Create the test component

    use tokio::io::AsyncBufReadExt as _;
    let test_setup = init("hello", "hello-world-rust").await?;
    let project_dir = test_setup.project_dir.clone();

    // Build the test component
    let mut proc = test_setup
        .base_command()
        .arg("build")
        .current_dir(project_dir.clone())
        .spawn()
        .context("failed to spawn proc(`wash build`)")?;
    let status: ExitStatus = proc.wait().await?;
    if !(status.code() == Some(0) && status.success()) {
        bail!("unexpected exit status for proc(`wash build`); {status:?}");
    }

    // Start a NATS server
    let port = find_open_port().await?;
    let mut nats = start_nats(port, &project_dir).await?;
    let nats_port = port.to_string();

    // ========================================================================
    // Test setup
    // ========================================================================
    // Create the 'wash dev' process using a piped stdout
    #[allow(clippy::zombie_processes)]
    let mut proc1 = test_setup
        .base_command()
        .env("RUST_BACKTRACE", "full")
        .args([
            "dev",
            "--nats-connect-only",
            "--nats-port",
            &nats_port,
            "--ctl-port",
            &nats_port,
            "--rpc-port",
            &nats_port,
            "-o",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(project_dir.clone())
        .spawn()
        .context("failed to spawn proc(`wash dev`)")?;
    let pid1 = proc1
        .id()
        .context("failed to get pid of proc(`wash dev`)")?;

    // Create the 'wc -l' process and use the piped stdout of wash dev as stdin
    #[allow(clippy::zombie_processes)]
    let mut proc2 = tokio::process::Command::new("wc")
        .arg("-l")
        .stdin(<tokio::process::ChildStdout as TryInto<Stdio>>::try_into(
            proc1
                .stdout
                .take()
                .context("failed to take stdout of proc(`wash dev`) as stdin for proc(`wc -l`)")?,
        )?)
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn piped proc(`wc -l`)")?;
    let pid2 = proc2
        .id()
        .context("failed to get pid of piped proc(`wc -l`)")?;

    // Wait for the first process('wash dev') to be started and is waiting for CTRL+C
    let stderr1_pattern = "press Ctrl+c to stop";
    let mut stderr1_out = String::new();
    let mut stderr1_reader = tokio::io::BufReader::new(
        proc1
            .stderr
            .take()
            .context("failed to take stderr of proc(`wash dev`)")?,
    );
    let mut stderr1_line_count = 0;
    let mut stderr = std::io::stderr(); // used to echo output of proc(`wash dev`) to stderr
    loop {
        let mut line = String::new();

        match stderr1_reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                write!(&mut stderr, "{line}")?;
                stderr1_out.push_str(&line);
                if line.contains(stderr1_pattern) {
                    break;
                }
            }
            Err(_) => break,
        }

        stderr1_line_count += 1;
        if stderr1_line_count >= 20 {
            bail!("failed to process stderr of proc(`wash dev`)");
        }
    }
    if !stderr1_out.contains(stderr1_pattern) {
        bail!(
            "Expected stderr to contain '{}', but it didn't",
            stderr1_pattern
        );
    }

    // Send SIGINT to second process; this will be trigger the
    // stdout of the first process to be closed
    {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid2 as i32),
            nix::sys::signal::Signal::SIGINT,
        )
        .context("cannot send ctrl-c to piped proc(`wc -l`)")?;
        proc2.wait().await?;
    }

    // Give the first process some time to do its job/damage
    thread::sleep(Duration::from_millis(500));

    // Send SIGINT to first process; unbuffered writes to stdout will result in a broken pipe
    {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid1 as i32),
            nix::sys::signal::Signal::SIGINT,
        )
        .context("cannot send ctrl-c to proc(`wash dev`)")?;
        proc1.wait().await?;
    }

    // Wait for the processes to complete
    let status1: ExitStatus = proc1
        .wait()
        .await
        .context("failed to wait for proc(`wash dev`), pid({})")?;
    let status2: ExitStatus = proc2
        .wait()
        .await
        .context("failed to wait for piped proc(`wc -l`), pid({})")?;

    // Echo the remaining stderr output of the first process
    loop {
        let mut line = String::new();

        match stderr1_reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => write!(&mut stderr, "{line}")?,
            Err(_) => break,
        }
    }

    // ========================================================================
    // Postamble
    // ========================================================================
    // Stop the NATS server
    nats.kill().await.map_err(|e| anyhow!(e))?;

    // Remove test component
    drop(test_setup.project_dir);
    test_setup.test_dir.close()?;

    // ========================================================================
    // Verdict
    // ========================================================================
    // The exit status of proc('wc -l') should be SIGINT(2)
    if !(status2.signal() == Some(2) && !status2.success() && status2.code().is_none()) {
        bail!("unexpected exit status for piped proc(`wc -l`), pid({pid2}); {status2:?}");
    }

    // The exit status of proc('wash dev') should be code 0
    if !(status1.signal().is_none() && status1.success() && status1.code() == Some(0)) {
        bail!("unexpected exit status for proc(`wash dev`), pid({pid1}); {status1:?}",);
    }

    Ok(())
}
