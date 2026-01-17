use axum::{Router, http::StatusCode, routing::get};
use config::{Config, Environment, File};
use encoding_rs::mem::decode_latin1;
use env_logger::{Env, TimestampPrecision};
use log::{debug, error, info, warn};
use mikrotik_api::prelude::TrapCategory;
use mikrotik_model::{
    MacAddress, MikrotikDevice,
    ascii::AsciiString,
    model::{
        CapsManInterfaceById, CapsManInterfaceState, CapsManRadioState,
        CapsManRegistrationTableState, InterfaceEthernet, InterfaceEthernetPoeMonitorState,
        InterfaceWifiByName, InterfaceWifiRadioState, InterfaceWifiRegistrationTableState,
        InterfaceWifiState, IpDhcpServerLease, ResourceType, SystemHealthState, SystemIdentityCfg,
    },
    resource::{DeserializeRosResource, SentenceResult, SingleResource, stream_resource},
    value::{RosValue, StatsPair},
};
use prometheus::{
    Encoder, TextEncoder,
    proto::{Counter, LabelPair, Metric, MetricFamily},
};
use serde::Deserialize;
use std::{
    borrow::Cow,
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    process,
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_stream::StreamExt;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Device {
    #[allow(dead_code)]
    name: Box<str>,
    address: IpAddr,
    user: Box<str>,
    password: Box<str>,
}
struct MetricsCollection {
    reg_tx_bytes: MetricFamily,
    reg_rx_bytes: MetricFamily,
    reg_tx_packets: MetricFamily,
    reg_rx_packets: MetricFamily,
    reg_rx_signal: MetricFamily,
    reg_uptime: MetricFamily,
    health_temperature: MetricFamily,
    health_voltage: MetricFamily,
    health_current: MetricFamily,
    health_power: MetricFamily,
    health_rpm: MetricFamily,
    ethernet_poe_out_voltage: MetricFamily,
    ethernet_poe_out_current: MetricFamily,
    ethernet_poe_out_power: MetricFamily,
}
impl Default for MetricsCollection {
    fn default() -> Self {
        Self {
            reg_tx_bytes: create_metric("mikrotik_exporter_wlan_registration_tx-bytes"),
            reg_rx_bytes: create_metric("mikrotik_exporter_wlan_registration_rx-bytes"),
            reg_tx_packets: create_metric("mikrotik_exporter_wlan_registration_tx-packets"),
            reg_rx_packets: create_metric("mikrotik_exporter_wlan_registration_rx-packets"),
            reg_rx_signal: create_metric("mikrotik_exporter_wlan_registration_rx-signal"),
            reg_uptime: create_metric("mikrotik_exporter_wlan_registration_uptime"),
            health_temperature: create_metric("mikrotik_exporter_health_temperature"),
            health_voltage: create_metric("mikrotik_exporter_health_voltage"),
            health_current: create_metric("mikrotik_exporter_health_current"),
            health_power: create_metric("mikrotik_exporter_health_power"),
            health_rpm: create_metric("mikrotik_exporter_health_rpm"),
            ethernet_poe_out_voltage: create_metric("mikrotik_exporter_ethernet_poe_out_voltage"),
            ethernet_poe_out_current: create_metric("mikrotik_exporter_ethernet_poe_out_current"),
            ethernet_poe_out_power: create_metric("mikrotik_exporter_ethernet_poe_out_power"),
        }
    }
}
impl MetricsCollection {
    fn collect(self) -> Box<[MetricFamily]> {
        [
            self.reg_tx_bytes,
            self.reg_rx_bytes,
            self.reg_tx_packets,
            self.reg_rx_packets,
            self.reg_rx_signal,
            self.reg_uptime,
            self.health_temperature,
            self.health_voltage,
            self.health_current,
            self.health_power,
            self.health_rpm,
            self.ethernet_poe_out_voltage,
            self.ethernet_poe_out_current,
            self.ethernet_poe_out_power,
        ]
        .into_iter()
        .filter(|f| !f.metric.is_empty())
        .collect::<Vec<_>>()
        .into_boxed_slice()
    }
}
struct IpLease {
    hostname: Box<str>,
    ip: IpAddr,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .parse_env(Env::default().filter_or("LOG_LEVEL", "info"))
        .format_timestamp(Some(TimestampPrecision::Millis))
        .init();

    let cfg = Config::builder()
        .add_source(File::with_name("config.yaml"))
        .add_source(
            Environment::with_prefix("APP")
                .separator("-")
                .prefix_separator("_"),
        )
        .build()?;

    info!("Loaded config: {:?}", cfg);
    let devs = cfg.get_array("devices").expect("No devices configured");
    info!("Devices: {:?}", devs);
    for x in devs {
        let map = x.into_table().expect("Device is not a table");
        let address = map.get("address").expect("No address configured");
        info!("Device address: {}", address);
        let user = map.get("user").expect("No user configured");
        info!("Device user: {}", user);
    }

    let port = cfg.get("port").ok().unwrap_or(8080);
    let address = SocketAddr::new(IpAddr::from([0; 8]), port);
    let listener = TcpListener::bind(address)
        .await
        .expect("Cannot bind socket");

    let app = Router::new()
        .route(
            "/metrics",
            get(async move || match metrics(cfg.clone()).await {
                Ok(metrics) => (StatusCode::OK, metrics),
                Err(e) => {
                    error!("Error collecting metrics: {:?}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("{:?}", e).into_bytes(),
                    )
                }
            }),
        )
        .route("/health", get(health));

    tokio::spawn(async move {
        info!("Starting server on {}", address);
        match axum::serve(listener, app.into_make_service()).await {
            Ok(_) => {}
            Err(e) => {
                error!("Cannot start server: {}", e);
                process::exit(1);
            }
        }
    });

    info!("Waiting for ctrl-c...");
    match tokio::signal::ctrl_c().await {
        Ok(_) => {
            info!("Received ctrl-c, exiting");
        }
        Err(e) => {
            error!("Cannot listen for ctrl-c: {}", e);
            process::exit(1);
        }
    }
    Ok(())
}
async fn metrics(cfg: Config) -> anyhow::Result<Vec<u8>> {
    // Gather the metrics.
    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    let metric_families = collect_metrics(cfg).await?.collect();
    encoder.encode(&metric_families, &mut buffer)?;
    Ok(buffer)
}
async fn health() -> &'static str {
    "OK\n"
}

