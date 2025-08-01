use std::net::TcpListener;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::process::Stdio;
use std::{
    env,
    fs::{create_dir_all, remove_dir_all},
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use oci_client::Reference;
use rand::{distr::Alphanumeric, Rng};
use tempfile::TempDir;
use tokio::{
    fs::File,
    io::AsyncWriteExt,
    process::{Child, Command},
    time::Duration,
};

use wash::cli::config::{WADM_VERSION, WASMCLOUD_HOST_VERSION};
use wash::lib::cli::output::{
    AppDeleteCommandOutput, AppDeployCommandOutput, AppGetCommandOutput, AppListCommandOutput,
    AppUndeployCommandOutput, CallCommandOutput, CreateContextCommandOutput,
    DeleteContextCommandOutput, GetHostsCommandOutput, PullCommandOutput, StartCommandOutput,
    StopCommandOutput, UpCommandOutput,
};
use wash::lib::common::CommandGroupUsage;
use wash::lib::config::{host_pid_file, wadm_pid_file};
use wash::lib::start::{
    ensure_nats_server, start_nats_server_with_timeout, NatsConfig, WADM_BINARY, WASMCLOUD_HOST_BIN,
};
use wasmcloud_control_interface::Host;

#[allow(unused)]
pub const LOCAL_REGISTRY: &str = "localhost:5001";

#[allow(unused)]
pub const HELLO_OCI_REF: &str = "ghcr.io/brooksmtownsend/http-hello-world-rust:0.1.1";

#[allow(unused)]
pub const HTTP_JSONIFY_OCI_REF: &str = "ghcr.io/wasmcloud/components/http-jsonify-rust:0.1.2";

#[allow(unused)]
pub const PROVIDER_HTTPSERVER_OCI_REF: &str = "ghcr.io/wasmcloud/http-server:0.23.2";

#[allow(unused)]
pub const FERRIS_SAYS_OCI_REF: &str = "ghcr.io/wasmcloud/components/ferris-says-rust:0.1.0";

pub const DEFAULT_WASH_INVOCATION_TIMEOUT_MS_ARG: &str = "40000";

#[allow(unused)]
const WKG_CONFIG_FILE: &str = "wkg_config.toml";

/// Helper function to create the `wash` binary process
#[allow(unused)]
pub fn wash() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_wash"))
}

#[allow(unused)]
pub fn output_to_string(output: std::process::Output) -> Result<String> {
    String::from_utf8(output.stdout).with_context(|| "Failed to convert output bytes to String")
}

#[allow(unused)]
pub async fn fetch_artifact_digest(url: &str) -> Result<String> {
    let image: Reference = url.to_lowercase().parse()?;

    let mut protocol = "http";
    if url.starts_with("https://") {
        protocol = "https"
    }

    let reference = image
        .tag()
        .or(image.digest())
        .context("Could not find a valid tag or digest in the provided artifact URL")?;

    let manifest_url = format!(
        "{}://{}/v2/{}/manifests/{}",
        protocol,
        image.registry(),
        image.repository(),
        reference
    );

    let accept_manifest_media_types = [
        oci_client::manifest::IMAGE_MANIFEST_MEDIA_TYPE,
        oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
    ]
    .join(",");

    let client = reqwest::Client::new();
    let resp = client
        .get(manifest_url)
        .header(reqwest::header::ACCEPT, accept_manifest_media_types)
        .send()
        .await
        .context("Unable to query the provided artifact URL")?;

    let header = resp
        .headers()
        .get("Docker-Content-Digest")
        .context("Could not find Docker-Content-Digest header for provided artifact URL")?;

    let digest = header
        .to_str()
        .context("Unable to convert Docker-Content-Digest header to value")?;

    Ok(digest.to_owned())
}

#[allow(unused)]
pub fn get_json_output(output: std::process::Output) -> Result<serde_json::Value> {
    let output_str = output_to_string(output)?;

    let json: serde_json::Value = serde_json::from_str(&output_str)
        .with_context(|| "Failed to parse json from output string")?;

    Ok(json)
}

#[allow(unused)]
/// Creates a subfolder in the test directory for use with a specific test
/// It's preferred that the same test that calls this function also
/// uses `std::fs::remove_dir_all` to remove the subdirectory
pub fn test_dir_with_subfolder(subfolder: &str) -> PathBuf {
    let root_dir = &env::var("CARGO_MANIFEST_DIR").expect("$CARGO_MANIFEST_DIR");
    let with_subfolder = PathBuf::from(format!("{root_dir}/tests/output/{subfolder}"));
    remove_dir_all(with_subfolder.clone());
    create_dir_all(with_subfolder.clone());
    with_subfolder
}

#[allow(unused)]
/// Returns a `PathBuf` by appending the subfolder and file arguments
/// to the test fixtures directory. This does _not_ create the file,
/// so the test is responsible for initialization and modification of this file
pub fn test_dir_file(subfolder: &str, file: &str) -> PathBuf {
    let root_dir = &env::var("CARGO_MANIFEST_DIR").expect("$CARGO_MANIFEST_DIR");
    PathBuf::from(format!("{root_dir}/tests/output/{subfolder}/{file}"))
}

