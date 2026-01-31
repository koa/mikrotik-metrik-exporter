use axum::{
    Router,
    http::{StatusCode, header},
    routing::get,
};
use config::{Config, Environment, File};
use encoding_rs::mem::decode_latin1;
use env_logger::{Env, TimestampPrecision};
use log::{debug, error, info, warn};
use mikrotik_model::{
    MacAddress, MikrotikDevice,
    ascii::AsciiString,
    mikrotik_api::TrapCategory,
    model::{
        CapsManInterfaceById, CapsManInterfaceState, CapsManRadioState,
        CapsManRegistrationTableState, InterfaceEthernet, InterfaceEthernetPoeMonitor,
        InterfaceWifi, InterfaceWifiRadioState, InterfaceWifiRegistrationTableState,
        IpDhcpServerLease, SystemHealthState, SystemIdentityCfg,
    },
    resource::{SentenceResult, SingleResource, monitor_once_resource, stream_resource},
    value::StatsPair,
};
use prometheus::{
    Encoder, TEXT_FORMAT, TextEncoder,
    proto::{Counter, LabelPair, Metric, MetricFamily},
};
use serde::Deserialize;
use std::{
    borrow::Cow,
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    ops::Add,
    process,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    pin, spawn,
    sync::mpsc::{self, Sender},
    time::{Instant, timeout_at},
};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};

