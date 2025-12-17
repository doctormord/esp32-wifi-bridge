//! Wi-Fi to Ethernet Bridge State Machine

extern crate alloc;
use alloc::{boxed::Box, format, string::String, sync::Arc};

use esp_idf_svc::{
    eth::{EthDriver, RmiiClockConfig, RmiiEth, RmiiEthChipset},
    eventloop::EspSystemEventLoop,
    hal::{delay, gpio, modem::Modem, prelude::Peripherals},
    nvs::EspDefaultNvsPartition,
    wifi::{AuthMethod, ClientConfiguration, Configuration, WifiDeviceId, WifiDriver},
};

use once_cell::sync::OnceCell;

/// Wi-Fi to Ethernet Bridge State Machine
pub struct Bridge<S> { state: S }

pub struct Idle {
    peripherals: Peripherals,
    sysloop: EspSystemEventLoop,
    nvs: Option<EspDefaultNvsPartition>,
}

pub struct EthReady {
    modem: Modem,
    sysloop: EspSystemEventLoop,
    nvs: Option<EspDefaultNvsPartition>,
    eth: EthDriver<'static, RmiiEth>,
    client_mac: [u8; 6],
}

pub struct WifiReady {
    eth: EthDriver<'static, RmiiEth>,
    wifi: WifiDriver<'static>,
    client_mac: [u8; 6], // Wir speichern die MAC hier nur zwischen
}

pub struct Running {
    _eth: Box<EthDriver<'static, RmiiEth>>,
    _wifi: Box<WifiDriver<'static>>,
}

impl Bridge<Idle> {
    pub fn new() -> Self {
        let peripherals = Peripherals::take().expect("Failed to take peripherals!");
        let sysloop = EspSystemEventLoop::take().expect("Failed to take sysloop!");
        let nvs = EspDefaultNvsPartition::take().ok();
        Self { state: Idle { peripherals, sysloop, nvs } }
    }
}

impl From<Bridge<Idle>> for Bridge<EthReady> {
    fn from(val: Bridge<Idle>) -> Self {
        let pins = val.state.peripherals.pins;
        let mut eth = EthDriver::new_rmii(
            val.state.peripherals.mac,
            pins.gpio25, pins.gpio26, pins.gpio27, pins.gpio23, pins.gpio22,
            pins.gpio21, pins.gpio19, pins.gpio18,
            RmiiClockConfig::<gpio::Gpio0, gpio::Gpio16, gpio::Gpio17>::Input(pins.gpio0),
            Some(pins.gpio16), RmiiEthChipset::LAN87XX, Some(1),
            val.state.sysloop.clone(),
        ).expect("Failed to init EthDriver!");

        let client_mac: Arc<OnceCell<[u8; 6]>> = Arc::new(OnceCell::new());
        let client_mac2 = Arc::clone(&client_mac);

        eth.set_rx_callback(move |frame| match frame.as_slice().get(6..12) {
            Some(mac_bytes) => {
                let src_mac = mac_bytes.try_into().unwrap();
                if client_mac2.set(src_mac).is_ok() {
                    log::warn!("Sniffed client MAC: {}", mac2str(src_mac));
                }
            }
            None => unreachable!("Failed to read source MAC"),
        }).expect("Failed to set Ethernet callback! (macsniff)");

        log::warn!("Waiting to sniff client MAC...");
        eth.start().expect("Failed to start Ethernet!");
        let client_mac = *client_mac.wait();

        eth.set_rx_callback(|_| {}).expect("Failed to unset callback");
        eth.set_promiscuous(true).expect("Failed to set promiscuous");

        Self {
            state: EthReady {
                modem: val.state.peripherals.modem,
                sysloop: val.state.sysloop,
                nvs: val.state.nvs,
                eth,
                client_mac,
            },
        }
    }
}

impl From<Bridge<EthReady>> for Bridge<WifiReady> {
    fn from(val: Bridge<EthReady>) -> Self {
        // Hier NUR Treiber erstellen, NICHTS setzen
        let wifi = WifiDriver::new(val.state.modem, val.state.sysloop.clone(), val.state.nvs)
            .expect("Failed to init WifiDriver!");
        
        Self { 
            state: WifiReady { 
                eth: val.state.eth, 
                wifi, 
                client_mac: val.state.client_mac // MAC weiterreichen
            } 
        }
    }
}

impl From<Bridge<WifiReady>> for Bridge<Running> {
    fn from(val: Bridge<WifiReady>) -> Self {
        let mut eth = Box::new(val.state.eth);
        let mut wifi = Box::new(val.state.wifi);
        let client_mac = val.state.client_mac;

        // 1. Set Callbacks
        let eth_ptr = &mut *eth as *mut EthDriver<'static, RmiiEth> as usize;
        unsafe {
            wifi.set_nonstatic_callbacks({
                let eth_ptr = eth_ptr;
                move |_, frame| {
                    let eth = &mut *(eth_ptr as *mut EthDriver<'static, RmiiEth>);
                    if eth.is_connected().unwrap_or(false) {
                        eth.send(frame.as_slice())?;
                    }
                    Ok(())
                }
            }, |_, _, _| {}).expect("Failed to set Wi-Fi callbacks!");
        }

        let wifi_ptr = &mut *wifi as *mut WifiDriver<'static> as usize;
        unsafe {
            eth.set_nonstatic_rx_callback({
                let wifi_ptr = wifi_ptr;
                move |frame| {
                    let wifi = &mut *(wifi_ptr as *mut WifiDriver<'static>);
                    if wifi.is_connected().unwrap_or(false) {
                        let _ = wifi.send(WifiDeviceId::Sta, frame.as_slice());
                    }
                }
            }).expect("Failed to set Ethernet callback!");
        }
        
        // 2. Start Ethernet
        eth.start().expect("Failed to start Ethernet!");

        // 3. Start Wi-Fi Loop
        const CREDENTIALS: &[(Option<&str>, Option<&str>)] = &[
            (option_env!("WIFI_SSID_1"), option_env!("WIFI_PASS_1")),
            (option_env!("WIFI_SSID_2"), option_env!("WIFI_PASS_2")),
        ];

        let mut connected = false;
        for (ssid_opt, pass_opt) in CREDENTIALS.iter() {
            let ssid = ssid_opt.unwrap_or("");
            if ssid.is_empty() { continue; }

            log::info!("Connecting to WiFi: '{}'", ssid);
            let wifi_config = Configuration::Client(ClientConfiguration {
                ssid: ssid.try_into().unwrap(),
                auth_method: AuthMethod::WPA2Personal,
                password: pass_opt.unwrap_or("").try_into().unwrap(),
                ..Default::default()
            });

            // A. ZUERST Konfigurieren
            wifi.set_configuration(&wifi_config).expect("Config failed");
            
            // B. JETZT MAC setzen (das behebt den Crash!)
            wifi.set_mac(WifiDeviceId::Sta, client_mac).expect("Set MAC failed");

            // C. Starten
            wifi.start().expect("Start failed");
            wifi.connect().expect("Connect failed");

            for _ in 0..100 { // Wait up to 10s
                if wifi.is_connected().unwrap_or(false) {
                    connected = true;
                    log::info!("Connected to: '{}'", ssid);
                    break;
                }
                delay::FreeRtos::delay_ms(100);
            }
            if connected { break; }
            wifi.stop().expect("Stop failed");
        }

        if !connected { panic!("No WiFi connection possible."); }
        
        log::info!("Bridge is running.");
        Self { state: Running { _eth: eth, _wifi: wifi } }
    }
}

#[inline]
fn mac2str(mac: [u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}