async fn collect_metrics(cfg: Config) -> anyhow::Result<MetricsCollection> {
    info!("Starting metrics collection");
    info!("cfg: {:?}", cfg);
    //let devices: Vec<Device> = cfg.get("devices")?;
    let devs = cfg.get_array("devices").expect("No devices configured");

    //info!("Devices: {:?}", devices);
    let mut metrics_collection = MetricsCollection::default();
    let mut connected_devices = Vec::with_capacity(devs.len());
    for device_cfg in devs {
        let mut device_table = device_cfg.into_table().expect("Device is not a table");
        let address = device_table
            .remove("address")
            .expect("No address configured")
            .into_string()
            .expect("Address is not a string");
        let user = device_table
            .remove("user")
            .expect("No user configured")
            .into_string()
            .expect("User is not a string");
        let password = device_table
            .remove("password")
            .expect("No password configured")
            .into_string()
            .expect("Password is not a string");
        match MikrotikDevice::connect(
            (address.clone(), 8728),
            user.as_bytes(),
            Some(password.as_bytes()),
        )
        .await
        {
            Ok(device) => {
                connected_devices.push(device);
            }
            Err(error) => {
                error!("Cannot connect to device: {address}, {error}");
            }
        }
    }

    let mut ip_leases = HashMap::<MacAddress, IpLease>::new();
    for device in connected_devices.iter() {
        collect_ip_leases(&mut ip_leases, device).await;
    }

    for device in connected_devices {
        let identity = SystemIdentityCfg::fetch(&device).await?;
        let identity = identity.as_ref().map(|i| &i.name).map(<&AsciiString>::into);
        let identity = identity.as_ref().map(Cow::as_ref).unwrap_or_default();
        collect_capsman_metric(&mut metrics_collection, &device, identity, &ip_leases).await;
        collect_wifi_metric(&mut metrics_collection, &device, identity, &ip_leases).await;
        collect_health_metric(&mut metrics_collection, &device, identity).await;
        collect_ethernet_metric(&mut metrics_collection, &device, identity).await;
    }
    Ok(metrics_collection)
}