#[allow(unused)]
/// writes content to specified file path... creates file if it doesn't exist
pub async fn set_test_file_content(path: &PathBuf, content: &str) -> Result<()> {
    let mut file = File::create(path).await.context(format!(
        "failed to open/create test file {}",
        path.to_string_lossy()
    ))?;

    file.write_all(content.as_bytes()).await.context(format!(
        "failed to write content to test file {}",
        path.to_string_lossy()
    ))?;
    Ok(())
}

#[allow(unused)]
pub async fn start_nats(port: u16, nats_install_dir: impl AsRef<Path>) -> Result<Child> {
    let nats_binary =
        ensure_nats_server(wash::lib::common::NATS_SERVER_VERSION, &nats_install_dir).await?;
    let store_dir = nats_install_dir.as_ref().join("store");
    tokio::fs::create_dir_all(&store_dir).await?;
    let mut config = NatsConfig::new_standalone("127.0.0.1", port, None);
    config.store_dir = store_dir;
    start_nats_server_with_timeout(
        nats_binary,
        std::process::Stdio::null(),
        config,
        CommandGroupUsage::UseParent,
        // We give this a bit of time to start to account for download times, especially in CI
        std::time::Duration::from_secs(120),
    )
    .await
}

/// Returns an open port on the interface, searching within the range endpoints, inclusive
pub async fn find_open_port() -> Result<u16> {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .context("failed to bind random port")?
        .local_addr()
        .map(|addr| addr.port())
        .context("failed to get local address from opened TCP socket")
}

#[allow(unused)]
pub struct TestWashInstance {
    /// ID of the host
    pub host_id: String,
    /// Port on which NATS is running (normally randomized)
    pub nats_port: u16,
    /// Command that can be executed to kill the server (returned @ server startup)
    pub kill_cmd: String,
    /// Host seed generated when starting the host
    pub host_seed: String,
    /// Cluster seed generated when starting the host
    pub cluster_seed: String,
    /// Deployed WADM manifest path (if there was one specified during `wash up`)
    pub deployed_wadm_manifest_path: Option<String>,
    /// Temporary directory used for the test as a home dir
    pub test_dir: TempDir,
    /// NATS server child process
    nats: Child,
}

impl Drop for TestWashInstance {
    fn drop(&mut self) {
        let TestWashInstance {
            test_dir, kill_cmd, ..
        } = self;

        // Attempt to stop the host (this may fail)
        let kill_cmd = (*kill_cmd).to_string();
        let (_wash, down) = kill_cmd.trim_matches('"').split_once(' ').unwrap();
        wash()
            .env("HOME", test_dir.path())
            .args(vec![
                down,
                "--host-id",
                &self.host_id,
                "--ctl-port",
                &self.nats_port.to_string(),
            ])
            .output()
            .expect("Could not spawn wash down process");

        // Attempt to stop NATS
        self.nats
            .start_kill()
            .expect("failed to start_kill() on nats instance");
    }
}

/// Arguments for creating a new `TestWashInstance`
#[derive(Debug, PartialEq, Eq, Default)]
struct TestWashInstanceNewArgs {
    /// Extra arguments to feed to `wash up`
    pub extra_args: Vec<String>,
}

#[allow(unused)]
impl TestWashInstance {
    /// Create a new [`TestWashInstance`]
    pub async fn create() -> Result<TestWashInstance> {
        Self::new(TestWashInstanceNewArgs::default()).await
    }

