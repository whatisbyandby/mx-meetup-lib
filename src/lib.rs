#![no_std]

use core::str::FromStr;
use cyw43::Control;
use embassy_net::Stack;
use embassy_net_driver_channel::Device as D;
use embassy_rp::adc::{Adc, Async, Channel};
use embassy_rp::flash::{Blocking as FlashBlocking, ERASE_SIZE};
use embassy_rp::peripherals::FLASH;
use heapless::String;
use serde::{Deserialize, Serialize};

use embassy_time::{Duration, Instant, Timer};

use embassy_net::tcp::TcpSocket;
use embassy_net::Ipv4Address;

use rust_mqtt::client::client::MqttClient;
use rust_mqtt::client::client_config::ClientConfig;
use rust_mqtt::packet::v5::reason_codes::ReasonCode;
use rust_mqtt::utils::rng_generator::CountingRng;

use defmt::info;

use core::fmt::Write;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Receiver;
use postcard::{from_bytes, to_vec};
use static_cell::StaticCell;

pub mod temperature_sensor;

const FLASH_SIZE: usize = 2 * 1024 * 1024;
const ADDR_OFFSET: u32 = 0x100000;

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Configuration {
    pub led_pin: u8,
    name: String<64>,
    wifi_ssid: String<64>,
    wifi_password: String<64>,
    mqtt_server: String<64>,
    mqtt_port: u16,
    mqtt_username: String<64>,
    mqtt_password: String<64>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct DeviceState {
    pub temperature: f32,
    pub led_state: bool,
}

pub fn deserialize_config(input: &String<256>) -> Result<Configuration, &'static str> {
    let (data, _remainder) =
        serde_json_core::from_str::<Configuration>(&input).map_err(|_| "Unable to parse Json")?;
    Ok(data)
}

#[derive(Debug, PartialEq)]
pub enum LedCommandParameter {
    On,
    Off,
    Toggle,
}

#[derive(Debug, PartialEq)]
pub enum PicoCommand {
    Led(LedCommandParameter),
    SetConfig(Configuration),
    Temperature,
    Reset,
    PrintState,
}

pub struct DemoDeviceBuilder<'a, M>
where
    M: RawMutex,
{
    command_receiver: Option<Receiver<'a, M, Result<PicoCommand, &'static str>, 5>>,
    stack: Option<&'a Stack<D<'static, 1514>>>,
    control: Option<Control<'a>>,
    adc: Option<(Adc<'a, Async>, Channel<'a>)>,
    id_string: Option<&'static String<16>>,
    watchdog: Option<embassy_rp::watchdog::Watchdog>,
    flash: Option<embassy_rp::flash::Flash<'static, FLASH, FlashBlocking, FLASH_SIZE>>,
}

impl<'a, M> DemoDeviceBuilder<'a, M>
where
    M: RawMutex,
{
    pub fn new() -> Self {
        Self {
            command_receiver: None,
            stack: None,
            control: None,
            adc: None,
            id_string: None,
            watchdog: None,
            flash: None,
        }
    }

    pub fn with_id(mut self, id: &'static heapless::String<16>) -> Self {
        self.id_string = Some(id);
        self
    }

    pub fn with_command_receiver(
        mut self,
        receiver: Receiver<'a, M, Result<PicoCommand, &'static str>, 5>,
    ) -> Self {
        self.command_receiver = Some(receiver);
        self
    }

    pub fn with_stack(mut self, stack: &'a Stack<D<'static, 1514>>) -> Self {
        self.stack = Some(stack);
        self
    }

    pub fn with_flash(
        mut self,
        flash: embassy_rp::flash::Flash<'static, FLASH, FlashBlocking, FLASH_SIZE>,
    ) -> Self {
        self.flash = Some(flash);
        self
    }

    pub fn with_control(mut self, control: Control<'a>) -> Self {
        self.control = Some(control);
        self
    }

    pub fn with_adc(mut self, adc: Adc<'a, Async>, channel: Channel<'a>) -> Self {
        self.adc = Some((adc, channel));
        self
    }

    pub fn with_watchdog(mut self, watchdog: embassy_rp::watchdog::Watchdog) -> Self {
        self.watchdog = Some(watchdog);
        self
    }

    pub fn build(self) -> DemoDevice<'a, M> {
        DemoDevice {
            adc: self.adc.unwrap(),
            config: None,
            current_led_state: false,
            control: self.control.unwrap(),
            stack: self.stack.unwrap(),
            command_receiver: self.command_receiver.unwrap(),
            id_string: self.id_string.unwrap(),
            mqtt_client: None,
            last_publish: Instant::now(),
            watchdog: self.watchdog.unwrap(),
            flash: self.flash.unwrap(),
        }
    }
}