async fn collect_ip_leases(ip_leases: &mut HashMap<MacAddress, IpLease>, device: &MikrotikDevice) {
    info!("============== IP Leases ==============");
    let mut lease_stream = stream_resource::<IpDhcpServerLease>(device)
        .await
        .filter_map(log_problems);
    while let Some(lease) = lease_stream.next().await {
        if let Some(mac_address) = lease.status.active_mac_address
            && let Some(hostname) = lease.status.host_name
        {
            ip_leases.insert(
                mac_address,
                IpLease {
                    hostname: hostname.to_string().into_boxed_str(),
                    ip: lease.cfg.address,
                },
            );
        }
    }
}

async fn collect_ethernet_metric(
    metrics_collection: &mut MetricsCollection,
    device: &MikrotikDevice,
    identity: &str,
) {
    info!("============== Ethernet ==============");
    let mut ethernet_stream = stream_resource::<InterfaceEthernet>(device)
        .await
        .filter_map(log_problems);
    let mut id_list = Vec::<u8>::new();
    let mut poe_count = 0;
    while let Some(value) = ethernet_stream.next().await {
        if value.cfg.poe_out.is_some() {
            if !id_list.is_empty() {
                id_list.extend(b",");
            }
            id_list.extend(value.status.id.encode_ros().as_ref());
            poe_count += 1;
        }
    }
    if !id_list.is_empty() {
        info!("============== PoE Monitor ==============");
        let cmd: [&[u8]; _] = [b"/", b"interface/ethernet/poe", b"/monitor"];

        let mut stream = device
            .send_command(
                &cmd,
                |cb| {
                    cb.attribute(b".id", id_list.as_slice())
                        .attribute(b"duration", b"0.5")
                        .attribute(b"interval", b"0.5")
                },
                ResourceType::InterfaceEthernetPoeMonitorState,
            )
            .await
            .take(poe_count)
            .map(|e| e.map(|r| InterfaceEthernetPoeMonitorState::unwrap_resource(r)))
            .filter_map(log_problems);
        while let Some(value) = stream.next().await {
            if let Some(value) = value {
                let labels = vec![
                    create_label_pair("hostname", identity.to_string()),
                    create_label_pair("interface", value.name.to_string()),
                ];
                if let Some(voltage) = value.poe_out_voltage {
                    metrics_collection
                        .ethernet_poe_out_voltage
                        .metric
                        .push(create_metric_value(&labels, voltage));
                }
                if let Some(current) = value.poe_out_current {
                    metrics_collection
                        .ethernet_poe_out_current
                        .metric
                        .push(create_metric_value(&labels, current));
                }
                if let Some(power) = value.poe_out_power {
                    metrics_collection
                        .ethernet_poe_out_power
                        .metric
                        .push(create_metric_value(&labels, power));
                }
            }
        }
    }
}

async fn collect_health_metric(
    metrics_collection: &mut MetricsCollection,
    device: &MikrotikDevice,
    identity: &str,
) {
    info!("============== Health ==============");
    let mut health_value_stream = stream_resource::<SystemHealthState>(device)
        .await
        .filter_map(log_problems);
    while let Some(value) = health_value_stream.next().await {
        let labels = vec![
            create_label_pair("hostname", identity.to_string()),
            create_label_pair("name", value.name.to_string()),
        ];
        match (
            value._type.0.as_ref(),
            value.value.to_string().parse::<f64>(),
        ) {
            (b"W", Ok(value)) => metrics_collection
                .health_power
                .metric
                .push(create_metric_value(&labels, value)),
            (b"C", Ok(value)) => metrics_collection
                .health_temperature
                .metric
                .push(create_metric_value(&labels, value)),
            (b"V", Ok(value)) => metrics_collection
                .health_voltage
                .metric
                .push(create_metric_value(&labels, value)),
            (b"RPM", Ok(value)) => metrics_collection
                .health_rpm
                .metric
                .push(create_metric_value(&labels, value)),
            (b"A", Ok(value)) => metrics_collection
                .health_current
                .metric
                .push(create_metric_value(&labels, value)),
            (b"", _) => {
                // ignore states
            }
            (field, Ok(value)) => {
                warn!("Unknown health state: {}={value}", decode_latin1(field));
            }
            (field, Err(e)) => {
                warn!("Cannot parse health state: {}:{e}", decode_latin1(field));
            }
        }
    }
}

