mod config;
mod device;

use std::convert::TryFrom;
use std::fs::{self, File};
use std::sync::{mpsc, Mutex};

use crate::config::{Config, InfluxDbConfig};
use crate::device::Device;
use bus::Bus;
use chrono::Local;
use clap::{app_from_crate, crate_authors, crate_description, crate_name, crate_version, Arg};
use crossbeam_utils::thread as crossbeam_thread;
use isahc;
use log::{debug, info, warn, error};
use modbus::{tcp::Transport, Error as ModbusError};
use simplelog::{Config as LogConfig, TermLogger, TerminalMode, WriteLogger};

fn main() -> Result<(), Box<dyn std::error::Error + 'static>> {
    // Parse command line arguments
    let matches = app_from_crate!()
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .takes_value(true)
                .value_name("FILE")
                .default_value("config.toml")
                .help("Sets a custom config file"),
        )
        .arg(
            Arg::with_name("logfile")
                .long("logfile")
                .takes_value(true)
                .value_name("FILE")
                .help("Sets a custom log file"),
        )
        .arg(
            Arg::with_name("loglevel")
                .long("loglevel")
                .takes_value(true)
                .value_name("LEVEL")
                .default_value("warn")
                .possible_values(&["off", "error", "warn", "info", "debug", "trace"])
                .help("Sets the logging level"),
        )
        .get_matches();

    // Setup logging
    let log_config = LogConfig {
        time_format: Some("%Y-%m-%dT%H:%M:%S%.3f%:z"), // RFC3339 format
        offset: Local::today().offset().clone(),
        ..LogConfig::default()
    };
    let log_level = matches.value_of("loglevel").unwrap().parse()?;
    match matches.value_of("logfile") {
        Some(logfile) => {
            let log_file = File::create(logfile)?;
            WriteLogger::init(log_level, log_config, log_file)?
        }
        None => {
            let _ = TermLogger::init(log_level, log_config, TerminalMode::Mixed);
        }
    }

    // Read configuration file
    let config_file = matches.value_of("config").unwrap();
    info!("Reading configuration file: {}", &config_file);

    let config_str = fs::read_to_string(config_file)?;
    let config: Config = toml::from_str(&config_str)?;

    let devices = config.devices.into_devices();

    // Connect Modbus
    let modbus_config = config.modbus;
    let (modbus_hostname, modbus_config) = modbus_config.into_modbus_tcp_config();

    debug!("Connecting to {}", modbus_hostname);
    let mb = Transport::new_with_cfg(&modbus_hostname, modbus_config)?;
    let mb = &Mutex::new(mb);

    let influxdb_config = &config.influxdb;

    // Share one failure counter for all devices.
    // With each failed device communication the counter is increased.
    // With each successfull device communicationt the counter is decreased.
    // When the counter reaches the threshold (e.g. all devices on the bus failed
    // two times in a row) action is taken.
    let mut fail_count = 0;
    let scan_interval_iter = devices.iter().map(|d| d.scan_interval.as_nanos());
    let fail_count_threshold = 2
        * devices.len()
        * usize::try_from(
            scan_interval_iter.clone().max().unwrap() / scan_interval_iter.clone().min().unwrap(),
        )
        .unwrap();
    let (fail_tx, fail_rx) = mpsc::channel::<bool>();

    let mut stop_notification = Bus::new(1);

    crossbeam_thread::scope(|s| {
        for dev in devices {
            let fail_tx = fail_tx.clone();
            let mut stop_notification = stop_notification.add_rx();
            s.spawn(move |_| loop {
                match read_device(&dev, &mut *mb.lock().unwrap(), influxdb_config) {
                    Ok(_) => {
                        debug!("ModbusTCP: Device {} read successfully", dev.id);
                        fail_tx.send(false).unwrap();
                    }
                    Err(e) => {
                        warn!("ModbusTCP: {}", e);
                        fail_tx.send(true).unwrap();
                    }
                }

                // During normal operation the timeout is used as delay.
                // A stop notficiation breaks out of the loop and exits the thread.
                if stop_notification
                    .recv_timeout(dev.scan_interval)
                    .is_ok()
                {
                    break;
                }
            });
        }

        // Wait for fail counter threshold to be reached
        while fail_count < fail_count_threshold {
            if fail_rx.recv().unwrap() {
                fail_count += 1;
                debug!("fail_count={}", fail_count);
            } else if fail_count > 0 {
                fail_count -= 1;
                debug!("fail_count={}", fail_count);
            }
        }

        error!("{} modbus communication errors, exiting...", fail_count);
        stop_notification.broadcast(());
    })
    .unwrap();

    Ok(())
}

fn read_device(
    dev: &Device,
    mb: &mut Transport,
    influxdb_config: &InfluxDbConfig,
) -> Result<(), ModbusError> {
    let lines = dev.read(mb)?;

    let req = influxdb_config
        .to_request(lines)
        .expect("Failed to create InfluxDB http request");
    let resp = isahc::send(req);
    match resp {
        Ok(resp) => {
            if !resp.status().is_success() {
                warn!("InfluxDB: {:?}", resp);
            }
        }
        Err(e) => warn!("InfluxDB: {}", e),
    }

    Ok(())
}