    /// Create a new [`TestWashInstance`], with extra arguments to `wash up`
    pub async fn create_with_extra_args(
        args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self> {
        let mut extra_args = args
            .into_iter()
            .map(|v| v.as_ref().to_string())
            .collect::<Vec<String>>();
        // we pin the optional wadm and wascloudhost versions
        // to the current release, to avoid undefined behaviour in tests
        if !extra_args.contains(&"--wadm-version".to_string()) {
            extra_args.push("--wadm-version".to_string());
            extra_args.push(WADM_VERSION.to_string());
        }
        if !extra_args.contains(&"--wasmcloud-version".to_string()) {
            extra_args.push("--wasmcloud-version".to_string());
            extra_args.push(WASMCLOUD_HOST_VERSION.to_string());
        }
        Self::new(TestWashInstanceNewArgs { extra_args }).await
    }

    async fn new(args: TestWashInstanceNewArgs) -> Result<Self> {
        let test_id: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(6)
            .map(char::from)
            .collect();
        let test_name = format!("test-{test_id}");
        let test_dir = tempfile::tempdir()?;
        let nats_path = test_dir.path().join("nats");
        tokio::fs::create_dir_all(&nats_path).await?;

        let nats_port = find_open_port().await?;
        let nats = start_nats(nats_port, &nats_path).await?;

        // Create pre-determined seeds
        let cluster_seed = nkeys::KeyPair::new_cluster();
        let cluster_seed_str = &cluster_seed
            .seed()
            .context("failed to generate cluster seed")?;

        // Create a pre-determined keypair for the host to use
        let host_seed = nkeys::KeyPair::new_server();
        let host_seed_str = &host_seed.seed().context("failed to generate host seed")?;
        let host_id = host_seed.public_key();

        // Start building the `wash up` command
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_wash"));

        // Compile list of arguments to `wash up`
        let mut cmd_args = [
            "up",
            "--nats-port",
            nats_port.to_string().as_ref(),
            "--nats-connect-only",
            "--output",
            "json",
            "--detached",
            "--host-seed",
            host_seed_str,
            "--cluster-seed",
            cluster_seed_str,
            "--multi-local",
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>();
        for arg in args.extra_args {
            cmd_args.push(arg);
        }

        // Run `wash up` command
        let output = cmd
            .args(cmd_args)
            .env("HOME", test_dir.path())
            .stdout(Stdio::piped())
            .output()
            .await
            .context("Could not run wash up")?;

        if !output.status.success() {
            bail!("wash up failed with exit code: {}", output.status);
        }

        let UpCommandOutput {
            kill_cmd,
            wasmcloud_log,
            deployed_wadm_manifest_path,
            ..
        } = serde_json::from_slice::<UpCommandOutput>(&output.stdout).with_context(|| {
            format!(
                "failed to parse wash up cmd output, received:\n===\n{}\n===",
                String::from_utf8_lossy(&output.stdout)
            )
        })?;

        // Wait until the host starts by checking the logs
        let logs_path = String::from(wasmcloud_log.to_string().trim_matches('"'));
        tokio::time::timeout(Duration::from_secs(15), async move {
            loop {
                match tokio::fs::read_to_string(&logs_path).await {
                    Ok(file_contents) => {
                        if file_contents.contains("started") {
                            // After wasmcloud says it's ready, it still requires some seconds to start up.
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            break;
                        }
                    }
                    _ => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        })
        .await?;

        Ok(TestWashInstance {
            kill_cmd: kill_cmd.to_string(),
            deployed_wadm_manifest_path,
            nats,
            nats_port,
            host_seed: host_seed_str.into(),
            cluster_seed: cluster_seed_str.into(),
            test_dir,
            host_id,
        })
    }

    /// Get the test dir currently used by this instance
    pub fn test_dir(&self) -> &Path {
        self.test_dir.path()
    }

    /// Get the current nats port
    pub fn nats_port(&self) -> u16 {
        self.nats_port
    }

    /// Returns a [`Command`] that can be used for running wash. This command is preconfigured to
    /// use the test instance's directory and the proper NATS ports.
    pub fn wash_cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wash"));
        cmd.env("HOME", self.test_dir.path());
        cmd.env("WASMCLOUD_CTL_PORT", self.nats_port.to_string());
        cmd
    }

    /// Trigger the equivalent of `wash pull` on a [`TestWashInstance`]
    pub(crate) async fn pull(&self, oci_ref: &str) -> Result<PullCommandOutput> {
        let output = self
            .wash_cmd()
            .args(["pull", oci_ref, "--output", "json"])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("failed to pull OCI artifact [{oci_ref}]"))?;
        serde_json::from_slice(&output.stdout).context("failed to parse output of `wash pull`")
    }

    /// Trigger the equivalent of `wash start component` on a [`TestWashInstance`]
    pub(crate) async fn start_component(
        &self,
        oci_ref: impl AsRef<str>,
        component_id: impl AsRef<str>,
    ) -> Result<StartCommandOutput> {
        let output = self
            .wash_cmd()
            .args([
                "start",
                "component",
                oci_ref.as_ref(),
                component_id.as_ref(),
                "--output",
                "json",
                "--timeout-ms",
                DEFAULT_WASH_INVOCATION_TIMEOUT_MS_ARG,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to start component")?;
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash start component`")
    }

    /// Trigger the equivalent of `wash start provider` on a [`TestWashInstance`]
    pub(crate) async fn start_provider(
        &self,
        oci_ref: impl AsRef<str>,
        component_id: impl AsRef<str>,
    ) -> Result<StartCommandOutput> {
        let output = self
            .wash_cmd()
            .args([
                "start",
                "provider",
                oci_ref.as_ref(),
                component_id.as_ref(),
                "--output",
                "json",
                "--timeout-ms",
                DEFAULT_WASH_INVOCATION_TIMEOUT_MS_ARG,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to start provider")?;

        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash start provider`")
    }

    /// Trigger the equivalent of `wash get hosts` on a [`TestWashInstance`]
    pub(crate) async fn get_hosts(&self) -> Result<GetHostsCommandOutput> {
        let output = self
            .wash_cmd()
            .args(["get", "hosts", "--output", "json"])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to call get hosts")?;
        serde_json::from_slice(&output.stdout).context("failed to parse output of `wash get hosts`")
    }

    /// Trigger the equivalent of `wash call` on a [`TestWashInstance`]
    pub(crate) async fn call_component(
        &self,
        component_id: impl AsRef<str>,
        operation: impl AsRef<str>,
        data: impl AsRef<str>,
    ) -> Result<CallCommandOutput> {
        let component_id = component_id.as_ref();
        let operation = operation.as_ref();
        let output = self
            .wash_cmd()
            .args([
                "call",
                component_id,
                operation,
                "--rpc-timeout-ms",
                DEFAULT_WASH_INVOCATION_TIMEOUT_MS_ARG,
                "--output",
                "json",
                "--http-body",
                data.as_ref(),
            ])
            .output()
            .await
            .with_context(|| {
                format!("failed to call operation [{operation}] on component [{component_id}]")
            })?;
        ensure!(output.status.success(), "wash call invocation failed");
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash call` output")
    }

    /// Trigger the equivalent of `wash stop component` on a [`TestWashInstance`]
    pub(crate) async fn stop_component(
        &self,
        component_id: impl AsRef<str>,
        host_id: Option<String>,
    ) -> Result<StopCommandOutput> {
        // Build dynamic arg list to feed to `wash stop component`
        let mut args: Vec<String> = [
            "stop",
            "component",
            component_id.as_ref(),
            "--output",
            "json",
            "--timeout-ms",
            DEFAULT_WASH_INVOCATION_TIMEOUT_MS_ARG,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        // Add --host-id to args if specified
        // Add host name to argument list if provided
        if let Some(host_id) = host_id {
            args.extend(["--host-id".into(), host_id]);
        }

        let output = self
            .wash_cmd()
            .args(&args)
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to stop component")?;
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash stop component`")
    }

    /// Trigger the equivalent of `wash stop provider` on a [`TestWashInstance`]
    pub(crate) async fn stop_provider(
        &self,
        provider_id: impl AsRef<str>,
        host_id: Option<String>,
    ) -> Result<StopCommandOutput> {
        // Dynamically build arg list to `wash stop provider`
        let mut args: Vec<String> = ["stop", "provider", provider_id.as_ref()]
            .iter()
            .map(ToString::to_string)
            .collect();

        // Add the rest of the arguments
        args.extend(
            [
                "--output",
                "json",
                "--timeout-ms",
                DEFAULT_WASH_INVOCATION_TIMEOUT_MS_ARG,
            ]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
        );
        // Add host name to argument list if provided
        if let Some(host_id) = host_id {
            args.extend(["--host-id".into(), host_id]);
        }

        let output = self
            .wash_cmd()
            .args(&args)
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to stop provider")?;
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash stop provider`")
    }

    /// Trigger the equivalent of `wash stop host` on a [`TestWashInstance`]
    pub(crate) async fn stop_host(&self) -> Result<StopCommandOutput> {
        let output = self
            .wash_cmd()
            .args([
                "stop",
                "host",
                self.host_id.as_ref(),
                "--output",
                "json",
                "--timeout-ms",
                DEFAULT_WASH_INVOCATION_TIMEOUT_MS_ARG,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to stop host")?;
        serde_json::from_slice(&output.stdout).context("failed to parse output of `wash stop host`")
    }

    /// Trigger the equivalent of `wash app deploy` on a [`TestWashInstance`]
    pub(crate) async fn deploy_app(&self, name_or_path: &str) -> Result<AppDeployCommandOutput> {
        let output = self
            .wash_cmd()
            .args(["app", "deploy", name_or_path, "--output", "json"])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to deploy app")?;
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash app deploy`")
    }

    /// Trigger the equivalent of `wash app list` on a [`TestWashInstance`]
    pub(crate) async fn list_apps(&self) -> Result<AppListCommandOutput> {
        let output = self
            .wash_cmd()
            .args(["app", "list", "--output", "json"])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to list apps")?;
        serde_json::from_slice(&output.stdout).context("failed to parse output of `wash app get`")
    }

    /// Trigger the equivalent of `wash app get` on a [`TestWashInstance`]
    pub(crate) async fn get_apps(&self) -> Result<AppGetCommandOutput> {
        let output = self
            .wash_cmd()
            .args(["app", "get", "--output", "json"])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to list apps")?;
        serde_json::from_slice(&output.stdout).context("failed to parse output of `wash app get`")
    }

    /// Trigger the equivalent of `wash app undeploy --all` on a [`TestWashInstance`]
    pub(crate) async fn undeploy_all_apps(&self) -> Result<AppUndeployCommandOutput> {
        let output = self
            .wash_cmd()
            .args(["app", "undeploy", "--all", "--output", "json"])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to undeploy all apps")?;
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash app undeploy --all`")
    }

    /// Trigger the equivalent of `wash app delete --all-undeployed` on a [`TestWashInstance`]
    pub(crate) async fn delete_all_undeployed_apps(&self) -> Result<AppDeleteCommandOutput> {
        let output = self
            .wash_cmd()
            .args(["app", "delete", "--all-undeployed", "--output", "json"])
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to undeploy all apps")?;
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash app undeploy --all`")
    }

    /// Create a wash context
    pub(crate) async fn create_context(
        &self,
        dir: impl AsRef<Path>,
        name: &str,
    ) -> Result<CreateContextCommandOutput> {
        let output = self
            .wash_cmd()
            .args([
                "ctx",
                "new",
                name,
                "--directory",
                &format!("{}", dir.as_ref().display()),
                "--output",
                "json",
            ])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("failed to create context [{name}]"))?;
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash ctx create`")
    }

    /// Delete a wash context
    pub(crate) async fn delete_context(
        &self,
        dir: impl AsRef<Path>,
        name: &str,
    ) -> Result<DeleteContextCommandOutput> {
        let output = self
            .wash_cmd()
            .args([
                "ctx",
                "del",
                "--directory",
                &format!("{}", dir.as_ref().display()),
                "--output",
                "json",
                name,
            ])
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("failed to delete context [{name}]"))?;
        serde_json::from_slice(&output.stdout)
            .context("failed to parse output of `wash ctx delete`")
    }
}

pub struct TestSetup {
    /// The path to the directory for the test.
    /// Added here so that the directory is not deleted until the end of the test.
    #[allow(dead_code)]
    pub test_dir: TempDir,
    /// The path to the created component's directory.
    #[allow(dead_code)]
    pub project_dir: PathBuf,
    /// A temp directory used as a HOME directory for wash commands
    #[allow(dead_code)]
    pub wash_home_dir: TempDir,
}

impl TestSetup {
    #[allow(dead_code)]
    /// Used to create a new [`TestSetup`] instance. This ensures a default wkg config is used for
    /// the test as well
    async fn new(test_dir: TempDir, project_dir: PathBuf) -> Result<Self> {
        let conf = wasm_pkg_client::Config::default();
        conf.to_file(test_dir.path().join(WKG_CONFIG_FILE)).await?;
        Ok(Self {
            test_dir,
            project_dir,
            wash_home_dir: tempfile::tempdir()?,
        })
    }

    #[allow(dead_code)]
    /// A helper that returns a new `wash` binary command configured to use the project directory
    /// and other test configuration
    pub fn base_command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wash"));
        cmd.current_dir(&self.project_dir);
        cmd.env(
            "WKG_CONFIG_FILE",
            self.test_dir.path().join(WKG_CONFIG_FILE),
        );
        cmd.env("WKG_CACHE_DIR", self.test_dir.path().join("cache"));
        cmd.env("HOME", self.wash_home_dir.path());
        cmd
    }
}

#[allow(dead_code)]
pub struct WorkspaceTestSetup {
    /// The path to the directory for the test.
    /// Added here so that the directory is not deleted until the end of the test.
    #[allow(dead_code)]
    pub test_dir: TempDir,
    /// The path to the created component's directory.
    #[allow(dead_code)]
    pub project_dirs: Vec<PathBuf>,
}

/// Inits an component build test by setting up a test directory and creating an component from a template.
/// Returns the paths of the test directory and component directory.
#[allow(dead_code)]
pub async fn init(component_name: &str, template_name: &str) -> Result<TestSetup> {
    let test_dir = TempDir::new()?;
    // Get the current dir so we can reset it after creating the new component
    let project_dir =
        init_component_from_template(component_name, template_name, &test_dir).await?;
    TestSetup::new(test_dir, project_dir).await
}

/// Same as `init`, but takes a path to a template directory. If the given path is absolute, it is
/// used as the template directory, otherwise this will use the top level directory of the
/// repository as the root path it joins the relative path with
#[allow(dead_code)]
pub async fn init_path(component_name: &str, path: impl AsRef<Path>) -> Result<TestSetup> {
    let test_dir = TempDir::new()?;
    let joined_path = if path.as_ref().is_absolute() {
        path.as_ref().to_path_buf()
    } else {
        let root = get_workspace_root()
            .await
            .context("Couldn't get workspace root")?;
        root.join(path)
    };
    let project_dir =
        init_component_from_template_path(component_name, joined_path, &test_dir).await?;
    TestSetup::new(test_dir, project_dir).await
}

/// Initializes a new component from a wasmCloud example in wasmcloud/wasmcloud
#[allow(dead_code)]
pub async fn init_component_from_template(
    component_name: &str,
    template_name: &str,
    parent_dir: impl AsRef<Path>,
) -> Result<PathBuf> {
    let status = Command::new(env!("CARGO_BIN_EXE_wash"))
        .args([
            "new",
            "component",
            component_name,
            "--template-name",
            template_name,
            "--silent",
            "--no-git-init",
        ])
        .current_dir(parent_dir.as_ref())
        .kill_on_drop(true)
        .status()
        .await
        .context("Failed to generate project")?;

    assert!(status.success());

    let project_dir = parent_dir.as_ref().join(component_name);
    Ok(project_dir)
}

/// Initializes a new component from the given path
pub async fn init_component_from_template_path(
    component_name: &str,
    path: impl AsRef<Path>,
    parent_dir: impl AsRef<Path>,
) -> Result<PathBuf> {
    let status = Command::new(env!("CARGO_BIN_EXE_wash"))
        .args([
            "new",
            "component",
            component_name,
            "--path",
            path.as_ref().as_os_str().to_string_lossy().as_ref(),
            "--silent",
            "--no-git-init",
        ])
        .current_dir(parent_dir.as_ref())
        .kill_on_drop(true)
        .status()
        .await
        .context("Failed to generate project")?;
    assert!(status.success());
    let project_dir = parent_dir.as_ref().join(component_name);
    Ok(project_dir)
}

#[allow(dead_code)]
pub async fn init_provider(provider_name: &str, template_name: &str) -> Result<TestSetup> {
    let test_dir = TempDir::new()?;
    let project_dir = init_provider_from_template(provider_name, template_name, &test_dir).await?;
    TestSetup::new(test_dir, project_dir).await
}

/// Same as `init_provider`, but takes a path to a template directory. If the given path is
/// absolute, it is used as the template directory, otherwise this will use the top level directory
/// of the repository as the root path it joins the relative path with
#[allow(dead_code)]
pub async fn init_provider_path(provider_name: &str, path: impl AsRef<Path>) -> Result<TestSetup> {
    let test_dir = TempDir::new()?;
    let joined_path = if path.as_ref().is_absolute() {
        path.as_ref().to_path_buf()
    } else {
        let root = get_workspace_root()
            .await
            .context("Couldn't get workspace root")?;
        root.join(path)
    };
    let project_dir =
        init_provider_from_template_path(provider_name, joined_path, &test_dir).await?;
    TestSetup::new(test_dir, project_dir).await
}

/// Initializes a new provider from a template provider template
#[allow(dead_code)]
pub async fn init_provider_from_template(
    provider_name: &str,
    template_name: &str,
    parent_dir: impl AsRef<Path>,
) -> Result<PathBuf> {
    let status = Command::new(env!("CARGO_BIN_EXE_wash"))
        .args([
            "new",
            "provider",
            provider_name,
            "--template-name",
            template_name,
            "--silent",
            "--no-git-init",
        ])
        .current_dir(parent_dir.as_ref())
        .kill_on_drop(true)
        .status()
        .await
        .context("Failed to generate provider")?;

    assert!(status.success());

    let project_dir = parent_dir.as_ref().join(provider_name);
    Ok(project_dir)
}

/// Initializes a new provider from the given path
#[allow(dead_code)]
pub async fn init_provider_from_template_path(
    provider_name: &str,
    path: impl AsRef<Path>,
    parent_dir: impl AsRef<Path>,
) -> Result<PathBuf> {
    let status = Command::new(env!("CARGO_BIN_EXE_wash"))
        .args([
            "new",
            "provider",
            provider_name,
            "--path",
            path.as_ref().as_os_str().to_string_lossy().as_ref(),
            "--silent",
            "--no-git-init",
        ])
        .current_dir(parent_dir.as_ref())
        .kill_on_drop(true)
        .status()
        .await
        .context("Failed to generate provider")?;
    assert!(status.success());
    let project_dir = parent_dir.as_ref().join(provider_name);
    Ok(project_dir)
}

/// Wait until a process has a given count on the current machine. If a PID is passed, it will
/// filter any processes where the parent PID is the given PID.
// NOTE(thomastaylor312): Why do we have this weird parent PID thing here? I'm glad you asked. So
// what happens is the sysinfo crate currently fetches _all tasks_ also as a processes when on
// linux, which really throws off these tests in CI. This will be fixed once
// https://github.com/GuillaumeGomez/sysinfo/pull/1436 is released, but for now we have to filter
// out by an optional parent if we so desire
#[allow(dead_code)]
pub async fn wait_until_process_has_count(
    filter: &str,
    predicate: impl Fn(usize) -> bool,
    timeout: Duration,
    check_interval: Duration,
    parent_pid: Option<u32>,
) -> Result<()> {
    // Check to see if process was removed
    let mut info = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::everything()
            .with_processes(sysinfo::ProcessRefreshKind::everything()),
    );

    let last_found = std::sync::Arc::new(tokio::sync::Mutex::new(0usize));
    let last_found_clone = last_found.clone();

    tokio::time::timeout(timeout, async move {
        loop {
            info.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let count = info
                .processes()
                .values()
                .filter(|p| {
                    // Because of the way the tests current kill/clean things up, things can exit
                    // before they get killed and the tempdir where things are located gets deleted
                    // out from under the process. This filters out any dead processes so retries
                    // can continue to run. We should find a way to make this better
                    if p.status() == sysinfo::ProcessStatus::Dead {
                        return false;
                    }
                    // On linux, all tasks are also processes and returned in this list. So if the parent
                    // is the process we are looking for, we should ignore it.
                    if let Some(parent_pid) = parent_pid {
                        if p.parent().map(|p| p.as_u32() == parent_pid).unwrap_or(false) {
                            return false;
                        }
                    }
                    let name = p.exe().map(|s| s.to_string_lossy()).unwrap_or_default();
                    name.contains(filter)
                })
                .count();
            *last_found_clone.lock().await = count;
            if predicate(count) {
                break;
            };
            tokio::time::sleep(check_interval).await;
        }
    })
    .await
    .context(format!(
        "failed to find satisfactory amount of processes named [{filter}], last number of processes found: {}",
        *last_found.lock().await
    ))?;

    Ok(())
}

#[allow(dead_code)]
pub async fn wait_for_single_host(
    ctl_port: u16,
    timeout: Duration,
    check_interval: Duration,
) -> Result<Host> {
    tokio::time::timeout(timeout, async move {
        loop {
            let output = Command::new(env!("CARGO_BIN_EXE_wash"))
                .args([
                    "get",
                    "hosts",
                    "--ctl-port",
                    ctl_port.to_string().as_str(),
                    "--output",
                    "json",
                ])
                .output()
                .await
                .context("get host command failed")?;

            // Continue until `wash get hosts` succeeds w/ non-empty content, until timeout
            // this may happen when a NATS instance is unavailable for a certain amount of time
            if !output.status.success() || output.stdout.is_empty() {
                continue;
            }

            let mut cmd_output: GetHostsCommandOutput = serde_json::from_slice(&output.stdout)
                .with_context(|| {
                    format!(
                        "failed to parse get hosts command JSON output: {}",
                        String::from_utf8_lossy(&output.stdout)
                    )
                })?;

            match &cmd_output.hosts[..] {
                [] => {}
                [_h] => break Ok(cmd_output.hosts.remove(0)),
                _ => bail!("unexpected received more than one host"),
            }

            tokio::time::sleep(check_interval).await;
        }
    })
    .await
    .context("failed to wait for single host to exist")?
}

/// Inits an component build test by setting up a test directory and creating an component from a template.
/// Returns the paths of the test directory and component directory.
#[allow(dead_code)]
pub async fn init_workspace(component_names: Vec<&str>) -> Result<WorkspaceTestSetup> {
    let test_dir = TempDir::new()?;

    let project_dirs: Vec<_> =
        futures::future::try_join_all(component_names.iter().map(|component_name| async {
            let project_dir =
                init_component_from_template(component_name, "hello-world-rust", &test_dir).await?;
            Result::<PathBuf>::Ok(project_dir)
        }))
        .await?;

    let members = component_names
        .iter()
        .map(|component_name| format!("\"{component_name}\""))
        .collect::<Vec<_>>()
        .join(",");
    let cargo_toml = format!(
        "
    [workspace]
    members = [{members}]
    "
    );

    let mut cargo_path = PathBuf::from(test_dir.path());
    cargo_path.push("Cargo.toml");
    let mut file = File::create(cargo_path).await?;
    file.write_all(cargo_toml.as_bytes()).await?;
    Ok(WorkspaceTestSetup {
        test_dir,
        project_dirs,
    })
}

/// Wait for no hosts to be running by checking for process names,
/// expecting that the wasmcloud process invocation contains `wasmcloud_host`
#[allow(dead_code)]
pub async fn wait_for_no_hosts() -> Result<()> {
    wait_for_num_hosts(0)
        .await
        .context("number of hosts running is still non-zero")?;
    let lockfile = host_pid_file()?;
    if wait_for_file_to_be_removed(&lockfile).await.is_err() {
        // If the PID file wasn't removed, attempt to delete it manually
        tokio::fs::remove_file(&lockfile).await.with_context(|| {
            format!(
                "failed to delete wasmcloud PID file at [{}]",
                lockfile.display()
            )
        })?;
    }
    Ok(())
}

/// Waits until the number of hosts running matches the expected number
pub async fn wait_for_num_hosts(num_hosts: usize) -> Result<()> {
    wait_until_process_has_count(
        WASMCLOUD_HOST_BIN,
        |v| v == num_hosts,
        // This is longer because it takes a bit to download the host when starting `dev` or `up`.
        Duration::from_secs(180),
        Duration::from_secs(2),
        None,
    )
    .await
    .context(format!("number of hosts running is not [{num_hosts}]"))
}

/// Wait for a file to be removed.
pub async fn wait_for_file_to_be_removed(file_path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if !file_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "file {} was not removed by previous test",
            file_path.display()
        )
    })
}

/// Wait for NATS to start running by checking for process names.
/// expecting that exactly one 'nats-server' process is running
#[allow(dead_code)]
pub async fn wait_for_nats_to_start(parent_pid: u32) -> Result<()> {
    wait_until_process_has_count(
        "nats-server",
        |v| v == 1,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Some(parent_pid),
    )
    .await
    .context("at least one nats-server process has not started")
}

/// Wait for no nats to be running by checking for process names
#[allow(dead_code)]
pub async fn wait_for_no_nats() -> Result<()> {
    wait_until_process_has_count(
        "nats-server",
        |v| v == 0,
        Duration::from_secs(10),
        Duration::from_millis(250),
        None,
    )
    .await
    .context("number of nats-server processes should be zero")
}

#[allow(dead_code)]
pub async fn wait_for_no_wadm() -> Result<()> {
    wait_until_process_has_count(
        WADM_BINARY,
        |v| v == 0,
        Duration::from_secs(15),
        Duration::from_millis(250),
        None,
    )
    .await
    .context("number of wadm processes should be zero")?;
    let lockfile = wadm_pid_file()?;
    if wait_for_file_to_be_removed(&lockfile).await.is_err() {
        // If the PID file wasn't removed, attempt to delete it manually
        tokio::fs::remove_file(&lockfile).await.with_context(|| {
            format!("failed to delete wadm PID file at [{}]", lockfile.display())
        })?;
    }
    Ok(())
}

/// Helper that gets the top level directory of a workspace.
#[allow(dead_code)]
pub async fn get_workspace_root() -> Result<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args([
            "locate-project",
            "--workspace",
            "-q",
            "--message-format=plain",
        ])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to get workspace root: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(PathBuf::from(String::from_utf8(output.stdout)?)
        .parent()
        .unwrap()
        .to_path_buf())
}

/// Gets the path to the fixture
#[allow(dead_code)]
pub fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Loads the fixture with the given name into a temporary directory. This will copy the fixture
/// from the tests/fixtures directory into a temporary directory and return the tempdir containing
/// that directory (and its path)
#[allow(dead_code)]
pub async fn load_fixture(fixture: &str) -> anyhow::Result<TestSetup> {
    let temp_dir = tempfile::tempdir()?;
    let fixture_path = fixture_dir().join(fixture);
    // This will error if it doesn't exist, which is what we want
    tokio::fs::metadata(&fixture_path).await?;
    let copied_path = temp_dir.path().join(fixture_path.file_name().unwrap());
    copy_dir(&fixture_path, &copied_path).await?;
    TestSetup::new(temp_dir, copied_path).await
}

#[allow(dead_code)]
async fn copy_dir(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&destination).await?;
    let mut entries = tokio::fs::read_dir(source).await?;
    while let Some(entry) = entries.next_entry().await? {
        let filetype = entry.file_type().await?;
        if filetype.is_dir() {
            // Skip the deps directory in case it is there from debugging
            if entry.path().file_name().unwrap_or_default() == "deps" {
                continue;
            }
            Box::pin(copy_dir(
                entry.path(),
                destination.as_ref().join(entry.file_name()),
            ))
            .await?;
        } else {
            let path = entry.path();
            let extension = path.extension().unwrap_or_default();
            // Skip any .lock or .wasm files that might be there from debugging
            if extension == "lock" || extension == "wasm" {
                continue;
            }
            tokio::fs::copy(path, destination.as_ref().join(entry.file_name())).await?;
        }
    }
    Ok(())
}

#[allow(unused)]
fn filter_process(process_name: &str) -> impl FnMut(&&sysinfo::Process) -> bool + use<'_> {
    move |p: &&sysinfo::Process| {
        p.exe()
            .map(|s| s.to_string_lossy().contains(process_name))
            .unwrap_or(false)
    }
}

#[allow(unused)]
#[cfg(target_family = "unix")]
/// Forcefully kill all wasmCloud and NATS processes that might be lingering from previous tests
pub async fn force_cleanup_processes() -> Result<()> {
    // First, try to kill all wasmcloud_host processes
    kill_processes_by_name(WASMCLOUD_HOST_BIN).await?;

    // Then, try to kill all nats-server processes
    kill_processes_by_name("nats-server").await?;

    // Also kill any wadm processes that might be lingering
    kill_processes_by_name(WADM_BINARY).await?;

    // Wait a moment to ensure processes are gone
    tokio::time::sleep(Duration::from_millis(500)).await;

    Ok(())
}

#[allow(unused)]
#[cfg(target_family = "unix")]
/// Kill all processes matching a given name
async fn kill_processes_by_name(process_name: &str) -> Result<()> {
    let mut system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::everything()
            .with_processes(sysinfo::ProcessRefreshKind::everything()),
    );

    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    // Find all matching processes
    let matching_pids: Vec<i32> = system
        .processes()
        .values()
        .filter(filter_process(process_name))
        .map(|p| p.pid().as_u32() as i32)
        .collect();

    // Kill each process with SIGKILL
    for pid in matching_pids {
        if let Err(e) = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            // Ignore errors here - process might have already terminated
            eprintln!("Note: Failed to send SIGKILL to process {pid}: {e}");
        }
    }

    // Verify processes are gone
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let count = system
                .processes()
                .values()
                .filter(filter_process(process_name))
                .count();

            if count == 0 {
                break;
            }

            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context(format!(
        "Timed out waiting for {process_name} processes to terminate"
    ))?;

    Ok(())
}
