pub mod commands;
mod parse;

use alvr_common::anyhow::Result;
use alvr_common::{dbg_connection, error};
use alvr_session::WiredClientAutoInstallConfig;
use alvr_system_info::{
    ClientFlavor, PACKAGE_NAME_GITHUB_DEV, PACKAGE_NAME_GITHUB_STABLE, PACKAGE_NAME_STORE,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, Instant};
use std::{fs,io};

use sha1::{Sha1, Digest};

pub enum WiredConnectionStatus {
    Ready,
    NotReady(String),
}

pub struct WiredConnection {
    adb_path: String,
    initial_autolaunch_delay: Option<Instant>,
    post_autolaunch_delay: Option<Instant>
}

impl WiredConnection {
    pub fn new(
        layout: &alvr_filesystem::Layout,
        download_progress_callback: impl Fn(usize, Option<usize>),
    ) -> Result<Self> {
        let adb_path = commands::require_adb(layout, download_progress_callback)?;

        Ok(Self { adb_path, initial_autolaunch_delay: None, post_autolaunch_delay: None })
    }

    pub fn setup(
        &mut self,        
        control_port: u16,
        stream_port: u16,
        client_type: &ClientFlavor,
        client_autolaunch: bool,
        layout: &alvr_filesystem::Layout,
        client_autoinstall_path: Option<WiredClientAutoInstallConfig>,
    ) -> Result<WiredConnectionStatus> {
        let Some(device_serial) = commands::list_devices(&self.adb_path)?
            .into_iter()
            .filter_map(|d| d.serial)
            .find(|s| !s.starts_with("127.0.0.1"))
        else {
            self.initial_autolaunch_delay = None;
            self.post_autolaunch_delay = None;
            return Ok(WiredConnectionStatus::NotReady(
                "No wired devices found".to_owned(),
            ));
        };

        let initial_autolaunch_delay = match self.initial_autolaunch_delay {
            Some(t) => t,
            None => {
                let t = Instant::now();
                self.initial_autolaunch_delay = Some( t ); // Start pre auto launch delay as soon as there is an adb connection
                t
            }
        };        

        let ports = HashSet::from([control_port, stream_port]);
        let forwarded_ports: HashSet<u16> =
            commands::list_forwarded_ports(&self.adb_path, &device_serial)?
                .into_iter()
                .map(|f| f.local)
                .collect();
        let missing_ports = ports.difference(&forwarded_ports);
        for port in missing_ports {
            commands::forward_port(&self.adb_path, &device_serial, *port)?;
            dbg_connection!(
                "setup_wired_connection: Forwarded port {port} of device {device_serial}"
            );
        }

        let application_ids = get_application_ids(client_type);

        if let Some(client_autoinstall) = client_autoinstall_path {            
            let mut client_autoinstall_path = PathBuf::from_str(&client_autoinstall.client_package_location)?;

            if client_autoinstall_path.is_relative() {
                client_autoinstall_path = layout.static_resources_dir.join(client_autoinstall_path);
            }

            dbg_connection!("wired_connection: checking auto install path {client_autoinstall_path:?}");

            if client_autoinstall_path.exists() && let Some(application_id)=application_ids.first() {

                let apk_path = client_autoinstall_path.to_string_lossy();
                let installed_sha1 = commands::get_package_sha1(&self.adb_path, &device_serial, application_id)?;
                dbg_connection!("wired_connection: installed package sha1 is {installed_sha1:?}");

                if let Some(installed_sha1) = installed_sha1 {
                    dbg_connection!("wired_connection: installed client hash could be read");
                    dbg_connection!("wired_connection: reading installed client from {client_autoinstall_path:?}");
                    let mut file = fs::File::open(&client_autoinstall_path)?;
                    let mut hasher = Sha1::new();
                    io::copy(&mut file, &mut hasher)?;
                    let hash = hasher.finalize();
                    let hash_str = format!("{hash:x}");
                    dbg_connection!("wired_connection: local client hash is {hash_str}");

                    if !installed_sha1.eq_ignore_ascii_case(&hash_str) {
                        dbg_connection!("wired_connection: hashes don't match");
                        dbg_connection!("wired_connection: uninstalling existing package");
                        commands::uninstall_package(&self.adb_path, &device_serial, application_id)?;
                        dbg_connection!("wired_connection: installing new package from {apk_path}");
                        commands::install_package(&self.adb_path, &device_serial, &apk_path)?;                        
                        client_autoinstall.permissions.iter().try_for_each(                            
                            |permission| {
                                dbg_connection!("wired_connection: granting permission {permission}");
                                commands::grant_package_permission(&self.adb_path, &device_serial, application_id, permission)
                            }
                        )?;
                    }
                    else {
                        dbg_connection!("wired_connection: hashes match");
                    }
                } else {
                    dbg_connection!("wired_connection: installing new package from {apk_path}");
                    commands::install_package(&self.adb_path, &device_serial, &apk_path)?;
                    client_autoinstall.permissions.iter().try_for_each(                            
                        |permission| {
                            dbg_connection!("wired_connection: granting permission {permission}");
                            commands::grant_package_permission(&self.adb_path, &device_serial, application_id, permission)
                        }
                    )?;
                }
            }
        }

        let Some(process_name) = get_process_name(&self.adb_path, &device_serial, application_ids)
        else {            
            return Ok(WiredConnectionStatus::NotReady(
                "No suitable ALVR client is installed".to_owned(),
            ));
        };

        if commands::get_process_id(&self.adb_path, &device_serial, &process_name)?.is_none() {
            if client_autolaunch && self.post_autolaunch_delay.is_none() {
                if initial_autolaunch_delay.elapsed() < Duration::from_secs(15) {
                    return Ok(WiredConnectionStatus::NotReady(
                        "Awaiting pre autolaunch delay".to_owned(),
                    ));
                }
                
                commands::start_application(&self.adb_path, &device_serial, &process_name)?;
                self.post_autolaunch_delay = Some(Instant::now());
                
                Ok(WiredConnectionStatus::NotReady(
                    "Starting ALVR client".to_owned(),
                ))
            } else {
                Ok(WiredConnectionStatus::NotReady(
                    "ALVR client is not running".to_owned(),
                ))
            }
        } else if !commands::is_activity_resumed(&self.adb_path, &device_serial, &process_name)? {
            Ok(WiredConnectionStatus::NotReady(
                "ALVR client is paused".to_owned(),
            ))
        } else {
            if let Some(t) = self.post_autolaunch_delay {
                if t.elapsed() < Duration::from_secs(5) {
                    return Ok(WiredConnectionStatus::NotReady(
                        "Awaiting post autolaunch delay".to_owned(),
                    ));
                } else {
                    self.post_autolaunch_delay = None;
                }
            }

            Ok(WiredConnectionStatus::Ready)
        }
    }
}

