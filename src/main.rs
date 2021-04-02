#[macro_use]
extern crate clap;
extern crate config;
extern crate ctrlc;
extern crate failure;
extern crate fuser;
extern crate gcsf;
#[macro_use]
extern crate log;
extern crate itertools;
extern crate pretty_env_logger;
extern crate serde;
extern crate serde_json;
extern crate xdg;

use clap::App;
use failure::{err_msg, Error};
use itertools::Itertools;
use std::ffi::OsStr;
use std::fs;
use std::io::prelude::*;
use std::iter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time;

use gcsf::{Config, DriveFacade, Gcsf, NullFs};

const DEBUG_LOG: &str =
    "hyper::client=info,hyper::http=info,hyper::net=info,rustls::client::hs=info,debug";

const INFO_LOG: &str =
    "hyper::client=error,hyper::http=error,hyper::net=error,fuser::session=error,info";

const DEFAULT_CONFIG: &str = r#"
### This is the configuration file that GCSF uses.
### It should be placed in $XDG_CONFIG_HOME/gcsf/gcsf.toml, which is usually
### defined as $HOME/.config/gcsf/gcsf.toml

# Show additional logging info?
debug = false

# Perform a mount check and fail early if it fails. Disable this if you
# encounter this error:
#
#     fuse: attempt to remount on active mount point: [...]
#     Could not mount to [...]: Undefined error: 0 (os error 0)
mount_check = true

# How long to cache the contents of a file after it has been accessed.
cache_max_seconds = 300

# How how many files to cache.
cache_max_items = 10

# How long to cache the size and capacity of the file system. These are the
# values reported by `df`.
cache_statfs_seconds = 60

# How many seconds to wait before checking for remote changes and updating them
# locally.
sync_interval = 10

# Mount options
mount_options = [
    "fsname=GCSF",
    # Allow file system access to root. This only works if `user_allow_other`
    # is set in /etc/fuse.conf
    "allow_root",
]

# If set to true, Google Drive will provide a code after logging in and
# authorizing GCSF. This code must be copied and pasted into GCSF in order to
# complete the process. Useful for running GCSF on a remote server.
#
# If set to false, Google Drive will attempt to communicate with GCSF directly.
# This is usually faster and more convenient.
authorize_using_code = false

# If set to true, all files with identical name will get an increasing number attached to the suffix.
rename_identical_files = false

# If set to true, will add an extension to special files (docs, presentations, sheets, drawings, sites), e.g. "\#.ods" for spreadsheets.
add_extensions_to_special_files = false

# If set to true, deleted files will remove them permanently instead of moving them to Trash.
# Deleting trashed files always removes them permanently.
skip_trash = false

# The Google OAuth client secret for Google Drive APIs. Create your own
# credentials at https://console.developers.google.com and paste them here
client_secret = """{"installed":{"client_id":"892276709198-2ksebnrqkhihtf5p743k4ce5bk0n7p5a.apps.googleusercontent.com","project_id":"gcsf-v02","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token","auth_provider_x509_cert_url":"https://www.googleapis.com/oauth2/v1/certs","client_secret":"1ImxorJzh-PuH2CxrcLPnJMU","redirect_uris":["urn:ietf:wg:oauth:2.0:oob","http://localhost"]}}"""
"#;

fn mount_gcsf(config: Config, mountpoint: &str) {
    let vals = config.mount_options();
    let options = iter::repeat("-o")
        .interleave_shortest(vals.iter().map(String::as_ref))
        .take(2 * vals.len())
        .map(OsStr::new)
        .collect::<Vec<_>>();

    debug!("Mount options: {:#?}", options);

    if config.mount_check() {
        match fuser::spawn_mount(NullFs {}, &mountpoint, &options) {
            Ok(_session) => {
                info!("Test mount of NullFs successful. Will mount GCSF next.");
            }
            Err(e) => {
                error!("Could not mount NullFs to {}: {}", &mountpoint, e);
                return;
            }
        };
    }

    info!("Creating and populating file system...");
    let fs: Gcsf = match Gcsf::with_config(config) {
        Ok(fs) => fs,
        Err(e) => {
            error!("Could not create GCSF instance: {}", e);
            return;
        }
    };
    info!("File system created.");
    info!("Mounting to {}", &mountpoint);
    match fuser::spawn_mount(fs, &mountpoint, &options) {
        Ok(_session) => {
            info!("Mounted to {}", &mountpoint);

            let running = Arc::new(AtomicBool::new(true));
            let r = running.clone();

            ctrlc::set_handler(move || {
                info!("Ctrl-C detected");
                r.store(false, Ordering::SeqCst);
            })
            .expect("Error setting Ctrl-C handler");

            while running.load(Ordering::SeqCst) {
                thread::sleep(time::Duration::from_millis(50));
            }
        }
        Err(e) => error!("Could not mount to {}: {}", &mountpoint, e),
    };
}

