//! `robotctl` - one binary for robot identity, WiFi, status and experiment stacks.

mod conf;
mod config;
mod experiments;
mod id;
mod log;
mod status;
mod systemd;
#[cfg(test)]
mod testutil;
mod wifi;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::config::Paths;
use crate::id::{RobotId, RosDomainId};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: RobotctlCommand,
}

#[derive(Subcommand, Debug)]
enum RobotctlCommand {
    /// Read and write robot identity.
    Id {
        #[command(subcommand)]
        action: IdAction,
    },
    /// WiFi client, fallback AP, and the supervisor state machine.
    Wifi {
        #[command(subcommand)]
        action: WifiAction,
    },
    /// Report identity, image, RAUC slot, network, disk and service health.
    Status {
        /// Emit a single JSON document instead of the human-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Run the docker-compose experiment stacks listed in enabled.conf.
    Experiments {
        #[command(subcommand)]
        action: ExperimentsAction,
    },
    /// First-boot provisioning.
    Provision,
}

impl RobotctlCommand {
    fn run(self, paths: &Paths) -> Result<()> {
        match self {
            RobotctlCommand::Id { action } => action.run(paths),
            RobotctlCommand::Wifi { action } => action.run(paths),
            RobotctlCommand::Status { json } => status::print(paths, json),
            RobotctlCommand::Experiments { action } => action.run(paths),
            RobotctlCommand::Provision => anyhow::bail!("provisioning is not implemented yet"),
        }
    }
}

#[derive(Subcommand, Debug)]
enum IdAction {
    /// Print one identity value, or all of them.
    Get {
        #[arg(value_enum)]
        key: IdKey,
    },
    /// Set an identity value and re-derive everything downstream of it.
    Set {
        #[command(subcommand)]
        target: IdTarget,
    },
    /// Re-derive /etc/hostname, /etc/hosts and the running hostname from robot.conf.
    ///
    /// Idempotent, and intended to run on every boot from a systemd unit ordered after /data is
    /// mounted and before anything that cares about the hostname.
    Apply,
}

impl IdAction {
    fn run(self, paths: &Paths) -> Result<()> {
        match self {
            IdAction::Get { key } => match key {
                IdKey::RobotId => {
                    println!("{}", id::robot_id(paths)?);
                    Ok(())
                }
                IdKey::RosDomainId => {
                    println!("{}", id::ros_domain_id(paths)?);
                    Ok(())
                }
            },

            IdAction::Set { target } => match target {
                IdTarget::RobotId { value } => id::set_robot_id(paths, &parse_robot_id(&value)?),
                IdTarget::RosDomainId { value } => {
                    id::set_ros_domain_id(paths, parse_ros_domain_id(&value)?)
                }
            },

            IdAction::Apply => {
                let config = id::load(paths)?;
                id::apply(paths, &config)
            }
        }
    }
}

#[derive(Subcommand, Debug)]
enum WifiAction {
    /// Report what the WiFi interface is currently doing.
    Status,
    /// Bring WiFi up and keep it up: the client / fallback-AP state machine.
    Start,
    /// Stop the WiFi stack and put the interface down.
    Stop,
}

impl WifiAction {
    fn run(self, paths: &Paths) -> Result<()> {
        match self {
            WifiAction::Status => wifi::print_status(paths),
            WifiAction::Start => wifi::start(paths),
            WifiAction::Stop => wifi::stop(paths),
        }
    }
}

#[derive(Subcommand, Debug)]
enum ExperimentsAction {
    /// Pull images and bring every enabled stack up.
    ///
    /// A failed pull is fatal and the stack is not started
    Start {
        /// Override IGNORE_PULL_FAILURES from experiments.conf for this run.
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        ignore_pull_failures: Option<bool>,
    },
    /// Bring every enabled stack down.
    Stop,
    /// Show `docker compose ps` for every enabled stack.
    Status,
}

impl ExperimentsAction {
    fn run(self, paths: &Paths) -> Result<()> {
        match self {
            ExperimentsAction::Start {
                ignore_pull_failures,
            } => experiments::start(
                paths,
                experiments::StartOptions {
                    ignore_pull_failures,
                },
            ),
            ExperimentsAction::Stop => experiments::stop(paths),
            ExperimentsAction::Status => experiments::status(paths),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum IdKey {
    RobotId,
    RosDomainId,
}

#[derive(Subcommand, Debug)]
enum IdTarget {
    /// The robot's name, and the source of truth for its hostname.
    RobotId { value: String },
    /// The ROS 2 domain ID (0-232).
    RosDomainId { value: String },
}

fn main() {
    if let Err(err) = run() {
        log::error(format!("{err:#}"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    Args::parse().command.run(&Paths::from_env())
}

fn parse_robot_id(value: &str) -> Result<RobotId> {
    RobotId::new(value).with_context(|| format!("invalid robot ID '{value}'"))
}

fn parse_ros_domain_id(value: &str) -> Result<RosDomainId> {
    value
        .parse::<RosDomainId>()
        .with_context(|| format!("invalid ROS domain ID '{value}'"))
}