async fn collect_wifi_metric(
    metrics_collection: &mut MetricsCollection,
    device: &MikrotikDevice,
    identity: &str,
    ip_leases: &HashMap<MacAddress, IpLease>,
) {
    info!("============== Wifi Radios ==============");
    let mut radio_stream = stream_resource::<InterfaceWifiRadioState>(&device)
        .await
        .filter_map(log_problems);
    let mut radios = HashMap::new();
    while let Some(radio) = radio_stream.next().await {
        radios.insert(radio.interface.clone(), radio);
    }
    if radios.is_empty() {
        return;
    }
    info!("============== Wifi Interfaces ==============");
    let mut interface_stream =
        stream_resource::<(InterfaceWifiByName, InterfaceWifiState)>(&device)
            .await
            .filter_map(log_problems);
    let mut interfaces = HashMap::new();
    while let Some((InterfaceWifiByName(cfg), state)) = interface_stream.next().await {
        interfaces.insert(cfg.name.clone(), (cfg, state));
    }
    info!("============== Wifi Registrations ==============");
    let mut registration_stream = stream_resource::<InterfaceWifiRegistrationTableState>(&device)
        .await
        .filter_map(log_problems);
    while let Some(value) = registration_stream.next().await {
        let if_name = &value.interface;
        let if_data = interfaces.get(if_name);
        let master_interface = if_data
            .and_then(|(i, _)| i.master_interface.as_ref())
            .unwrap_or(if_name);
        let cap_identity = radios
            .get(&master_interface)
            .and_then(|mi| mi.cap.as_ref())
            .and_then(|cap| cap.split(|ch| *ch == b'@').next())
            .map(|cap| decode_latin1(cap));
        let cap_identity: &str = cap_identity.as_ref().map(Cow::as_ref).unwrap_or_default();
        let label = create_caps_labels(
            identity,
            cap_identity,
            value.mac_address,
            value.ssid,
            "wifi",
            ip_leases,
        );
        write_caps_metrics(
            metrics_collection,
            label,
            value.bytes,
            value.packets,
            value.signal,
            value.uptime,
        );
    }
}

async fn collect_capsman_metric(
    metrics_collection: &mut MetricsCollection,
    device: &MikrotikDevice,
    identity: &str,
    ip_leases: &HashMap<MacAddress, IpLease>,
) {
    info!("============== Capsman Radio ==============");
    let mut cap_identities_by_radio = HashMap::new();
    let mut radio_stream = stream_resource::<CapsManRadioState>(&device)
        .await
        .filter_map(log_problems);
    while let Some(radio) = radio_stream.next().await {
        cap_identities_by_radio.insert(radio.interface.clone(), radio);
    }
    if cap_identities_by_radio.is_empty() {
        return;
    }
    let mut configuration_by_interface = HashMap::new();
    info!("============== Capsman Interfaces ==============");
    let mut interface_stream =
        stream_resource::<(CapsManInterfaceById, CapsManInterfaceState)>(&device)
            .await
            .filter_map(log_problems);
    while let Some((cfg, state)) = interface_stream.next().await {
        //info!("Found Interface: {:?}, {:?}", cfg,state);
        let cfg = cfg.data;
        configuration_by_interface.insert(cfg.name.clone(), (cfg, state));
    }

    info!("============== Capsman Registrations ==============");

    let mut registration_table = stream_resource::<CapsManRegistrationTableState>(&device)
        .await
        .filter_map(log_problems);
    while let Some(value) = registration_table.next().await {
        let if_name = &value.interface;
        let if_data = configuration_by_interface.get(if_name);
        let master_interface = if_data
            .and_then(|(i, _)| i.master_interface.as_ref())
            .and_then(|n| n.value())
            .unwrap_or(if_name);
        let cap_identity_option = cap_identities_by_radio
            .get(master_interface)
            .map(|i| &i.remote_cap_identity)
            .map(<&AsciiString>::into);
        let cap_identity: &str = cap_identity_option
            .as_ref()
            .map(Cow::as_ref)
            .unwrap_or_default();
        let label = create_caps_labels(
            identity,
            cap_identity,
            value.mac_address,
            value.ssid,
            "legacy",
            ip_leases,
        );
        write_caps_metrics(
            metrics_collection,
            label,
            value.bytes,
            value.packets,
            value.rx_signal,
            value.uptime,
        );
    }
}

