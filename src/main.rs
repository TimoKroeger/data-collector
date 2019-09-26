mod config;
mod device;

use std::cmp;
use std::fs::{self, File};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;

use crate::config::{Config, InfluxDbConfig};
use crate::device::Device;
use clap::{app_from_crate, crate_authors, crate_description, crate_name, crate_version, Arg};
use isahc;
use log::{debug, info, warn};
use modbus::{tcp::Transport, Error as ModbusError};
use simplelog::{Config as LogConfig, TermLogger, WriteLogger};

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
        time_format: Some("%+"),
        ..LogConfig::default()
    };
    let log_level = matches.value_of("loglevel").unwrap().parse()?;
    match matches.value_of("logfile") {
        Some(logfile) => {
            let log_file = File::create(logfile)?;
            WriteLogger::init(log_level, log_config, log_file)?
        }
        None => TermLogger::init(log_level, log_config)?,
    }

    // Read configuration file
    let config_file = matches.value_of("config").unwrap();
    info!("Reading configuration file: {}", &config_file);

    let config_str = fs::read_to_string(config_file)?;
    let config: Config = toml::from_str(&config_str)?;

    // Connect Modbus
    let modbus_config = config.modbus;
    let (modbus_hostname, modbus_config) = modbus_config.into_modbus_tcp_config();

    debug!("Connecting to {}", modbus_hostname);
    let mb = Transport::new_with_cfg(&modbus_hostname, modbus_config)?;

    let devices = config.devices.into_devices();

    // Wrappers for thread safe access of shared data
    let mb = Arc::new(Mutex::new(mb));
    let influxdb_config = Arc::new(config.influxdb);

    // Share one failure counter for all devices.
    // With each failed device communication the counter is increased.
    // With each successfull device communicationt the counter is decreased.
    // When the counter reaches the threshold (e.g. all devices on the bus failed
    // two times in a row) action is taken.
    let mut fail_count = 0isize;
    let fail_count_threshold = devices.len() as isize * 2;
    let (fail_count_tx, fail_count_rx) = mpsc::channel::<isize>();

    let running = Arc::new((Mutex::new(true), Condvar::new()));

    // Use one thread per device
    let mut threads = Vec::new();

    for dev in devices {
        let mb = mb.clone();
        let influxdb_config = influxdb_config.clone();
        let fail_count_tx = fail_count_tx.clone();
        let running = running.clone();
        threads.push(thread::spawn(move || loop {
            match read_device(&dev, &mut *mb.lock().unwrap(), &influxdb_config) {
                Ok(_) => fail_count_tx.send(-1).unwrap(),
                Err(e) => {
                    warn!("ModbusTCP: {}", e);
                    fail_count_tx.send(1).unwrap();
                }
            }

            // Block until scan interval or stop thread when receiving a notification.
            let &(ref lock, ref cvar) = &*running;
            let running = lock.lock().unwrap();
            let (running, _) = cvar.wait_timeout(running, dev.get_scan_interval()).unwrap();
            if !*running {
                break;
            }
        }));
    }

    // Wait for fail counter threshold to be reached
    while fail_count < fail_count_threshold {
        fail_count = cmp::max(0, fail_count + fail_count_rx.recv().unwrap());
        debug!("fail_count={}", fail_count);
    }

    // Stop all threads with a notification
    let &(ref lock, ref cvar) = &*running;
    {
        let mut running = lock.lock().unwrap();
        *running = false;
    }
    cvar.notify_one();

    // Wait for all devices to fail before exiting.
    for thread in threads {
        thread.join().unwrap();
    }

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
