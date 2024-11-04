#![no_std]

use core::str::FromStr;
use cyw43::Control;
use embassy_net::Stack;
use embassy_net_driver_channel::Device as D;
use embassy_rp::adc::{Adc, Async, Channel};
use embassy_rp::flash::{Blocking as FlashBlocking, ERASE_SIZE};
use embassy_rp::peripherals::{FLASH, USB};
use embassy_rp::usb::Driver;
use embassy_usb::class::cdc_acm::Sender;
use heapless::{String, Vec};
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
    info!("Deserializing: {:?}", input.as_str());
    let (data, _remainder) =
        serde_json_core::from_str::<Configuration>(&input).map_err(|_| "Unable to parse Json\n")?;
    Ok(data)
}

#[derive(Debug, PartialEq)]
pub enum PicoCommand {
    Led(bool),
    SetConfig(Configuration),
    Temperature,
    Reset,
    PrintState,
}

pub struct DemoDeviceBuilder<'a, M>
where
    M: RawMutex,
{
    usb_sender: Option<Sender<'a, Driver<'a, USB>>>,
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
            usb_sender: None,
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

    pub fn with_usb_sender(mut self, sender: Sender<'a, Driver<'a, USB>>) -> Self {
        self.usb_sender = Some(sender);
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
            usb_sender: self.usb_sender.unwrap(),
            mqtt_client: None,
            temp_readings: Vec::new(),
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
    usb_sender: Sender<'a, Driver<'a, USB>>,
    id_string: &'static String<16>,
    mqtt_client: Option<MqttClient<'a, TcpSocket<'a>, 5, CountingRng>>,
    temp_readings: Vec<f32, 20>,
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
            self.set_led(true).await;
            Timer::after_millis(1000).await;
            self.set_led(false).await;
            Timer::after_millis(1000).await;
        }

        // let default_config = Configuration {
        //     led_pin: 25,
        //     wifi_ssid: heapless::String::from("PerkyIoT"),
        //     wifi_password: String::from("M5T$f6FrmACKoY9k"),
        //     mqtt_server: String::from("192.168.1.88"),
        //     mqtt_port: 1883,
        //     mqtt_username: String::from("test"),
        //     mqtt_password: String::from("test"),
        // };


        // self.write_config(default_config).unwrap();

        match self.read_config() {
            Ok(_) => {
                let mut config_string = heapless::String::<256>::new();
                write!(config_string, "{:?}", self.config).unwrap();
                info!("Config read successfully");
                info!("{}", config_string.as_str());
                Timer::after_millis(5000).await;
            }
            Err(_) => {
                info!("Failed to read default config");
            }
        }

         // Wait until the device is configured
         while self.config.is_none() {
            self.check_for_command().await;
            Timer::after_millis(1000).await;
            self.set_led(true).await;
            Timer::after_millis(100).await;
            self.set_led(false).await;
        }

        // self.watchdog.start(Duration::from_millis(5000));
        self.connect_wifi().await;
        self.watchdog.feed();
        self.connect_mqtt().await;
        self.watchdog.feed();
    }

    pub fn read_config(&mut self) -> Result<(), &'static str> {
        let mut bytes = [0u8; 256];
        self.flash
            .blocking_read(ADDR_OFFSET, &mut bytes)
            .map_err(|err| "Failed to read config")?;
        let config: Configuration = from_bytes(&bytes)
            .map_err(|_| "Failed to deserialize config")
            .unwrap();
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
                    self.usb_sender
                        .write_packet("join failed\n".as_bytes())
                        .await
                        .unwrap();
                    info!("join failed with status={}", err.status);
                }
            }
        }
        self.watchdog.feed();

        // Wait for DHCP, not necessary when using static IP
        self.usb_sender
            .write_packet("waiting for DHCP...\n".as_bytes())
            .await
            .unwrap();
        info!("waiting for DHCP...");
        while !self.stack.is_config_up() {
            Timer::after_millis(100).await;
        }
        self.watchdog.feed();
        self.usb_sender
            .write_packet("DHCP is now up!\n".as_bytes())
            .await
            .unwrap();
        info!("DHCP is now up!");

        self.usb_sender
            .write_packet("waiting for link up...\n".as_bytes())
            .await
            .unwrap();
        info!("waiting for link up...");
        while !self.stack.is_link_up() {
            Timer::after_millis(500).await;
        }
        self.watchdog.feed();
        info!("Link is up!");
        self.usb_sender
            .write_packet("Link is up!\n".as_bytes())
            .await
            .unwrap();

        info!("waiting for stack to be up...");
        self.usb_sender
            .write_packet("waiting for stack to be up...\n".as_bytes())
            .await
            .unwrap();
        self.stack.wait_config_up().await;
        self.watchdog.feed();
        info!("Stack is up!");
        self.usb_sender
            .write_packet("Stack is up!\n".as_bytes())
            .await
            .unwrap();
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
            Ok(()) => info!("MQTT Connected"),
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => info!("MQTT Network Error"),
                ReasonCode::Success => info!("Success"),
                _ => info!("Another Error {:?}", mqtt_error),
            },
        }
        self.watchdog.feed();

        client
            .send_message(
                static_will_topic.as_str(),
                "CONNECTED".as_bytes(),
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
                            self.usb_sender
                                .write_packet(response.as_bytes())
                                .await
                                .unwrap();
                        }
                        Err(msg) => {
                            self.usb_sender.write_packet(msg.as_bytes()).await.unwrap();
                        }
                    }
                }
                Err(msg) => {
                    self.usb_sender.write_packet(msg.as_bytes()).await.unwrap();
                }
            },
            Err(_) => {
                // No command ready
            }
        }
    }

    pub async fn run(&mut self) {
        loop {
            self.watchdog.feed();
            self.check_for_command().await;
            self.read_temperature().await;

            if self.last_publish.elapsed() > Duration::from_millis(1000) {
                self.publish_state().await;
                self.last_publish = Instant::now();
            }

            Timer::after_millis(50).await;
        }
    }

    pub async fn publish_state(&mut self) {
        if self.mqtt_client.is_none() {
            return;
        }
        let json_state = self.get_state_json().unwrap();

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
        if self.temp_readings.len() == 20 {
            self.temp_readings.pop().unwrap();
        }
        self.temp_readings.push(temp_c).unwrap();
        temp_c
    }

    pub fn get_avg_temperature(&self) -> f32 {
        let mut sum = 0.0;
        for temp in self.temp_readings.iter() {
            sum += temp;
        }
        sum / self.temp_readings.len() as f32
    }

    pub async fn set_led(&mut self, value: bool) {
        self.current_led_state = value;
        self.control.gpio_set(0, value).await;
    }

    pub fn get_state(&self) -> DeviceState {
        DeviceState {
            temperature: self.get_avg_temperature(),
            led_state: self.current_led_state,
        }
    }

    pub fn get_state_json(&self) -> Result<String<64>, &'static str> {
        let mut heapless_string = String::new();
        let serde_string = serde_json_core::to_string::<DeviceState, 64>(&self.get_state())
            .map_err(|_| "Failed to serialize state");
        match serde_string {
            Ok(serde_string) => {
                heapless_string
                    .push_str(serde_string.as_str())
                    .map_err(|_| "Failed to serialize state");
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
                message_string
                    .push_str("LED set\n")
                    .map_err(|_| "Failed to set LED\n")?;
                self.publish_state().await;
                Ok(message_string)
            }
            PicoCommand::Reset => {
                self.watchdog.trigger_reset();
                let mut message: String<64> = String::new();
                message
                    .push_str("Resetting\n")
                    .map_err(|_| "Failed to reset\n")?;
                Ok(message)
            }
            PicoCommand::SetConfig(config) => {
                self.write_config(&config).map_err(|_| "Failed to set config\n")?;
                self.set_config(config);
                let mut message_string = String::new();
                message_string
                    .push_str("Config set\n")
                    .map_err(|_| "Failed to set config\n")?;
                Ok(message_string)
            }
            PicoCommand::Temperature => {
                let temp_c = self.read_temperature().await;
                let mut message_string = String::new();
                write!(message_string, "{:.2}", temp_c)
                    .map_err(|_| "Failed to read temperature\n")?;
                message_string
                    .push_str("° C\n")
                    .map_err(|_| "Failed to read temperature\n")?;
                Ok(message_string)
            }
            PicoCommand::PrintState => {
                if self.config.is_none() {
                    return Err("Config not set\n");
                }

                let current_state = self.get_state();
                let mut heapless_string = String::new();
                let state_string = serde_json_core::to_string::<DeviceState, 64>(&current_state)
                    .map_err(|_| "Failed to serialize state\n")?;
                return match heapless_string.push_str(state_string.as_str()) {
                    Ok(_) => Ok(heapless_string),
                    Err(_) => Err("Failed to serialize state\n"),
                };
            }
        };
    }
}

