#![no_std]

use cyw43::Control;
use embassy_net::Stack;
use embassy_net_driver_channel::Device as D;
use embassy_rp::adc::{Adc, Async, Channel};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_usb::class::cdc_acm::Sender;
use heapless::String;
use serde::{Deserialize, Serialize};

use embassy_time::Timer;

use defmt::info;

use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Receiver;

pub mod temperature_sensor;

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
        serde_json_core::from_str::<Configuration>(&input).map_err(|_| "Unable to parse Json")?;
    Ok(data)
}

#[derive(Debug, PartialEq)]
pub enum PicoCommand {
    Led(bool),
    SetConfig(Configuration),
    PrintState,
}

pub struct DemoDeviceBuilder<'a, M>
where
    M: RawMutex,
{
    usb_sender: Option<Sender<'a, Driver<'a, USB>>>,
    command_receiver: Option<Receiver<'a, M, Result<PicoCommand, &'static str>, 64>>,
    stack: Option<&'a Stack<D<'static, 1514>>>,
    control: Option<Control<'a>>,
	adc: Option<(Adc<'a, Async>, Channel<'a>)>
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
			adc: None
        }
    }

    pub fn with_usb_sender(mut self, sender: Sender<'a, Driver<'a, USB>>) -> Self {
        self.usb_sender = Some(sender);
        self
    }

    pub fn with_command_receiver(
        mut self,
        receiver: Receiver<'a, M, Result<PicoCommand, &'static str>, 64>,
    ) -> Self {
        self.command_receiver = Some(receiver);
        self
    }

    pub fn with_stack(mut self, stack: &'a Stack<D<'static, 1514>>) -> Self {
        self.stack = Some(stack);
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

    pub fn build(self) -> DemoDevice<'a, M> {
        DemoDevice {
			adc: self.adc.unwrap(),
            config: None,
            current_temperature: 0.0,
            current_led_state: false,
            control: self.control.unwrap(),
            stack: self.stack.unwrap(),
            command_receiver: self.command_receiver.unwrap(),
            usb_sender: self.usb_sender.unwrap(),
        }
    }
}

pub struct DemoDevice<'a, M>
where
    M: RawMutex,
{	
	adc: (Adc<'a, Async>, Channel<'a>),
    config: Option<Configuration>,
    current_temperature: f32,
    current_led_state: bool,
    control: Control<'a>,
    stack: &'a Stack<D<'static, 1514>>,
    command_receiver: Receiver<'a, M, Result<PicoCommand, &'static str>, 64>,
    usb_sender: Sender<'a, Driver<'a, USB>>,
}

impl<'a, M> DemoDevice<'a, M>
where
    M: RawMutex,
{
    pub async fn init(&mut self) {
        self.control
            .set_power_management(cyw43::PowerManagementMode::PowerSave)
            .await;

		// loop for 30 iterations
		for _ in 0..15 {
			Timer::after_millis(100).await;
			self.control.gpio_set(0, true).await;
			Timer::after_millis(100).await;
			self.control.gpio_set(0, false).await;
		}

		while self.config.is_none() {
			info!("No config set");
			self.run().await;
			Timer::after_millis(1000).await;
		}

		let ssid = self.config.as_ref().unwrap().wifi_ssid.as_str();
		let passwd = self.config.as_ref().unwrap().wifi_password.as_str();

        loop {
            match self.control.join_wpa2(ssid, passwd).await {
                Ok(_) => break,
                Err(err) => {
                    info!("join failed with status={}", err.status);
                }
            }
        }

        // Wait for DHCP, not necessary when using static IP
        info!("waiting for DHCP...");
        while !self.stack.is_config_up() {
            Timer::after_millis(100).await;
        }
        info!("DHCP is now up!");

        info!("waiting for link up...");
        while !self.stack.is_link_up() {
            Timer::after_millis(500).await;
        }
        info!("Link is up!");

        info!("waiting for stack to be up...");
        self.stack.wait_config_up().await;
        info!("Stack is up!");
    }

    pub async fn run(&mut self) {
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

    pub fn set_config(&mut self, config: Configuration) {
        self.config = Some(config);
    }

    pub fn get_config(&self) -> Option<&Configuration> {
        self.config.as_ref()
    }

    pub fn set_temperature(&mut self, value: f32) {
        self.current_temperature = value;
    }

    pub fn get_temperature(&self) -> f32 {
        self.current_temperature
    }

    pub async fn set_led(&mut self, value: bool) {
        self.current_led_state = value;
        self.control.gpio_set(0, value).await;
    }

    pub fn get_state(&self) -> DeviceState {
        DeviceState {
            temperature: self.current_temperature,
            led_state: self.current_led_state,
        }
    }

    pub fn get_state_json(&self) -> Result<String<64>, &'static str> {
        serde_json_core::to_string::<DeviceState, 64>(&self.get_state())
            .map_err(|_| "Failed to serialize state")
    }

    pub async fn execute_command(
        &mut self,
        command: PicoCommand,
    ) -> Result<String<64>, &'static str> {
        return match command {
            PicoCommand::Led(value) => {
                if self.config.is_none() {
                    return Err("Config not set");
                }

                self.set_led(value).await;
                let mut message_string = String::new();
                message_string
                    .push_str("LED set")
                    .map_err(|_| "Failed to set LED")?;
                Ok(message_string)
            }
            PicoCommand::SetConfig(config) => {
                self.set_config(config);
                let mut message_string = String::new();
                message_string
                    .push_str("Config set")
                    .map_err(|_| "Failed to set config")?;
                Ok(message_string)
            }
            PicoCommand::PrintState => {
                if self.config.is_none() {
                    return Err("Config not set");
                }

                let current_state = self.get_state();
                let state_string = serde_json_core::to_string::<DeviceState, 64>(&current_state)
                    .map_err(|_| "Failed to serialize state")?;
                Ok(state_string)
            }
        };
    }
}

pub fn parse_command(input: String<256>) -> Result<PicoCommand, &'static str> {
    let mut iter = input.split_whitespace();
    let command = iter.next().ok_or(()).map_err(|_| "No command found")?;
    match command {
        "LED" => {
            let value = iter
                .next()
                .ok_or(())
                .map_err(|_| "LED command requires a parameter of ON or OFF")?;
            match value {
                "ON" => return Ok(PicoCommand::Led(true)),
                "OFF" => return Ok(PicoCommand::Led(false)),
                _ => return Err("Invalid value for LED"),
            }
        }
        "CONFIG" => {
            let config_str = iter
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
            let param = iter.next().is_some();
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