#[derive(Debug, Deserialize)]
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
#[derive(Debug, Copy, Clone, PartialEq)]
enum MetricKey {
    Wlan(WlanKey),
    Health(HealthKey),
    Poe(PoeKey),
}
#[derive(Debug, Copy, Clone, PartialEq)]
enum WlanKey {
    TxBytes,
    RxBytes,
    TxPackets,
    RxPackets,
    RxSignal,
    Uptime,
}
#[derive(Debug, Copy, Clone, PartialEq)]
enum HealthKey {
    Temperature,
    Voltage,
    Current,
    Power,
    Rpm,
}
#[derive(Debug, Copy, Clone, PartialEq)]
enum PoeKey {
    Voltage,
    Current,
    Power,
}
impl Default for MetricsCollection {
    fn default() -> Self {
        Self {
            reg_tx_bytes: create_metric("mikrotik_exporter_wlan_registration_tx_bytes"),
            reg_rx_bytes: create_metric("mikrotik_exporter_wlan_registration_rx_bytes"),
            reg_tx_packets: create_metric("mikrotik_exporter_wlan_registration_tx_packets"),
            reg_rx_packets: create_metric("mikrotik_exporter_wlan_registration_rx_packets"),
            reg_rx_signal: create_metric("mikrotik_exporter_wlan_registration_rx_signal"),
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

    let devices: Vec<Device> = cfg.get("devices")?;
    debug!("Devices: {:?}", devices);

    let port = cfg.get("port").ok().unwrap_or(8080);
    let address = SocketAddr::new(IpAddr::from([0; 8]), port);
    let listener = TcpListener::bind(address)
        .await
        .expect("Cannot bind socket");

    let app = Router::new()
        .route(
            "/metrics",
            get(async move || match metrics(cfg.clone()).await {
                Ok(metrics) => (StatusCode::OK, metrics.0, metrics.1),
                Err(e) => {
                    error!("Error collecting metrics: {:?}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [(header::CONTENT_TYPE, "text/plain")],
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
async fn metrics(
    cfg: Config,
) -> anyhow::Result<([(header::HeaderName, &'static str); 1], Vec<u8>)> {
    // Gather the metrics.
    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    let metric_families = collect_metrics(cfg).await?.collect();
    encoder.encode(&metric_families, &mut buffer)?;
    Ok(([(header::CONTENT_TYPE, TEXT_FORMAT)], buffer))
}
async fn health() -> &'static str {
    "OK\n"
}

const DHCP_TIMEOUT: Duration = Duration::from_secs(2);
const COLLECT_TIMEOUT: Duration = Duration::from_secs(8 + 2);

async fn collect_metrics(cfg: Config) -> anyhow::Result<MetricsCollection> {
    let start_time = Instant::now();
    let devices: Vec<Device> = cfg.get("devices")?;
    let mut connected_devices = Vec::with_capacity(devices.len());
    for device_cfg in devices {
        let address = device_cfg.address;
        match MikrotikDevice::connect(
            (address, 8728),
            device_cfg.user.as_bytes(),
            Some(device_cfg.password.as_bytes()),
        )
        .await
        {
            Ok(device) => {
                connected_devices.push((device, device_cfg.name.clone()));
            }
            Err(error) => {
                error!("Cannot connect to device: {address}, {error}");
            }
        }
    }

    let (tx, rx) = mpsc::channel(20);
    for (device, name) in connected_devices.iter().cloned() {
        let sender = tx.clone();
        spawn(async move {
            match timeout_at(
                start_time.add(DHCP_TIMEOUT),
                collect_ip_leases(sender, &device),
            )
            .await
            {
                Ok(_) => {}
                Err(_) => {
                    error!("Timeout collecting IP Leases from device {name}");
                }
            }
        });
    }
    drop(tx);
    let stream = ReceiverStream::new(rx);
    pin!(stream);
    let mut ip_leases = HashMap::new();
    while let Some((mac_address, lease)) = stream.next().await {
        ip_leases.insert(mac_address, lease);
    }
    info!("Finished collecting IP Leases: {}", ip_leases.len());

    let (tx, rx) = mpsc::channel(20);
    let ip_leases = Arc::new(ip_leases);

    for (device, name) in connected_devices {
        let tx = tx.clone();
        let ip_leases = ip_leases.clone();
        spawn(async move {
            match timeout_at(
                start_time.add(COLLECT_TIMEOUT),
                process_device_metrics(&device, &tx, &ip_leases),
            )
            .await
            {
                Ok(_) => {}
                Err(_) => {
                    error!("Timeout collecting Metrics from device {name}");
                }
            }
        });
    }
    drop(tx);
    let stream = ReceiverStream::new(rx)
        .map(Some)
        .merge(send_at(start_time.add(COLLECT_TIMEOUT)).map(|_| None));
    pin!(stream);
    let mut metrics_collection = MetricsCollection::default();
    while let Some(Some((key, metric))) = stream.next().await {
        match key {
            MetricKey::Wlan(WlanKey::RxBytes) => {
                metrics_collection.reg_rx_bytes.metric.push(metric);
            }
            MetricKey::Wlan(WlanKey::TxBytes) => {
                metrics_collection.reg_tx_bytes.metric.push(metric);
            }
            MetricKey::Wlan(WlanKey::TxPackets) => {
                metrics_collection.reg_tx_packets.metric.push(metric);
            }
            MetricKey::Wlan(WlanKey::RxPackets) => {
                metrics_collection.reg_rx_packets.metric.push(metric);
            }
            MetricKey::Wlan(WlanKey::RxSignal) => {
                metrics_collection.reg_rx_signal.metric.push(metric);
            }
            MetricKey::Wlan(WlanKey::Uptime) => {
                metrics_collection.reg_uptime.metric.push(metric);
            }
            MetricKey::Health(HealthKey::Temperature) => {
                metrics_collection.health_temperature.metric.push(metric);
            }
            MetricKey::Health(HealthKey::Voltage) => {
                metrics_collection.health_voltage.metric.push(metric);
            }
            MetricKey::Health(HealthKey::Current) => {
                metrics_collection.health_current.metric.push(metric);
            }
            MetricKey::Health(HealthKey::Power) => {
                metrics_collection.health_power.metric.push(metric);
            }
            MetricKey::Health(HealthKey::Rpm) => {
                metrics_collection.health_rpm.metric.push(metric);
            }
            MetricKey::Poe(PoeKey::Current) => {
                metrics_collection
                    .ethernet_poe_out_current
                    .metric
                    .push(metric);
            }
            MetricKey::Poe(PoeKey::Voltage) => {
                metrics_collection
                    .ethernet_poe_out_voltage
                    .metric
                    .push(metric);
            }
            MetricKey::Poe(PoeKey::Power) => {
                metrics_collection
                    .ethernet_poe_out_power
                    .metric
                    .push(metric);
            }
        }
    }
    Ok(metrics_collection)
}

async fn process_device_metrics(
    device: &MikrotikDevice,
    result_sender: &Sender<(MetricKey, Metric)>,
    ip_leases: &Arc<HashMap<MacAddress, IpLease>>,
) {
    let identity= match SystemIdentityCfg::fetch(&device).await {
        Ok(id) => id,
        Err(e) => {
            error!("Error fetching identity for device: {e}");
            return;
        }
    };
    let identity = identity.as_ref().map(|i| &i.name).map(<&AsciiString>::into);
    let identity = identity.as_ref().map(Cow::as_ref).unwrap_or_default();
    match collect_device_data(&ip_leases, &device, &result_sender, identity).await {
        Ok(_) => {}
        Err(e) => {
            error!("Error collecting metrics from device {identity}: {e}");
        }
    }
}

fn send_at(deadline: Instant) -> ReceiverStream<()> {
    let (tx, rx) = mpsc::channel::<()>(1);
    spawn(async move {
        tokio::time::sleep_until(deadline).await;
        match tx.send(()).await {
            Ok(_) => {}
            Err(_) => {}
        }
    });
    ReceiverStream::new(rx)
}

async fn collect_device_data(
    ip_leases: &HashMap<MacAddress, IpLease>,
    device: &MikrotikDevice,
    tx: &Sender<(MetricKey, Metric)>,
    identity: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    collect_capsman_metric(&tx, &device, identity, &ip_leases).await?;
    collect_wifi_metric(&tx, &device, identity, &ip_leases).await?;
    collect_health_metric(&tx, &device, identity).await?;
    collect_ethernet_metric(&tx, &device, identity).await?;
    Ok(())
}

async fn collect_ip_leases(ip_leases: Sender<(MacAddress, IpLease)>, device: &MikrotikDevice) {
    let mut lease_stream = stream_resource::<IpDhcpServerLease>(device)
        .await
        .filter_map(log_problems("", "IP Leases"));
    while let Some(lease) = lease_stream.next().await {
        if let Some(mac_address) = lease.status.active_mac_address
            && let Some(hostname) = lease.status.host_name
        {
            ip_leases
                .send((
                    mac_address,
                    IpLease {
                        hostname: hostname.to_string().into_boxed_str(),
                        ip: lease.cfg.address,
                    },
                ))
                .await
                .unwrap();
        }
    }
}

async fn collect_ethernet_metric(
    metrics_collection: &Sender<(MetricKey, Metric)>,
    device: &MikrotikDevice,
    identity: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let ethernet_stream = stream_resource::<InterfaceEthernet>(device)
        .await
        .filter_map(log_problems(identity, "Ethernet"));
    let ids = ethernet_stream
        .filter(|e| e.cfg.poe_out.is_some())
        .map(|e| e.status.id)
        .collect::<Vec<_>>()
        .await;
    let mut stream = monitor_once_resource::<InterfaceEthernetPoeMonitor>(ids.into_iter(), device)
        .await
        .filter_map(log_problems(identity, "PoE Monitor"));
    while let Some(value) = stream.next().await {
        let labels = vec![
            create_label_pair("hostname", identity.to_string()),
            create_label_pair("interface", value.name.to_string()),
        ];
        if let Some(voltage) = value.poe_out_voltage {
            metrics_collection
                .send((
                    MetricKey::Poe(PoeKey::Voltage),
                    create_metric_value(&labels, voltage),
                ))
                .await?;
        }
        if let Some(current) = value.poe_out_current {
            metrics_collection
                .send((
                    MetricKey::Poe(PoeKey::Current),
                    create_metric_value(&labels, current),
                ))
                .await?;
        }
        if let Some(power) = value.poe_out_power {
            metrics_collection
                .send((
                    MetricKey::Poe(PoeKey::Power),
                    create_metric_value(&labels, power),
                ))
                .await?;
        }
    }
    Ok(())
}

async fn collect_health_metric(
    metrics_collection: &Sender<(MetricKey, Metric)>,
    device: &MikrotikDevice,
    identity: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut health_value_stream = stream_resource::<SystemHealthState>(device)
        .await
        .filter_map(log_problems(identity, "Health"));
    while let Some(value) = health_value_stream.next().await {
        let labels = vec![
            create_label_pair("hostname", identity.to_string()),
            create_label_pair("name", value.name.to_string()),
        ];
        match (
            value._type.0.as_ref(),
            value.value.to_string().parse::<f64>(),
        ) {
            (b"W", Ok(value)) => {
                metrics_collection
                    .send((
                        MetricKey::Health(HealthKey::Power),
                        create_metric_value(&labels, value),
                    ))
                    .await?
            }
            (b"C", Ok(value)) => {
                metrics_collection
                    .send((
                        MetricKey::Health(HealthKey::Temperature),
                        create_metric_value(&labels, value),
                    ))
                    .await?
            }
            (b"V", Ok(value)) => {
                metrics_collection
                    .send((
                        MetricKey::Health(HealthKey::Voltage),
                        create_metric_value(&labels, value),
                    ))
                    .await?
            }
            (b"RPM", Ok(value)) => {
                metrics_collection
                    .send((
                        MetricKey::Health(HealthKey::Rpm),
                        create_metric_value(&labels, value),
                    ))
                    .await?
            }
            (b"A", Ok(value)) => {
                metrics_collection
                    .send((
                        MetricKey::Health(HealthKey::Current),
                        create_metric_value(&labels, value),
                    ))
                    .await?
            }
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
    Ok(())
}

async fn collect_wifi_metric(
    metrics_collection: &Sender<(MetricKey, Metric)>,
    device: &MikrotikDevice,
    identity: &str,
    ip_leases: &HashMap<MacAddress, IpLease>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut radio_stream = stream_resource::<InterfaceWifiRadioState>(&device)
        .await
        .filter_map(log_problems(identity, "Wifi Radios"));
    let mut radios = HashMap::new();
    while let Some(radio) = radio_stream.next().await {
        radios.insert(radio.interface.clone(), radio);
    }
    if radios.is_empty() {
        return Ok(());
    }
    let mut interface_stream = stream_resource::<InterfaceWifi>(&device)
        .await
        .filter_map(log_problems(identity, "Wifi Interfaces"));
    let mut interfaces = HashMap::new();
    while let Some(InterfaceWifi { cfg, status }) = interface_stream.next().await {
        interfaces.insert(cfg.name.clone(), (cfg, status));
    }
    let mut registration_stream = stream_resource::<InterfaceWifiRegistrationTableState>(&device)
        .await
        .filter_map(log_problems(identity, "Wifi Registrations"));
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
        )
        .await?;
    }
    Ok(())
}

async fn collect_capsman_metric(
    sender: &Sender<(MetricKey, Metric)>,
    device: &MikrotikDevice,
    identity: &str,
    ip_leases: &HashMap<MacAddress, IpLease>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cap_identities_by_radio = HashMap::new();
    let mut radio_stream = stream_resource::<CapsManRadioState>(&device)
        .await
        .filter_map(log_problems(identity, "Capsman Radio"));
    while let Some(radio) = radio_stream.next().await {
        cap_identities_by_radio.insert(radio.interface.clone(), radio);
    }
    if cap_identities_by_radio.is_empty() {
        return Ok(());
    }
    let mut configuration_by_interface = HashMap::new();
    let mut interface_stream =
        stream_resource::<(CapsManInterfaceById, CapsManInterfaceState)>(&device)
            .await
            .filter_map(log_problems(identity, "Capsman Interfaces"));
    while let Some((cfg, state)) = interface_stream.next().await {
        //info!("Found Interface: {:?}, {:?}", cfg,state);
        let cfg = cfg.data;
        configuration_by_interface.insert(cfg.name.clone(), (cfg, state));
    }

    let mut registration_table = stream_resource::<CapsManRegistrationTableState>(&device)
        .await
        .filter_map(log_problems(identity, "Capsman Registrations"));
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
            sender,
            label,
            value.bytes,
            value.packets,
            value.rx_signal,
            value.uptime,
        )
        .await?;
    }
    Ok(())
}

async fn write_caps_metrics(
    sender: &Sender<(MetricKey, Metric)>,
    label: Vec<LabelPair>,
    bytes_pair: StatsPair<u64>,
    packets_pair: StatsPair<u64>,
    signal: i16,
    uptime: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    sender
        .send((
            MetricKey::Wlan(WlanKey::TxBytes),
            create_metric_value(&label, bytes_pair.tx as f64),
        ))
        .await?;
    sender
        .send((
            MetricKey::Wlan(WlanKey::RxBytes),
            create_metric_value(&label, bytes_pair.rx as f64),
        ))
        .await?;
    sender
        .send((
            MetricKey::Wlan(WlanKey::RxSignal),
            create_metric_value(&label, signal as f64),
        ))
        .await?;
    sender
        .send((
            MetricKey::Wlan(WlanKey::RxPackets),
            create_metric_value(&label, packets_pair.rx as f64),
        ))
        .await?;
    sender
        .send((
            MetricKey::Wlan(WlanKey::TxPackets),
            create_metric_value(&label, packets_pair.tx as f64),
        ))
        .await?;
    sender
        .send((
            MetricKey::Wlan(WlanKey::Uptime),
            create_metric_value(&label, uptime.as_secs_f64()),
        ))
        .await?;
    Ok(())
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

fn log_problems<E>(identity: &str, step: &str) -> impl Fn(SentenceResult<E>) -> Option<E> {
    move |entry| match entry {
        SentenceResult::Row { value, warnings } => {
            warnings.iter().for_each(|w| warn!("Warning: {w}"));
            Some(value)
        }

        SentenceResult::Error { errors, warnings } => {
            warnings.iter().for_each(|w| warn!("Warning: {w}"));
            for error in errors.iter() {
                error!("{identity} {step} Error: {error}");
            }
            None
        }
        SentenceResult::Trap { category, message } => {
            if category.is_none() && message.as_ref() == b"no such command prefix" {
                debug!(
                    "{identity} {step} Trap: {category:?}: {}",
                    decode_latin1(message.as_ref())
                );
            } else if let Some(TrapCategory::MissingItemOrCommand) = category {
                debug!(
                    "{identity} {step} Trap: {category:?}: {}",
                    decode_latin1(message.as_ref())
                );
            } else {
                error!(
                    "{identity} {step} Trap: {category:?}: {}",
                    decode_latin1(message.as_ref())
                );
            }
            None
        }
    }
}