pub fn parse_command(input: heapless::String<256>) -> Result<PicoCommand, &'static str> {
    let mut iter = input.split_whitespace();
    let command = iter.next().ok_or(()).map_err(|_| "No command found\n")?;
    match command {
        "LED" => {
            let value = iter
                .next()
                .ok_or(())
                .map_err(|_| "LED command requires a parameter of ON or OFF\n")?;
            match value {
                "ON" => return Ok(PicoCommand::Led(true)),
                "OFF" => return Ok(PicoCommand::Led(false)),
                _ => return Err("Invalid value for LED\n"),
            }
        }
        "TEMP" => {
            let param = iter.next().is_some();
            if param {
                return Err("TEMP command does not take any parameters\n");
            }
            return Ok(PicoCommand::Temperature);
        }
        "RESET" => {
            let param = iter.next().is_some();
            if param {
                return Err("RESET command does not take any parameters\n");
            }
            return Ok(PicoCommand::Reset);
        }
        "CONFIG" => {
            let config_str = iter
                .next()
                .ok_or(())
                .map_err(|_| "CONFIG requires a Json parameter\n")?;
            let mut config_string = String::new();
            config_string
                .push_str(config_str)
                .map_err(|_| "Failed to parse JSON\n")?;
            let config = deserialize_config(&config_string).map_err(|msg| msg)?;
            return Ok(PicoCommand::SetConfig(config));
        }
        "PRINT" => {
            let param = iter.next().is_some();
            if param {
                return Err("PRINT command does not take any parameters\n");
            }
            return Ok(PicoCommand::PrintState);
        }
        _ => return Err("Unknown command\n"),
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
        assert_eq!(command, PicoCommand::Led(true));

        let mut input = String::new();
        input.push_str("LED OFF").unwrap();
        let command = parse_command(input).unwrap();
        assert_eq!(command, PicoCommand::Led(false));
    }

    #[test]
    fn test_parse_config() {
        let mut input = String::new();
        input.push_str(r#"CONFIG {"led_pin":1,"wifi_ssid":"ssid","wifi_password":"password","mqtt_server":"server","mqtt_port":1883,"mqtt_username":"username","mqtt_password":"password"}"#).unwrap();
        let _command = parse_command(input).unwrap();
    }

    #[test]
    fn test_deserialize_config() {
        let test_config = r#"{"led_pin":1,"wifi_ssid":"ssid","wifi_password":"password","mqtt_server":"server","mqtt_port":1883,"mqtt_username":"username","mqtt_password":"password"}"#;

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
    }
}
