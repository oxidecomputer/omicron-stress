use clap::Parser;
use std::path::PathBuf;

/// Command-line configuration options.
#[derive(Parser)]
pub struct Config {
    /// The number of test instances to create.
    #[arg(long, default_value_t = 4)]
    pub num_test_instances: usize,

    /// The number of antagonist threads to create for each instance.
    #[arg(long, default_value_t = 4)]
    pub threads_per_instance: usize,

    /// The number of test disks to create.
    #[arg(long, default_value_t = 4)]
    pub num_test_disks: usize,

    /// The number of antagonist threads to create for each disk.
    #[arg(long, default_value_t = 4)]
    pub threads_per_disk: usize,

    /// The number of test snapshots to create.
    #[arg(long, default_value_t = 4)]
    pub num_test_snapshots: usize,

    /// If true, all snapshot antagonishs use the same disk. If false, then use
    /// one disk per snapshot antagonist.
    #[arg(long)]
    pub snapshots_use_same_disk: bool,

    /// The number of antagonist threads to create for each snapshot.
    #[arg(long, default_value_t = 4)]
    pub threads_per_snapshot: usize,

    /// The URI of the Nexus instance the stress test should interact with.
    /// If not set, falls back to the value of the OXIDE_HOST environment
    /// variable.
    #[arg(long)]
    pub host_uri: Option<String>,

    /// The directory in which to search for a `hosts.toml` file from which to
    /// read an authentication token to supply to Nexus. If not set, defaults to
    /// $HOME_DIRECTORY/.config/oxide. If no token is found with the
    /// `hosts.toml` method, falls back to the value of the OXIDE_TOKEN
    /// environment variable.
    #[arg(long)]
    pub hosts_toml_dir: Option<PathBuf>,

    /// Halt omicron-stress if a 500 series error was seen
    #[arg(long)]
    pub server_errors_fatal: bool,
}
