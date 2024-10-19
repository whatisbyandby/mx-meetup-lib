#![no_std]

use cyw43::Control;
use heapless::String;
use serde::{Serialize, Deserialize};

use defmt::info;

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
	let (data, _remainder) = serde_json_core::from_str::<Configuration>(&input).map_err(|_| "Unable to parse Json")?;
	Ok(data)
}


#[derive(Debug, PartialEq)]
pub enum PicoCommand {
	Led(bool),
	SetConfig(Configuration),
	PrintState,
}


pub struct DemoDevice<'a> {
	config: Option<Configuration>,
	current_temperature: f32,
	current_led_state: bool,
	wifi_control: Control<'a>,

}

impl <'a>DemoDevice<'a> {
	pub fn new(wifi_control: Control<'a>) -> Self {
		Self {
			config: None,
			current_temperature: 0.0,
			current_led_state: false,
			wifi_control,
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
		self.wifi_control.gpio_set(0, value).await;
	}

	pub fn get_state(&self) -> DeviceState {
		DeviceState {
			temperature: self.current_temperature,
			led_state: self.current_led_state,
		}
	}

	pub fn get_state_json(&self) -> Result<String<64>, &'static str> {
		serde_json_core::to_string::<DeviceState, 64>(&self.get_state()).map_err(|_| "Failed to serialize state")
	}

	pub async fn execute_command(&mut self, command: PicoCommand) -> Result<String<64>, &'static str> {
		return match command {
			PicoCommand::Led(value) => {

				if self.config.is_none() {
					return Err("Config not set");
				}

				self.set_led(value).await;
				let mut message_string = String::new();
				message_string.push_str("LED set").map_err(|_| "Failed to set LED")?;
				Ok(message_string)
			},
			PicoCommand::SetConfig(config) => {
				self.set_config(config);
				let mut message_string = String::new();
				message_string.push_str("Config set").map_err(|_| "Failed to set config")?;
				Ok(message_string)
			},
			PicoCommand::PrintState => {

				if self.config.is_none() {
					return Err("Config not set");
				}

				let current_state = self.get_state();
				let state_string = serde_json_core::to_string::<DeviceState, 64>(&current_state).map_err(|_| "Failed to serialize state")?;
				Ok(state_string)
			}
		}
	}
}




pub fn parse_command(input: String<256>) -> Result<PicoCommand, &'static str> {

	let mut iter = input.split_whitespace();
	let command = iter.next().ok_or(()).map_err(|_| "No command found")?;
	match command {
		"LED" => {
			let value = iter.next().ok_or(()).map_err(|_| "LED command requires a parameter of ON or OFF")?;
			match value {
				"ON" => return Ok(PicoCommand::Led(true)),
				"OFF" => return Ok(PicoCommand::Led(false)),
				_ => return Err("Invalid value for LED"),
			}
		},
		"CONFIG" => {
			let config_str = iter.next().ok_or(()).map_err(|_| "CONFIG requires a Json parameter")?;
			let mut config_string = String::new();
			config_string.push_str(config_str).map_err(|_| "Failed to parse JSON")?;
			let config = deserialize_config(&config_string).map_err(|msg| msg)?;
			return Ok(PicoCommand::SetConfig(config));
		}
		"PRINT" => {
			let param = iter.next().is_some();
			if param {
				return Err("PRINT command does not take any parameters");
			}
			return Ok(PicoCommand::PrintState);
		},
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
