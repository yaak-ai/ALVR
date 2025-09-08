pub mod commands;
mod parse;

use alvr_common::anyhow::Result;
use alvr_common::{dbg_connection, error};
use alvr_system_info::{
    ClientFlavor, PACKAGE_NAME_GITHUB_DEV, PACKAGE_NAME_GITHUB_STABLE, PACKAGE_NAME_STORE,
};
use std::collections::HashSet;
use std::time::{Duration, Instant};

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

        let Some(process_name) = get_process_name(&self.adb_path, &device_serial, client_type)
        else {            
            return Ok(WiredConnectionStatus::NotReady(
                "No suitable ALVR client is installed".to_owned(),
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

pub fn get_process_name(
    adb_path: &str,
    device_serial: &str,
    flavor: &ClientFlavor,
) -> Option<String> {
    let fallbacks = match flavor {
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
    };

    fallbacks
        .iter()
        .find(|name| {
            commands::is_package_installed(adb_path, device_serial, name)
                .is_ok_and(|installed| installed)
        })
        .map(|name| (*name).to_string())
}
