use std::env;
/// We want to keep the qdrant binary running here so we can use that along
/// with the client to power our semantic search and everything else which is
/// required.
use std::fs::write;
use std::path::Path;
use std::{fs::create_dir_all, process::Child};

use anyhow::Result;
use tracing::info;

use crate::application::config::configuration::Configuration;

pub struct QdrantServerProcess {
    child: Option<Child>,
}

/// This will drop the child process and when it exits, it will kill the process
impl Drop for QdrantServerProcess {
    fn drop(&mut self) {
        info!("Dropping QdrantServerProcess");
        if let Some(mut child) = self.child.take() {
            child.kill().unwrap();
        }
    }
}

impl QdrantServerProcess {
    /// If we already have the qdrant server running, we don't have to start
    /// our own process
    pub async fn initialize(configuration: &Configuration) -> Result<Self> {
        let qdrant_location = configuration.qdrant_storage();
        let qd_config_dir = qdrant_location.join("config");
        // Create the directory if it does not exist
        create_dir_all(&qd_config_dir).unwrap();

        // Write out the config file to the proper location
        write(
            qd_config_dir.join("config.yaml"),
            format!(
                include_str!("QDRANT.yaml"),
                storage = &qdrant_location.join("storage").to_string_lossy(),
                snapshots = &qdrant_location.join("snapshots").to_string_lossy()
            ),
        )
        .unwrap();

        let qdrant_binary_directory = configuration
            .qdrant_binary_directory
            .clone()
            .expect("qdrant binary directory should be present");
        let binary_name = get_qdrant_binary_name().expect("qdrant binary to be present");
        info!(?binary_name, "qdrant binary name");
        let binary_path = qdrant_binary_directory.join(binary_name);

        let child = Some(run_command(&binary_path, &qdrant_location));

        info!("qdrant process started");

        Ok(Self { child })
    }
}

fn get_qdrant_binary_name() -> Option<String> {
    let os = env::consts::OS;
    if os == "macos" {
        Some("qdrant_mac".to_owned())
    } else {
        None
    }
}

#[cfg(unix)]
fn run_command(command: &Path, qdrant_dir: &Path) -> Child {
    use std::process::Command;

    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    use tracing::error;
    match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok((current_soft, current_hard)) if current_hard < 16535 => {
            if let Err(err) = setrlimit(Resource::RLIMIT_NOFILE, 2048, 16535) {
                error!(
                    ?err,
                    new_soft = 2048,
                    new_hard = 16535,
                    current_soft,
                    current_hard,
                    "failed to set rlimit/nofile"
                );
            }
        }
        Ok((current_soft, current_hard)) => {
            info!(current_soft, current_hard, "no change to rlimit needed");
        }
        Err(err) => {
            error!(?err, "failed to get rlimit/nofile");
        }
    }

    // nix::sys::resource::setrlimit().unwrap();
    info!(?command, ?qdrant_dir, "qdrant process");
    Command::new(command)
        .current_dir(qdrant_dir)
        .spawn()
        .expect("failed to start qdrant")
}

#[cfg(windows)]
fn run_command(command: &Path, qdrant_dir: &Path) -> Child {
    use std::os::windows::process::CommandExt;

    Command::new(command)
        .current_dir(qdrant_dir)
        // Add a CREATE_NO_WINDOW flag to prevent qdrant console popup
        .creation_flags(0x08000000)
        .spawn()
        .expect("failed to start qdrant")
}

/// We want to wait for qdrant to startup so we can do embedding search etc
pub async fn wait_for_qdrant() {
    use qdrant_client::prelude::*;
    let qdrant =
        // This is hardcoded but its the default which can be found in QDRANT.yaml file
        QdrantClient::new(Some(QdrantClientConfig::from_url("http://127.0.0.1:6334"))).unwrap();

    for _ in 0..60 {
        info!("pinging qdrant server");
        if qdrant.health_check().await.is_ok() {
            info!("success, qdrant server alive");
            return;
        }
        info!("qdrant server not alive, sleeping for 1 second");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    panic!("qdrant cannot be started");
}