fn login(config: &mut Config) -> Result<(), Error> {
    debug!("{:#?}", &config);

    if config.token_file().exists() {
        return Err(err_msg(format!(
            "token file {:?} already exists.",
            config.token_file()
        )));
    }

    // Create a DriveFacade which will store the authentication token in the desired file.
    // And make an arbitrary request in order to trigger the authentication process.
    let mut df = DriveFacade::new(&config);
    let _result = df.root_id();

    Ok(())
}

fn load_conf() -> Result<Config, Error> {
    let xdg_dirs = xdg::BaseDirectories::with_prefix("gcsf").unwrap();
    let config_file = xdg_dirs
        .place_config_file("gcsf.toml")
        .map_err(|_| err_msg("Cannot create configuration directory"))?;

    info!("Config file: {:?}", &config_file);

    if !config_file.exists() {
        let mut config_file = fs::File::create(config_file.clone())
            .map_err(|_| err_msg("Could not create config file"))?;
        config_file.write_all(DEFAULT_CONFIG.as_bytes())?;
    }

    let mut settings = config::Config::default();
    settings
        .merge(config::File::with_name(config_file.to_str().unwrap()))
        .expect("Invalid configuration file");

    let mut config = settings.try_into::<Config>()?;
    config.config_dir = Some(xdg_dirs.get_config_home());

    Ok(config)
}

fn main() {
    let mut config = load_conf().expect("Could not load configuration file.");

    if config.debug() {
        info!("Debug mode enabled.");
    }

    pretty_env_logger::formatted_builder()
        .parse_filters(if config.debug() { DEBUG_LOG } else { INFO_LOG })
        .init();

    let yaml = load_yaml!("cli.yml");
    let matches = App::from_yaml(yaml).get_matches();

    if let Some(matches) = matches.subcommand_matches("login") {
        config.session_name = Some(matches.value_of("session_name").unwrap().to_string());

        match login(&mut config) {
            Ok(_) => {
                println!(
                    "Successfully logged in. Saved credentials to {:?}",
                    &config.token_file()
                );
            }
            Err(e) => {
                error!("Could not log in: {}", e);
            }
        };
    }

    if let Some(matches) = matches.subcommand_matches("logout") {
        config.session_name = Some(matches.value_of("session_name").unwrap().to_string());
        let tf = config.token_file();
        match fs::remove_file(&tf) {
            Ok(_) => {
                println!("Successfully removed {:?}", &tf);
            }
            Err(e) => {
                println!("Could not remove {:?}: {}", &tf, e);
            }
        };
    }

    if let Some(_matches) = matches.subcommand_matches("list") {
        let exception = String::from("gcsf.toml");
        let mut sessions: Vec<_> = fs::read_dir(&config.config_dir())
            .unwrap()
            .map(Result::unwrap)
            .map(|f| f.file_name().to_str().unwrap().to_string())
            .filter(|name| name != &exception)
            .collect();
        sessions.sort();

        if sessions.is_empty() {
            println!("No sessions found.");
        } else {
            println!("Sessions:");
            for session in sessions {
                println!("\t- {}", &session);
            }
        }
    }

    if let Some(matches) = matches.subcommand_matches("mount") {
        let mountpoint = matches.value_of("mountpoint").unwrap();
        config.session_name = Some(matches.value_of("session_name").unwrap().to_string());

        if !config.token_file().exists() {
            error!("Token file {:?} does not exist.", config.token_file());
            error!("Try logging in first using `gcsf login`.");
            return;
        }

        if config.client_secret.is_none() {
            error!("No Google OAuth client secret was provided.");
            error!("Try deleting your config file to force GCSF to generate it with the default credentials.");
            error!("Alternatively, you can create your own credentials or manually set the default ones from https://github.com/harababurel/gcsf/blob/master/sample_config.toml");
            return;
        }

        mount_gcsf(config, mountpoint);
    }
}