fn convert_to_celsius(raw_temp: u16) -> f32 {
    // According to chapter 4.9.5. Temperature Sensor in RP2040 datasheet
    let temp = 27.0 - (raw_temp as f32 * 3.3 / 4096.0 - 0.706) / 0.001721;
    let sign = if temp < 0.0 { -1.0 } else { 1.0 };
    let rounded_temp_x10: i16 = ((temp * 10.0) + 0.5 * sign) as i16;
    (rounded_temp_x10 as f32) / 10.0
}

pub struct DemoDevice<'a, M>
where
    M: RawMutex,
{
    adc: (Adc<'a, Async>, Channel<'a>),
    config: Option<Configuration>,
    current_led_state: bool,
    control: Control<'a>,
    stack: &'a Stack<D<'static, 1514>>,
    command_receiver: Receiver<'a, M, Result<PicoCommand, &'static str>, 5>,
    id_string: &'static String<16>,
    mqtt_client: Option<MqttClient<'a, TcpSocket<'a>, 5, CountingRng>>,
    last_publish: Instant,
    watchdog: embassy_rp::watchdog::Watchdog,
    flash: embassy_rp::flash::Flash<'static, FLASH, FlashBlocking, FLASH_SIZE>,
}

impl<'a, M> DemoDevice<'a, M>
where
    M: RawMutex,
{
    pub async fn init(&mut self) {
        self.control
            .set_power_management(cyw43::PowerManagementMode::PowerSave)
            .await;

        // Blink the LED to indicate the device is starting up
        for _ in 0..2 {
            self.set_led(LedCommandParameter::On).await;
            Timer::after_millis(1000).await;
            self.set_led(LedCommandParameter::Off).await;
            Timer::after_millis(1000).await;
        }

        match self.read_config() {
            Ok(_) => {
                let mut config_string = heapless::String::<256>::new();
                write!(config_string, "{:?}", self.config).unwrap();
                log::info!("Config read successfully");
                Timer::after_millis(5000).await;
            }
            Err(_) => {
                log::error!("Failed to read default config");
            }
        }

        // Wait until the device is configured
        while self.config.is_none() {
            log::info!("Waiting for config");
            self.check_for_command().await;
            Timer::after_millis(1000).await;
            self.set_led(LedCommandParameter::On).await;
            Timer::after_millis(100).await;
            self.set_led(LedCommandParameter::Off).await;
        }

        self.connect_wifi().await;
        self.connect_mqtt().await;
        self.watchdog.start(Duration::from_millis(5000));
    }

    pub fn read_config(&mut self) -> Result<(), &'static str> {
        let mut bytes = [0u8; 256];
        self.flash
            .blocking_read(ADDR_OFFSET, &mut bytes)
            .map_err(|err| "Failed to read config")?;
        let config: Configuration =
            from_bytes(&bytes).map_err(|_| "Failed to deserialize config")?;
        self.set_config(config);
        Ok(())
    }

    pub fn write_config(&mut self, config: &Configuration) -> Result<(), &'static str> {
        let bytes: heapless::Vec<u8, 256> =
            to_vec(config).map_err(|_| "Failed to serialize config")?;
        self.flash
            .blocking_erase(ADDR_OFFSET, ADDR_OFFSET + ERASE_SIZE as u32)
            .map_err(|_| "Failed to erase flash")?;
        self.flash
            .blocking_write(ADDR_OFFSET, &bytes)
            .map_err(|_| "Failed to write config")?;
        Ok(())
    }

    pub async fn connect_wifi(&mut self) {
        let ssid = self.config.as_ref().unwrap().wifi_ssid.as_str();
        let passwd = self.config.as_ref().unwrap().wifi_password.as_str();

        loop {
            match self.control.join_wpa2(ssid, passwd).await {
                Ok(_) => break,
                Err(err) => {
                    log::info!("join failed with status={}", err.status);
                }
            }
        }
        log::info!("waiting for DHCP...");
        while !self.stack.is_config_up() {
            Timer::after_millis(100).await;
        }
        log::info!("DHCP is now up!");
        log::info!("waiting for link up...");
        while !self.stack.is_link_up() {
            Timer::after_millis(500).await;
        }
        log::info!("Link is up!");
        log::info!("waiting for stack to be up...");
        self.stack.wait_config_up().await;

        log::info!("Stack is up!");
    }

    pub async fn connect_mqtt(&mut self) {
        static RX_BUFFER: StaticCell<[u8; 4096]> = StaticCell::new();
        static TX_BUFFER: StaticCell<[u8; 4096]> = StaticCell::new();

        let rx_buffer: [u8; 4096] = [0; 4096];
        let tx_buffer: [u8; 4096] = [0; 4096];

        let static_rx_buffer = RX_BUFFER.init(rx_buffer);
        let static_tx_buffer = TX_BUFFER.init(tx_buffer);

        let mut socket = TcpSocket::new(self.stack, static_rx_buffer, static_tx_buffer);

        socket.set_timeout(None);

        let address = Ipv4Address::from_str(self.config.as_ref().unwrap().mqtt_server.as_str())
            .map_err(|_| "Failed to parse IP address")
            .unwrap();

        let remote_endpoint = (address, 1883);
        socket.connect(remote_endpoint).await.unwrap();
        self.watchdog.feed();

        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );

        static PASSWORD : StaticCell<String<64>> = StaticCell::new();
        static USERNAME : StaticCell<String<64>> = StaticCell::new();

        let static_passwrord = PASSWORD.init(self.config.as_ref().unwrap().mqtt_password.clone());
        let static_username = USERNAME.init(self.config.as_ref().unwrap().mqtt_username.clone());

        config.add_password(static_passwrord.as_str());
        config.add_username(static_username.as_str());

        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        let mut client_id = String::<64>::new();
        client_id.push_str("device-").unwrap();
        client_id.push_str(self.id_string.as_str()).unwrap();
        static CLIENT_ID: StaticCell<String<64>> = StaticCell::new();
        let static_client_id = CLIENT_ID.init(client_id);
        config.add_client_id(static_client_id.as_str());

        let mut will_topic = String::<64>::new();
        will_topic.push_str("device/").unwrap();
        will_topic.push_str(self.id_string.as_str()).unwrap();
        will_topic.push_str("/status").unwrap();
        static WILL_TOPIC: StaticCell<String<64>> = StaticCell::new();
        let static_will_topic = WILL_TOPIC.init(will_topic);

        config.add_will(static_will_topic.as_str(), "DISCONNECTED".as_bytes(), true);
        config.max_packet_size = 100;

        let recv_buffer = [0u8; 256];
        static RECV_BUFFER: StaticCell<[u8; 256]> = StaticCell::new();
        let static_recv_buffer = RECV_BUFFER.init(recv_buffer);

        let write_buffer = [0u8; 256];
        static WRITE_BUFFER: StaticCell<[u8; 256]> = StaticCell::new();
        let static_write_buffer = WRITE_BUFFER.init(write_buffer);

        let mut client: MqttClient<'a, TcpSocket<'a>, 5, CountingRng> = MqttClient::<_, 5, _>::new(
            socket,
            static_write_buffer,
            256,
            static_recv_buffer,
            256,
            config,
        );

        match client.connect_to_broker().await {
            Ok(()) => log::info!("MQTT Connected"),
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => info!("MQTT Network Error"),
                ReasonCode::Success => info!("Success"),
                _ => info!("Another Error {:?}", mqtt_error),
            },
        }
        self.watchdog.feed();

        let mut subscribe_topic = String::<64>::new();
        write!(subscribe_topic, "device/{}/command", self.id_string).unwrap();
        client.subscribe_to_topic(subscribe_topic.as_str()).await.unwrap();

        client
            .send_message(
                static_will_topic.as_str(),
                "CONNECTED".as_bytes(),
                rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                true,
            )
            .await
            .unwrap();

        let mut state_topic = String::<64>::new();
        state_topic.push_str("device/").unwrap();
        state_topic.push_str(self.id_string.as_str()).unwrap();
        state_topic.push_str("/name").unwrap();

        client
            .send_message(
                state_topic.as_str(),
                self.get_config().unwrap().name.as_bytes(),
                rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                true,
            )
            .await
            .unwrap();

        self.watchdog.feed();

        self.mqtt_client = Some(client);
    }

    pub async fn check_for_command(&mut self) {
        let command_ready = self.command_receiver.try_receive();
        match command_ready {
            Ok(command) => match command {
                Ok(command) => {
                    let response = self.execute_command(command).await;
                    match response {
                        Ok(response) => {
                            log::info!("{}", response);
                        }
                        Err(msg) => {
                           log::error!("{}", msg);
                        }
                    }
                }
                Err(msg) => {
                   log::error!("{}", msg);
                }
            },
            Err(_) => {
                // No command ready
            }
        }
    }

    pub async fn run(&mut self) {
            self.watchdog.feed();
            self.check_for_command().await;

            if self.last_publish.elapsed() > Duration::from_millis(1000) {
                self.publish_state().await;
                self.last_publish = Instant::now();
                log::debug!("Published state");
            }
    }

    pub async fn publish_state(&mut self) {
        if self.mqtt_client.is_none() {
            return;
        }
        let json_state = self.get_state_json().await.unwrap();

        let mut state_topic = String::<64>::new();
        state_topic.push_str("device/").unwrap();
        state_topic.push_str(self.id_string.as_str()).unwrap();
        state_topic.push_str("/state").unwrap();

        self.mqtt_client
            .as_mut()
            .unwrap()
            .send_message(
                state_topic.as_str(),
                json_state.as_bytes(),
                rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1,
                true,
            )
            .await
            .unwrap();
    }

    pub fn set_config(&mut self, config: Configuration) {
        self.config = Some(config);
    }

    pub fn get_config(&self) -> Option<&Configuration> {
        self.config.as_ref()
    }

    pub async fn read_temperature(&mut self) -> f32 {
        let raw_reading = self.adc.0.read(&mut self.adc.1).await.unwrap();
        let temp_c = convert_to_celsius(raw_reading);
        temp_c
    }

    pub async fn set_led(&mut self, value: LedCommandParameter) {

        match value {
            LedCommandParameter::On => {
                self.current_led_state = true;
            }
            LedCommandParameter::Off => {
                self.current_led_state = false;
            }
            LedCommandParameter::Toggle => {
                self.current_led_state = !self.current_led_state;
            }
        }

        self.control.gpio_set(0, self.current_led_state).await;
    }

    pub async fn get_state(&mut self) -> DeviceState {
        DeviceState {
            temperature: self.read_temperature().await,
            led_state: self.current_led_state,
        }
    }

    pub async fn get_state_json(&mut self) -> Result<String<64>, &'static str> {
        let mut heapless_string = String::new();
        let serde_string = serde_json_core::to_string::<DeviceState, 64>(&self.get_state().await)
            .map_err(|_| "Failed to serialize state");
        match serde_string {
            Ok(serde_string) => {
                heapless_string
                    .push_str(serde_string.as_str())
                    .map_err(|_| "Failed to serialize state")?;
                Ok(heapless_string)
            }
            Err(msg) => Err(msg),
        }
    }

    pub async fn execute_command(
        &mut self,
        command: PicoCommand,
    ) -> Result<String<64>, &'static str> {
        return match command {
            PicoCommand::Led(value) => {
                self.set_led(value).await;
                let mut message_string = String::new();
                write!(message_string, "LED set to {:?}", self.current_led_state)
                    .map_err(|_| "Failed to set LED")?;
                self.publish_state().await;
                Ok(message_string)
            }
            PicoCommand::Reset => {
                self.watchdog.trigger_reset();
                let mut message: String<64> = String::new();
                message
                    .push_str("Resetting")
                    .map_err(|_| "Failed to reset")?;
                Ok(message)
            }
            PicoCommand::SetConfig(config) => {
                self.write_config(&config)
                    .map_err(|_| "Failed to set config")?;
                self.set_config(config);
                let mut message_string = String::new();
                message_string
                    .push_str("Config set")
                    .map_err(|_| "Failed to set config")?;
                Ok(message_string)
            }
            PicoCommand::Temperature => {
                let temp_c = self.read_temperature().await;
                let mut message_string = String::new();
                write!(message_string, "{:.2}", temp_c)
                    .map_err(|_| "Failed to read temperature")?;
                message_string
                    .push_str("° C")
                    .map_err(|_| "Failed to read temperature")?;
                Ok(message_string)
            }
            PicoCommand::PrintState => {
                if self.config.is_none() {
                    return Err("Config not set");
                }

                let current_state = self.get_state().await;
                let mut heapless_string = String::new();
                let state_string = serde_json_core::to_string::<DeviceState, 64>(&current_state)
                    .map_err(|_| "Failed to serialize state")?;
                return match heapless_string.push_str(state_string.as_str()) {
                    Ok(_) => Ok(heapless_string),
                    Err(_) => Err("Failed to serialize state"),
                };
            }
        };
    }
}

