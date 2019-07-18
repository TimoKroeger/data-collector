mod config;
mod sensor;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{app_from_crate, crate_authors, crate_description, crate_name, crate_version, Arg};
use config::*;
use humantime;
use log::{debug, error, info, warn};
use reqwest::{Client as HttpClient, RequestBuilder};
use sensor::Sensor;
use simplelog::{Config as LogConfig, TermLogger, WriteLogger};
use tokio_modbus::{client::sync::Context, prelude::*};
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortSettings, StopBits};

fn influxdb_line(
    measurement: &str,
    tags: &Vec<(&str, &str)>,
    value: u16,
    timestamp: u64,
) -> String {
    let escape_meas = |s: &str| s.replace(',', "\\,").replace(' ', "\\ ");
    let escape_tag = |s: &str| escape_meas(s).replace('=', "\\=");

    let mut line = escape_meas(measurement);
    for (k, v) in tags {
        line.push_str(&format!(",{}={}", escape_tag(k), escape_tag(v)));
    }
    line.push_str(&format!(" value={} {}\n", value, timestamp));
    line
}

fn get_influxdb_lines(sensor: &Sensor, register_values: &HashMap<u16, u16>) -> String {
    let mut lines = String::new();
    for (&reg_addr, &value) in register_values {
        let mut tags = Vec::new();

        tags.push(("group", sensor.group));

        let id_str = sensor.id.to_string();
        tags.push(("id", &id_str));

        let reg_str = reg_addr.to_string();
        tags.push(("register", &reg_str));

        for t in &sensor.tags {
            tags.push((&t.0, &t.1));
        }

        let line = influxdb_line(
            &sensor.registers.get_name(reg_addr),
            &tags,
            value,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );
        lines.push_str(&line);
    }
    lines
}

fn connection_task(
    mut mb: impl SyncReader,
    http_req: RequestBuilder,
    sensor_groups: &BTreeMap<String, SensorGroupConfig>,
) -> Result<(), Error> {
    // TODO: Support more than one sensor group
    let (group, sensor_group) = sensor_groups.iter().next().unwrap();

    let register_map = sensor_group
        .measurement_registers
        .clone()
        .into_register_map();

    let mut sensors = Vec::new();
    for sensor in &sensor_group.sensors {
        // Convert all tag values to strings
        let mut tags: Vec<(String, String)> = Vec::new();
        for (k, v) in &sensor.tags {
            tags.push((
                k.clone(),
                match v {
                    toml::Value::String(s) => s.clone(),
                    _ => v.to_string(),
                },
            ));
        }

        sensors.push(Sensor {
            id: sensor.id,
            group: &group,
            registers: &register_map,
            tags,
        });
    }

    loop {
        let mut lines = String::new();

        for sensor in &mut sensors {
            match sensor.read_registers(&mut mb) {
                Ok(register_values) => {
                    lines.push_str(&get_influxdb_lines(&sensor, &register_values))
                }
                Err(e) => warn!("ModbusTCP: Sensor {}: {}", sensor.id, e),
            }
        }

        if lines.is_empty() {
            return Err(Error::new(
                ErrorKind::NotConnected,
                "Error detected at each sensor",
            ));
        }

        let resp = http_req.try_clone().unwrap().body(lines).send();
        match resp {
            Ok(resp) => {
                if !resp.status().is_success() {
                    warn!("InfluxDB Response: {:?}", resp);
                }
            }
            Err(e) => warn!("InfluxDB: {}", e),
        }

        thread::sleep(sensor_group.scan_interval);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error + 'static>> {
    // Parse command line arguments
    let matches = app_from_crate!()
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .takes_value(true)
                .value_name("FILE")
                .default_value("datacollector.toml")
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
    let mut log_config = LogConfig::default();
    log_config.time_format = Some("%+");
    let log_level = matches.value_of("loglevel").unwrap().parse()?;
    match matches.value_of("logfile") {
        Some(logfile) => {
            let log_file = File::create(logfile)?;
            WriteLogger::init(log_level, log_config, log_file)?
        }
        None => TermLogger::init(log_level, log_config)?,
    }

    let config_file = matches.value_of("config").unwrap();
    let config = Config::new(&config_file);
    info!(
        "Configuration loaded from {} with {} sensor groups",
        config_file,
        config.sensor_groups.len()
    );

    let connect_fn: Box<dyn Fn() -> Result<Context, Error>> =
        if let Some(mb_tcp_cfg) = config.modbus {
            let modbus_hostname = format!("{}:{}", mb_tcp_cfg.hostname, mb_tcp_cfg.port);
            let modbus_hostaddr = modbus_hostname.parse().unwrap();
            // TODO: Use the timeout value from the configuration file.

            Box::new(move || {
                debug!("ModbusTCP: Connecting to {}", modbus_hostname);
                sync::tcp::connect(modbus_hostaddr).map(|c| {
                    info!("ModbusTCP: Successfully connected to {}", modbus_hostname);
                    c
                })
            })
        } else if let Some(mb_rtu_cfg) = config.modbus_rtu {
            let serial_config = SerialPortSettings {
                baud_rate: 19200,
                data_bits: DataBits::Eight,
                flow_control: FlowControl::None,
                parity: Parity::Even,
                stop_bits: StopBits::One,
                timeout: Duration::from_millis(200),
            };
            Box::new(move || {
                debug!("ModbusRTU: Connecting to {}", mb_rtu_cfg.port);
                sync::rtu::connect(&mb_rtu_cfg.port, &serial_config).map(|c| {
                    info!("ModbusRTU: Successfully connected to {}", mb_rtu_cfg.port);
                    c
                })
            })
        } else {
            panic!("No modbus configuration found!");
        };

    let client = HttpClient::new();
    let req = if let Some(influx) = config.influxdb {
        let req = client
            .post(&format!("{}/write", influx.hostname))
            .query(&[("db", influx.database)]);
        match (influx.username, influx.password) {
            (Some(username), Some(password)) => req.query(&[("u", username), ("p", password)]),
            (_, _) => req,
        }
    } else if let Some(influx2) = config.influxdb2 {
        client
            .post(&format!("{}/write", influx2.hostname))
            .query(&[("org", influx2.organization), ("bucket", influx2.bucket)])
            .header("Authorization", format!("Token {}", influx2.auth_token))
    } else {
        panic!("No influxdb configuration found!");
    };
    let req = req.query(&[("precision", "s")]);

    // Retry to connect forever
    loop {
        let e = match connect_fn() {
            Ok(mb) => {
                connection_task(mb, req.try_clone().unwrap(), &config.sensor_groups).unwrap_err()
            }
            Err(e) => e,
        };

        // TODO: Exponential backoff
        let delay = Duration::from_secs(10);
        error!(
            "ModbusTCP: {}, reconnecting in {}...",
            e,
            humantime::format_duration(delay)
        );
        thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sensor::RegisterMap;

    #[test]
    fn test_get_influxdb_lines() {
        let mut reg_map = BTreeMap::new();
        reg_map.insert(0, String::from("reg0"));
        reg_map.insert(1, String::from("reg1"));
        let reg_map = RegisterMap::new(reg_map);

        let tags = vec![
            (String::from("tag1"), String::from("value1")),
            (String::from("tag2"), String::from("value2")),
        ];

        let sensor = Sensor {
            id: 1,
            group: "mygroup",
            registers: &reg_map,
            tags,
        };

        let mut reg_values = HashMap::new();
        reg_values.insert(0, 100);
        reg_values.insert(1, 101);

        let lines = get_influxdb_lines(&sensor, &reg_values);
        print!("{}", lines);
    }
}