fn write_caps_metrics(
    metrics_collection: &mut MetricsCollection,
    label: Vec<LabelPair>,
    bytes_pair: StatsPair<u64>,
    packets_pair: StatsPair<u64>,
    signal: i16,
    uptime: Duration,
) {
    metrics_collection
        .reg_tx_bytes
        .metric
        .push(create_metric_value(&label, bytes_pair.tx as f64));
    metrics_collection
        .reg_rx_bytes
        .metric
        .push(create_metric_value(&label, bytes_pair.rx as f64));
    metrics_collection
        .reg_tx_packets
        .metric
        .push(create_metric_value(&label, packets_pair.tx as f64));
    metrics_collection
        .reg_rx_packets
        .metric
        .push(create_metric_value(&label, packets_pair.rx as f64));
    metrics_collection
        .reg_rx_signal
        .metric
        .push(create_metric_value(&label, signal as f64));
    metrics_collection
        .reg_uptime
        .metric
        .push(create_metric_value(&label, uptime.as_secs_f64()));
}

fn create_metric_value(label: &Vec<LabelPair>, x: f64) -> Metric {
    Metric {
        label: label.clone(),
        counter: protobuf::MessageField::some(Counter {
            value: Some(x),
            special_fields: Default::default(),
        }),
        ..Metric::default()
    }
}

fn create_caps_labels(
    identity: &str,
    cap_identity: &str,
    mac_address: MacAddress,
    ssid: AsciiString,
    caps: &str,
    ip_leases: &HashMap<MacAddress, IpLease>,
) -> Vec<LabelPair> {
    vec![
        create_label_pair("mac_address", mac_address.to_string()),
        create_label_pair("hostname", identity),
        create_label_pair(
            "client_hostname",
            ip_leases
                .get(&mac_address)
                .map(|lease| lease.hostname.clone())
                .unwrap_or_default(),
        ),
        create_label_pair(
            "client_ip",
            ip_leases
                .get(&mac_address)
                .map(|lease| lease.ip.to_string())
                .unwrap_or_default(),
        ),
        create_label_pair("cap_identity", cap_identity),
        create_label_pair("ssid", ssid.to_string()),
        create_label_pair("caps", caps),
    ]
}
fn create_label_pair(name: impl Into<String>, value: impl Into<String>) -> LabelPair {
    LabelPair {
        name: Some(name.into()),
        value: Some(value.into()),
        ..LabelPair::default()
    }
}

fn create_metric(name: &str) -> MetricFamily {
    let mut tx_counter = MetricFamily::new();
    tx_counter.name = Some(name.to_string());
    tx_counter
}

fn log_problems<E>(entry: SentenceResult<E>) -> Option<E> {
    match entry {
        SentenceResult::Row { value, warnings } => {
            warnings.iter().for_each(|w| warn!("Warning: {w}"));
            Some(value)
        }

        SentenceResult::Error { errors, warnings } => {
            warnings.iter().for_each(|w| warn!("Warning: {w}"));
            for error in errors.iter() {
                error!("Error: {error}");
            }
            None
        }
        SentenceResult::Trap { category, message } => {
            if category.is_none() && message.as_ref() == b"no such command prefix" {
                debug!("Trap: {category:?}: {}", decode_latin1(message.as_ref()));
            } else if let Some(TrapCategory::MissingItemOrCommand) = category {
                debug!("Trap: {category:?}: {}", decode_latin1(message.as_ref()));
            } else {
                error!("Trap: {category:?}: {}", decode_latin1(message.as_ref()));
            }
            None
        }
    }
}