pub fn parse_command(input: heapless::String<256>) -> Result<PicoCommand, &'static str> {
    // Use splitn to split at the first space
    let mut parts = input.splitn(2, ' ');

    let command = parts
        .next()
        .ok_or(())
        .map_err(|_| "Failed to parse command")?;

    match command {
        "LED" => {
            let value = parts
                .next()
                .ok_or(())
                .map_err(|_| "LED command requires a parameter of ON or OFF")?;
            match value {
                "ON" => return Ok(PicoCommand::Led(LedCommandParameter::On)),
                "OFF" => return Ok(PicoCommand::Led(LedCommandParameter::Off)),
                "TOGGLE" => return Ok(PicoCommand::Led(LedCommandParameter::Toggle)),
                _ => return Err("Invalid value for LED"),
            }
        }
        "TEMP" => {
            let param = parts.next().is_some();
            if param {
                return Err("TEMP command does not take any parameters");
            }
            return Ok(PicoCommand::Temperature);
        }
        "RESET" => {
            let param = parts.next().is_some();
            if param {
                return Err("RESET command does not take any parameters");
            }
            return Ok(PicoCommand::Reset);
        }
        "CONFIG" => {
            let config_str = parts
                .next()
                .ok_or(())
                .map_err(|_| "CONFIG requires a Json parameter")?;
            let mut config_string = String::new();
            config_string
                .push_str(config_str)
                .map_err(|_| "Failed to parse JSON")?;
            let config = deserialize_config(&config_string).map_err(|msg| msg)?;
            return Ok(PicoCommand::SetConfig(config));
        }
        "PRINT" => {
            let param = parts.next().is_some();
            if param {
                return Err("PRINT command does not take any parameters");
            }
            return Ok(PicoCommand::PrintState);
        }
        _ => return Err("Unknown command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_led() {
        let mut input = String::new();
        input.push_str("LED ON").unwrap();
        let command = parse_command(input).unwrap();
        assert_eq!(command, PicoCommand::Led(LedCommandParameter::On));

        let mut input = String::new();
        input.push_str("LED OFF").unwrap();
        let command = parse_command(input).unwrap();
        assert_eq!(command, PicoCommand::Led(LedCommandParameter::Off));
    }

    #[test]
    fn test_parse_config() {
        let mut input = String::new();
        input.push_str(r#"CONFIG {"wifi_ssid":"PerkyIoT","wifi_password":"M5T$f6FrmACKoY9k","mqtt_server":"192.168.1.88","mqtt_username":"Test","mqtt_password":"Test","mqtt_port":1883,"led_pin":1,"name":"Test Name"}"#).unwrap();
        let command = parse_command(input).unwrap();
        assert!(matches!(command, PicoCommand::SetConfig(_)));
    }

    #[test]
    fn test_deserialize_config() {
        let test_config = r#"{"wifi_ssid":"ssid","wifi_password":"password","mqtt_server":"server","mqtt_username":"username","mqtt_password":"password","mqtt_port":1883,"led_pin":1,"name":"Test Device"}"#;

        let mut input = String::new();
        input.push_str(test_config).unwrap();
        let config = deserialize_config(&input).unwrap();

        assert_eq!(config.led_pin, 1);
        assert_eq!(config.wifi_ssid, "ssid");
        assert_eq!(config.wifi_password, "password");
        assert_eq!(config.mqtt_server, "server");
        assert_eq!(config.mqtt_port, 1883);
        assert_eq!(config.mqtt_username, "username");
        assert_eq!(config.mqtt_password, "password");
        assert_eq!(config.name, "Test Device");
    }
}