impl Drop for WiredConnection {
    fn drop(&mut self) {
        dbg_connection!("wired_connection: Killing ADB server");
        if let Err(e) = commands::kill_server(&self.adb_path) {
            error!("{e:?}");
        }
    }
}

pub fn get_application_ids(flavor: &ClientFlavor) -> Vec<&str> {
    match flavor {
        ClientFlavor::Store => {
            if alvr_common::is_stable() {
                vec![PACKAGE_NAME_STORE, PACKAGE_NAME_GITHUB_STABLE]
            } else {
                vec![PACKAGE_NAME_GITHUB_DEV]
            }
        }
        ClientFlavor::Github => {
            if alvr_common::is_stable() {
                vec![PACKAGE_NAME_GITHUB_STABLE, PACKAGE_NAME_STORE]
            } else {
                vec![PACKAGE_NAME_GITHUB_DEV]
            }
        }
        ClientFlavor::Custom(name) => {
            if alvr_common::is_stable() {
                vec![name, PACKAGE_NAME_STORE, PACKAGE_NAME_GITHUB_STABLE]
            } else {
                vec![name, PACKAGE_NAME_GITHUB_DEV]
            }
        }
    }
}

pub fn get_process_name(
    adb_path: &str,
    device_serial: &str,
    application_ids: Vec<&str>,
) -> Option<String> {
    application_ids
        .iter()
        .find(|name| {
            commands::is_package_installed(adb_path, device_serial, name)
                .is_ok_and(|installed| installed)
        })
        .map(|name| (*name).to_string())
}
